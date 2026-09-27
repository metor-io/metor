//! A pack built as a shared library, loaded into this process.

use core::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use libloading::Library;
use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;

use crate::coordinator::{EndReason, ParamError, Step};
use crate::def::{DefCx, DefError};
use crate::pack::def::{Instance, PackDef};
use crate::pack::raw::{RawRing, RawSlice};
use crate::pack::{ABI_VERSION, Status};
use crate::system::SystemDef;

use super::PackError;
use super::codec::{
    DESCRIPTOR_CAPACITY, decode_create_error, decode_def, decode_descriptor, encode_def_cx,
    encode_instance,
};

type VersionFn = unsafe extern "C" fn() -> u32;
type PackDefFn = unsafe extern "C" fn(*mut u8, usize, *mut usize) -> u32;
type DefFn = unsafe extern "C" fn(RawSlice, RawSlice, *mut u8, usize, *mut usize) -> u32;
type CreateFn = unsafe extern "C" fn(
    RawSlice,
    RawSlice,
    RawSlice,
    RawSlice,
    RawSlice,
    *mut RawSlice,
) -> *mut c_void;
type ExecuteFn = unsafe extern "C" fn(*mut c_void, i64) -> u32;
type DestroyFn = unsafe extern "C" fn(*mut c_void);

// Resident code may still be used by guest threads and TLS destructors.
static LIBRARIES: Mutex<Vec<(PathBuf, Arc<Library>)>> = Mutex::new(Vec::new());

fn library_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn is_loaded(path: &Path) -> bool {
    let key = library_key(path);
    LIBRARIES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(path, _)| *path == key)
}

unsafe fn resident_library(path: &Path) -> Result<Arc<Library>, PackError> {
    let key = library_key(path);
    {
        let libraries = LIBRARIES.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, lib)) = libraries.iter().find(|(path, _)| *path == key) {
            return Ok(lib.clone());
        }
    }
    // SAFETY: DlPack::open's caller authorizes library initialization.
    let lib = Arc::new(unsafe { Library::new(path) }.map_err(PackError::Open)?);
    let mut libraries = LIBRARIES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, existing)) = libraries.iter().find(|(path, _)| *path == key) {
        return Ok(existing.clone());
    }
    libraries.push((key, lib.clone()));
    Ok(lib)
}

unsafe fn descriptor(def_fn: PackDefFn) -> Result<PackDef, PackError> {
    // SAFETY: the matched ABI writes only inside this buffer and result slot.
    let (status, bytes) = call_export(|dst, cap, written| unsafe { def_fn(dst, cap, written) });
    decode_descriptor(status, bytes.as_deref())
}

/// Calls an export that writes into a host buffer, returning its status and
/// the bytes it reports writing, or `None` when that is past the buffer.
fn call_export(export: impl FnOnce(*mut u8, usize, *mut usize) -> u32) -> (u32, Option<Vec<u8>>) {
    let mut bytes = vec![0u8; DESCRIPTOR_CAPACITY];
    let mut written = 0;
    let status = export(bytes.as_mut_ptr(), bytes.len(), &mut written);
    let fits = written <= bytes.len();
    bytes.truncate(written);
    (status, fits.then_some(bytes))
}

/// The exports after the version check, as bare pointers valid while the
/// library is loaded.
#[derive(Clone, Copy)]
struct PackFns {
    def: DefFn,
    create: CreateFn,
    execute: ExecuteFn,
    destroy: DestroyFn,
}

/// A `DlPack` is a loaded pack library and the descriptor it exported.
pub struct DlPack {
    lib: Arc<Library>,
    fns: PackFns,
    def: PackDef,
}

impl DlPack {
    /// Loads a pack, checking its ABI version before anything else.
    ///
    /// # Safety
    /// `path` names a metor-fsw-3 pack built against this ABI; loading a
    /// library runs the code in its initializers. The artifact must not be
    /// removed or replaced while this process runs, including during this call.
    pub unsafe fn open(path: &Path) -> Result<DlPack, PackError> {
        // SAFETY: the caller's contract.
        unsafe { Self::open_with(path, ABI_VERSION) }
    }

    /// As [`open`](DlPack::open), against an ABI version the caller chooses.
    /// Only a test passes anything but [`ABI_VERSION`].
    ///
    /// # Safety
    /// As [`open`](DlPack::open).
    #[doc(hidden)]
    pub unsafe fn open_with(path: &Path, expected: u32) -> Result<DlPack, PackError> {
        // SAFETY: the caller's contract.
        let lib = unsafe { resident_library(path) }?;
        // SAFETY: the version export takes nothing and returns a `u32` in
        // every ABI, which is why it is resolved and called first.
        let found = unsafe { symbol::<VersionFn>(&lib, "metor_fsw_abi_version")?() };
        if found != expected {
            return Err(PackError::AbiMismatch { found, expected });
        }
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

    /// One exported type's definition under an instance's config.
    pub fn def(&self, ty: &str, cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        let cx = encode_def_cx(cx)?;
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
        decode_def(status, bytes.as_deref())
    }

    /// The descriptor this pack exported.
    pub fn descriptor(&self) -> &PackDef {
        &self.def
    }

    /// The loaded library, shared with every instance created from it.
    pub fn library(&self) -> &Arc<Library> {
        &self.lib
    }

    /// Creates one instance of `ty` over the host's rings.
    ///
    /// `instance` carries the definition the host resolved, including any ports
    /// the config added, and the adapter the config named.
    pub fn create(
        &self,
        ty: &str,
        params: &serde_json::Value,
        instance: &Instance,
        inputs: &[Vec<&RingBuffer>],
        outputs: &[&RingBuffer],
    ) -> Result<DlStep, ParamError> {
        let (params, def) = encode_instance(params, instance)?;
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
        let mut error = RawSlice::EMPTY;
        // SAFETY: arrays and export owners outlive the call. Guest attachments
        // retain their backing through host callbacks.
        let instance = unsafe {
            (self.fns.create)(
                RawSlice::of(ty.as_bytes()),
                RawSlice::of(&params),
                RawSlice::of(&def),
                RawSlice::of(&ports),
                RawSlice::of(&outs),
                &raw mut error,
            )
        };
        if instance.is_null() {
            // SAFETY: the pack owns the buffer until the next `create` on this thread.
            return Err(decode_create_error(unsafe { error.as_bytes() }));
        }
        Ok(DlStep {
            instance,
            fns: self.fns,
            lib: self.lib.clone(),
            end: None,
        })
    }
}

/// Resolves one export, naming it when it is missing.
fn symbol<T: Copy>(lib: &Library, name: &'static str) -> Result<T, PackError> {
    // SAFETY: the name is one of the five the ABI fixes, and `T` is the
    // signature that version guarantees for it.
    let symbol = unsafe { lib.get::<T>(name.as_bytes()) };
    Ok(*symbol.map_err(|_| PackError::MissingSymbol(name))?)
}

/// A `DlStep` is one instance living inside a pack library.
pub struct DlStep {
    instance: *mut c_void,
    fns: PackFns,
    /// Keeps the library loaded for as long as the instance lives.
    lib: Arc<Library>,
    end: Option<EndReason>,
}

impl Step for DlStep {
    fn execute(&mut self, now: Timestamp) {
        if self.end.is_some() {
            return;
        }
        // SAFETY: the instance came from `create` on this thread and has not
        // been destroyed.
        let status = Status::from_raw(unsafe { (self.fns.execute)(self.instance, now.0) });
        self.end = status.end();
    }

    /// Nothing: the pack has already written the fault line.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    fn ended(&self) -> Option<&EndReason> {
        self.end.as_ref()
    }
}

impl Drop for DlStep {
    fn drop(&mut self) {
        // SAFETY: the instance came from `create` and is destroyed once. `lib`
        // drops after this, so the code running here is still mapped.
        unsafe { (self.fns.destroy)(self.instance) };
        let _ = &self.lib;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::SystemTable;
    use crate::pack::DefStatus;

    unsafe extern "C" fn status<const STATUS: u32>(_: *mut u8, _: usize, _: *mut usize) -> u32 {
        STATUS
    }

    unsafe extern "C" fn oversized_length(_: *mut u8, capacity: usize, written: *mut usize) -> u32 {
        // SAFETY: descriptor supplies a writable result slot.
        unsafe { written.write(capacity + 1) };
        DefStatus::Ok as u32
    }

    unsafe extern "C" fn empty_table(dst: *mut u8, capacity: usize, written: *mut usize) -> u32 {
        // SAFETY: descriptor supplies valid, nonoverlapping storage.
        unsafe { crate::pack::def(SystemTable::new, dst, capacity, written) }
    }

    #[test]
    fn test_descriptor_through_export() {
        // SAFETY: this callback implements the descriptor ABI.
        let def = unsafe { descriptor(empty_table) }.expect("decodes");
        assert!(def.systems.is_empty());
        for callback in [oversized_length as PackDefFn, status::<0>] {
            // SAFETY: these callbacks write at most the supplied result slot.
            let decoded = unsafe { descriptor(callback) };
            assert!(matches!(decoded, Err(PackError::Decode)));
        }
    }
}
