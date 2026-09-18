//! Dynamic library pack loading

use core::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use libloading::Library;
use metor_proto::types::Timestamp;

use crate::coordinator::{Make, ParamError, Step, SystemTable, TableEntry};
use crate::pack::def::{PackDef, PackSystemDef};
use crate::pack::raw::{RawPort, RawRing, RawSlice};
use crate::pack::{ABI_VERSION, DefStatus, Status};

type VersionFn = unsafe extern "C" fn() -> u32;
type DefFn = unsafe extern "C" fn(*mut u8, usize, *mut usize) -> u32;
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

/// A `PackError` is why a library is no usable pack.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("loading the pack: {0}")]
    Open(libloading::Error),
    #[error("the pack exports no `{0}`")]
    MissingSymbol(&'static str),
    #[error("the pack is built against ABI {found}, this host speaks {expected}")]
    AbiMismatch { found: u32, expected: u32 },
    #[error("the pack's descriptor did not decode")]
    Decode,
    #[error("pack descriptor exceeds {capacity} bytes")]
    DescriptorTooLarge { capacity: usize },
    #[error("pack descriptor export failed with status {0}")]
    DescriptorStatus(u32),
}

const DESCRIPTOR_CAPACITY: usize = 1024 * 1024;

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
    // SAFETY: Pack::open's caller authorizes library initialization.
    let lib = Arc::new(unsafe { Library::new(path) }.map_err(PackError::Open)?);
    let mut libraries = LIBRARIES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, existing)) = libraries.iter().find(|(path, _)| *path == key) {
        return Ok(existing.clone());
    }
    libraries.push((key, lib.clone()));
    Ok(lib)
}

unsafe fn descriptor(def_fn: DefFn) -> Result<PackDef, PackError> {
    let mut bytes = vec![0u8; DESCRIPTOR_CAPACITY];
    let mut written = 0;
    // SAFETY: the matched ABI writes only inside this buffer and result slot.
    let status = unsafe { def_fn(bytes.as_mut_ptr(), bytes.len(), &mut written) };
    if status == DefStatus::TooSmall as u32 {
        return Err(PackError::DescriptorTooLarge {
            capacity: bytes.len(),
        });
    }
    if status != DefStatus::Ok as u32 {
        return Err(PackError::DescriptorStatus(status));
    }
    let bytes = bytes.get(..written).ok_or(PackError::Decode)?;
    serde_json::from_slice(bytes).map_err(|_| PackError::Decode)
}

/// The exports after the version check, as bare pointers valid while the
/// library is loaded.
#[derive(Clone, Copy)]
pub struct PackFns {
    create: CreateFn,
    execute: ExecuteFn,
    destroy: DestroyFn,
}

/// A `Pack` is a loaded pack library and the descriptor it exported.
pub struct Pack {
    lib: Arc<Library>,
    fns: PackFns,
    def: PackDef,
}

impl Pack {
    /// Loads a pack, checking its ABI version before anything else.
    ///
    /// # Safety
    /// `path` names a metor-fsw-3 pack built against this ABI; loading a
    /// library runs the code in its initializers. The artifact must not be
    /// removed or replaced while this process runs, including during this call.
    pub unsafe fn open(path: &Path) -> Result<Pack, PackError> {
        // SAFETY: the caller's contract.
        unsafe { Self::open_with(path, ABI_VERSION) }
    }

    /// As [`open`](Pack::open), against an ABI version the caller chooses.
    /// Only a test passes anything but [`ABI_VERSION`].
    ///
    /// # Safety
    /// As [`open`](Pack::open).
    #[doc(hidden)]
    pub unsafe fn open_with(path: &Path, expected: u32) -> Result<Pack, PackError> {
        // SAFETY: the caller's contract.
        let lib = unsafe { resident_library(path) }?;
        // SAFETY: the version export takes nothing and returns a `u32` in
        // every ABI, which is why it is resolved and called first.
        let found = unsafe { symbol::<VersionFn>(&lib, "metor_fsw_abi_version")?() };
        if found != expected {
            return Err(PackError::AbiMismatch { found, expected });
        }
        let fns = PackFns {
            create: symbol(&lib, "metor_fsw_create")?,
            execute: symbol(&lib, "metor_fsw_execute")?,
            destroy: symbol(&lib, "metor_fsw_destroy")?,
        };
        let def_fn: DefFn = symbol(&lib, "metor_fsw_pack_def")?;
        // SAFETY: the descriptor function belongs to the checked ABI.
        let def = unsafe { descriptor(def_fn) }?;
        Ok(Pack { lib, fns, def })
    }

    /// The systems this pack exports, in the order it registered them.
    pub fn systems(&self) -> impl Iterator<Item = &PackSystemDef> {
        self.def.systems.iter()
    }

    /// The descriptor this pack exported.
    pub fn def(&self) -> &PackDef {
        &self.def
    }

    /// The loaded library, shared with every instance created from it.
    pub fn library(&self) -> &Arc<Library> {
        &self.lib
    }
}

/// Resolves one export, naming it when it is missing.
fn symbol<T: Copy>(lib: &Library, name: &'static str) -> Result<T, PackError> {
    // SAFETY: the name is one of the five the ABI fixes, and `T` is the
    // signature that version guarantees for it.
    let symbol = unsafe { lib.get::<T>(name.as_bytes()) };
    Ok(*symbol.map_err(|_| PackError::MissingSymbol(name))?)
}

impl SystemTable {
    /// Registers every system in `pack` under `"{id}.{ty}"`.
    pub fn register_pack(&mut self, id: &str, pack: &Pack) {
        for system in pack.systems() {
            let (lib, fns) = (pack.lib.clone(), pack.fns);
            let ty = system.ty.to_string();
            let entry = TableEntry {
                def: system.def.clone(),
                doc: system.doc.clone(),
                schema: system.params.clone(),
                make: Make::Cyclic(Box::new(move |params, def, inputs, outputs| {
                    let params = serde_json::to_vec(params.0)
                        .map_err(|e| ParamError::Decode(e.to_string()))?;
                    // The instance def, so the pack binds the ports the host
                    // resolved, including any the config added.
                    let def =
                        serde_json::to_vec(def).map_err(|e| ParamError::Decode(e.to_string()))?;
                    let input_owners: Vec<Vec<_>> = inputs
                        .iter()
                        .map(|rings| rings.iter().map(|ring| ring.export()).collect())
                        .collect();
                    let output_owners: Vec<_> = outputs.iter().map(|ring| ring.export()).collect();
                    let edges: Vec<Vec<RawRing>> = input_owners
                        .iter()
                        .map(|rings| rings.iter().map(RawRing::of).collect())
                        .collect();
                    let ports: Vec<RawPort> = edges.iter().map(|rings| port(rings)).collect();
                    let outs: Vec<RawRing> = output_owners.iter().map(RawRing::of).collect();
                    let mut error = RawSlice::EMPTY;
                    // SAFETY: arrays and export owners outlive the call. Guest
                    // attachments retain their backing through host callbacks.
                    let instance = unsafe {
                        (fns.create)(
                            RawSlice::of(ty.as_bytes()),
                            RawSlice::of(&params),
                            RawSlice::of(&def),
                            RawSlice::of(&ports),
                            RawSlice::of(&outs),
                            &raw mut error,
                        )
                    };
                    if instance.is_null() {
                        return Err(create_error(&error));
                    }
                    Ok(Box::new(DlStep {
                        instance,
                        fns,
                        lib: lib.clone(),
                        latched: false,
                    }))
                })),
            };
            self.insert(&format!("{id}.{}", system.ty), entry);
        }
    }
}

/// One input port over the rings of its producers.
fn port(rings: &[RawRing]) -> RawPort {
    RawPort {
        rings: rings.as_ptr(),
        len: rings.len(),
    }
}

/// Decodes the error a failed `create` wrote, or names the failure it could not.
fn create_error(error: &RawSlice) -> ParamError {
    // SAFETY: the pack owns the buffer until the next `create` on this thread.
    let bytes = unsafe { error.as_bytes() };
    if bytes.is_empty() {
        return ParamError::Decode("pack create failed".to_string());
    }
    serde_json::from_slice(bytes).unwrap_or_else(|e| ParamError::Decode(e.to_string()))
}

/// A `DlStep` is one instance living inside a pack library.
pub struct DlStep {
    instance: *mut c_void,
    fns: PackFns,
    /// Keeps the library loaded for as long as the instance lives.
    lib: Arc<Library>,
    latched: bool,
}

impl Step for DlStep {
    fn execute(&mut self, now: Timestamp) {
        if self.latched {
            return;
        }
        // SAFETY: the instance came from `create` on this thread and has not
        // been destroyed.
        let status = Status::from_raw(unsafe { (self.fns.execute)(self.instance, now.0) });
        self.latched |= status == Status::Panicked;
    }

    /// Nothing: the pack has already written the fault line.
    fn fault(&mut self, _now: Timestamp, _message: &str) {}

    fn latched(&self) -> bool {
        self.latched
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
    fn reads_a_guest_descriptor_into_owned_storage() {
        // SAFETY: this callback implements the descriptor ABI.
        let def = unsafe { descriptor(empty_table) }.unwrap();
        assert!(def.systems.is_empty());
    }

    #[test]
    fn rejects_failed_descriptor_exports() {
        // SAFETY: these callbacks touch no storage and return failure statuses.
        assert!(matches!(
            unsafe { descriptor(status::<1>) },
            Err(PackError::DescriptorTooLarge {
                capacity: DESCRIPTOR_CAPACITY
            })
        ));
        for (callback, expected) in [
            (status::<2> as DefFn, 2),
            (status::<3>, 3),
            (status::<99>, 99),
        ] {
            // SAFETY: as above.
            assert!(matches!(unsafe { descriptor(callback) },
                Err(PackError::DescriptorStatus(found)) if found == expected));
        }
    }

    #[test]
    fn rejects_invalid_lengths_and_json() {
        for callback in [oversized_length as DefFn, status::<0>] {
            // SAFETY: callbacks write at most the supplied result slot.
            assert!(matches!(
                unsafe { descriptor(callback) },
                Err(PackError::Decode)
            ));
        }
    }
}
