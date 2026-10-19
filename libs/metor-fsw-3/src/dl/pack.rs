//! Loads packs that are built as shared libraries into this process.

use core::ffi::c_void;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use libloading::Library;
use metor_proto::types::Timestamp;

use crate::coordinator::{EndReason, MakeError, Step, StepCx};
use crate::def::{DefCx, DefError};
use crate::pack::Status;
use crate::pack::def::{DefCxOwned, Instance, PackDef};
use crate::pack::raw::{RawRing, RawSlice};
use crate::system::SystemDef;

use super::codec::DESCRIPTOR_CAPACITY;
use super::{InstanceRings, PackError};

type VersionFn = unsafe extern "C" fn() -> u32;
type PackDefFn = unsafe extern "C" fn(*mut u8, usize, *mut usize) -> u32;
type DefFn = unsafe extern "C" fn(RawSlice, RawSlice, *mut u8, usize, *mut usize) -> u32;
type CreateFn = unsafe extern "C" fn(
    RawSlice,
    RawSlice,
    RawSlice,
    RawSlice,
    RawSlice,
    *mut RawRing,
    u64,
    *mut *mut c_void,
    *mut u8,
    usize,
    *mut usize,
) -> u32;
type ExecuteFn = unsafe extern "C" fn(*mut c_void, i64) -> u32;
type DestroyFn = unsafe extern "C" fn(*mut c_void);

// Libraries are never unloaded, because guest threads and TLS destructors may
// still run their code.
static LIBRARIES: Mutex<BTreeMap<PathBuf, Arc<Library>>> = Mutex::new(BTreeMap::new());

fn library_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn is_loaded(path: &Path) -> bool {
    let key = library_key(path);
    LIBRARIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
}

unsafe fn resident_library(path: &Path) -> Result<Arc<Library>, PackError> {
    let mut libraries = LIBRARIES.lock().unwrap_or_else(|e| e.into_inner());
    match libraries.entry(library_key(path)) {
        Entry::Occupied(entry) => Ok(entry.get().clone()),
        Entry::Vacant(entry) => {
            // SAFETY: DlPack::open's caller authorizes library initialization.
            let lib = unsafe { Library::new(path) }.map_err(PackError::Open)?;
            Ok(entry.insert(Arc::new(lib)).clone())
        }
    }
}

unsafe fn descriptor(def_fn: PackDefFn) -> Result<PackDef, PackError> {
    // SAFETY: the matched ABI writes only inside this buffer and result slot.
    let (status, bytes) = call_export(|dst, cap, written| unsafe { def_fn(dst, cap, written) });
    PackDef::decode(status, bytes.as_deref())
}

/// Calls an export that writes into a host buffer.
///
/// Returns the export's status and the bytes it wrote. The bytes are `None` if
/// the export reported writing more than the buffer holds.
fn call_export(export: impl FnOnce(*mut u8, usize, *mut usize) -> u32) -> (u32, Option<Vec<u8>>) {
    let mut bytes = vec![0u8; DESCRIPTOR_CAPACITY];
    let mut written = 0;
    let status = export(bytes.as_mut_ptr(), bytes.len(), &mut written);
    let fits = written <= bytes.len();
    bytes.truncate(written);
    (status, fits.then_some(bytes))
}

/// The pack's exported functions, resolved after the version check.
///
/// The pointers are valid as long as the library stays loaded.
#[derive(Clone, Copy)]
struct PackFns {
    def: DefFn,
    create: CreateFn,
    execute: ExecuteFn,
    destroy: DestroyFn,
}

/// A pack loaded from a shared library.
pub struct DlPack {
    lib: Arc<Library>,
    fns: PackFns,
    def: PackDef,
}

impl DlPack {
    /// Loads the pack at `path`.
    ///
    /// The ABI version is checked before any other export is called.
    ///
    /// # Safety
    /// `path` must be a metor-fsw-3 pack built against this ABI. Loading a
    /// library runs its initializers. The file must not be removed or replaced
    /// while this process runs, including during this call.
    pub unsafe fn open(path: &Path) -> Result<DlPack, PackError> {
        // SAFETY: the caller's contract.
        let lib = unsafe { resident_library(path) }?;
        // SAFETY: the version export takes no arguments and returns a `u32` in
        // every ABI version. That is why it is resolved and called first.
        let found = unsafe { symbol::<VersionFn>(&lib, "metor_fsw_abi_version")?() };
        PackError::check_abi(found)?;
        let fns = PackFns {
            def: symbol(&lib, "metor_fsw_def")?,
            create: symbol(&lib, "metor_fsw_create")?,
            execute: symbol(&lib, "metor_fsw_execute")?,
            destroy: symbol(&lib, "metor_fsw_destroy")?,
        };
        let def_fn: PackDefFn = symbol(&lib, "metor_fsw_pack_def")?;
        // SAFETY: the descriptor function belongs to the checked ABI.
        let def = unsafe { descriptor(def_fn) }?;
        Ok(DlPack { lib, fns, def })
    }

    /// Returns the definition of type `ty` for one instance's config.
    pub fn def(&self, ty: &str, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        let cx = DefCxOwned::encode(cx)?;
        // SAFETY: the matched ABI writes only inside this buffer and result slot,
        // and reads the two arrays for the length of the call.
        let (status, bytes) = call_export(|dst, cap, written| unsafe {
            (self.fns.def)(
                RawSlice::of(ty.as_bytes()),
                RawSlice::of(&cx),
                dst,
                cap,
                written,
            )
        });
        SystemDef::decode(status, bytes.as_deref())
    }

    /// Returns the descriptor this pack exported.
    pub fn descriptor(&self) -> &PackDef {
        &self.def
    }

    /// Returns the loaded library.
    ///
    /// Every instance created from this pack keeps the library loaded.
    pub fn library(&self) -> &Arc<Library> {
        &self.lib
    }

    /// Creates one instance of type `ty` on the given rings.
    ///
    /// `instance` has the definition the host resolved, including any ports the
    /// config added, and the instance's adapter. `table` is the system's IO
    /// region in, and the region the pack uses out; see `pack::create`.
    pub fn create(
        &self,
        ty: &str,
        params: &serde_json::Value,
        instance: &Instance,
        InstanceRings { inputs, outputs }: InstanceRings<'_>,
        table: &mut RawRing,
    ) -> Result<DlStep, MakeError> {
        let (params, def) = instance.encode(params)?;
        let input_owners: Vec<Vec<_>> = inputs
            .iter()
            .map(|rings| rings.iter().map(|ring| ring.export()).collect())
            .collect();
        let output_owners: Vec<_> = outputs.iter().map(|ring| ring.export()).collect();
        let edges: Vec<Vec<RawRing>> = input_owners
            .iter()
            .map(|rings| rings.iter().map(RawRing::of).collect())
            .collect();
        let ports: Vec<RawSlice> = edges.iter().map(|rings| RawSlice::of(rings)).collect();
        let outs: Vec<RawRing> = output_owners.iter().map(RawRing::of).collect();
        let seed = instance.seed;
        let mut instance = core::ptr::null_mut();
        // SAFETY: the arrays and the exported ring handles outlive the call,
        // and the export writes only inside this buffer and the result slots.
        // The pack keeps its own references to the ring memory through host
        // callbacks.
        let (status, bytes) = call_export(|dst, cap, written| unsafe {
            (self.fns.create)(
                RawSlice::of(ty.as_bytes()),
                RawSlice::of(&params),
                RawSlice::of(&def),
                RawSlice::of(&ports),
                RawSlice::of(&outs),
                table,
                seed,
                &raw mut instance,
                dst,
                cap,
                written,
            )
        });
        MakeError::check_create(status, bytes.as_deref())?;
        Ok(DlStep {
            instance,
            fns: self.fns,
            _lib: self.lib.clone(),
            end: None,
        })
    }
}

/// Looks up one exported symbol.
///
/// If the symbol is missing, the error includes its name.
fn symbol<T: Copy>(lib: &Library, name: &'static str) -> Result<T, PackError> {
    // SAFETY: the name is one of the exports the ABI defines, and `T` is the
    // signature this ABI version gives it.
    let symbol = unsafe { lib.get::<T>(name.as_bytes()) };
    Ok(*symbol.map_err(|_| PackError::MissingSymbol(name))?)
}

/// A system instance running inside a pack library.
pub struct DlStep {
    instance: *mut c_void,
    fns: PackFns,
    /// Keeps the library loaded for as long as the instance lives.
    _lib: Arc<Library>,
    end: Option<EndReason>,
}

impl Step for DlStep {
    fn execute(&mut self, now: Timestamp, _cx: &mut StepCx<'_>) {
        if self.end.is_some() {
            return;
        }
        // SAFETY: the instance came from `create` on this thread and has not
        // been destroyed.
        let status = Status::from_raw(unsafe { (self.fns.execute)(self.instance, now.0) });
        self.end = status.end();
    }

    /// Does nothing, because the pack has already logged the fault.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    fn ended(&self) -> Option<&EndReason> {
        self.end.as_ref()
    }
}

impl Drop for DlStep {
    fn drop(&mut self) {
        // SAFETY: the instance came from `create` and is destroyed once. `_lib`
        // drops after this, so the code running here is still mapped.
        unsafe { (self.fns.destroy)(self.instance) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::DefStatus;

    unsafe extern "C" fn oversized_length(_: *mut u8, capacity: usize, written: *mut usize) -> u32 {
        // SAFETY: descriptor supplies a writable result slot.
        unsafe { written.write(capacity + 1) };
        DefStatus::Ok as u32
    }

    #[test]
    fn test_oversized_descriptor_is_refused() {
        // SAFETY: the callback writes only the supplied result slot.
        let decoded = unsafe { descriptor(oversized_length) };
        assert!(matches!(decoded, Err(PackError::Decode)));
    }

    #[test]
    fn test_matching_abi_is_accepted() {
        assert!(PackError::check_abi(crate::pack::ABI_VERSION).is_ok());
    }

    #[test]
    fn test_other_abi_reports_both_versions() {
        let found = crate::pack::ABI_VERSION + 1;
        let refused = PackError::check_abi(found);
        assert!(
            matches!(
                refused,
                Err(PackError::AbiMismatch { found: f, expected })
                    if f == found && expected == crate::pack::ABI_VERSION
            ),
            "{refused:?}"
        );
    }
}
