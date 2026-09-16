//! The host side of a pack: load a library, read its descriptor, and register
//! every system in it into an ordinary [`SystemTable`].
//!
//! A registered entry's `make` calls the pack's `create` with the regions of
//! the rings it was handed, so the pack claims the reader slots and the writer
//! itself. The returned handle is a [`DlStep`], which keeps the library loaded
//! until the instance is destroyed.

use core::ffi::c_void;
use std::borrow::Cow;
use std::path::Path;
use std::rc::Rc;

use libloading::Library;
use metor_proto::types::Timestamp;

use crate::coordinator::{ParamError, Step, SystemTable, TableEntry};
use crate::pack::def::{PackDef, PackSystemDef};
use crate::pack::raw::{RawPort, RawRing, RawSlice};
use crate::pack::{ABI_VERSION, Status};

type VersionFn = unsafe extern "C" fn() -> u32;
type DefFn = unsafe extern "C" fn() -> RawSlice;
type CreateFn =
    unsafe extern "C" fn(RawSlice, RawSlice, RawSlice, RawSlice, *mut RawSlice) -> *mut c_void;
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
    lib: Rc<Library>,
    fns: PackFns,
    def: PackDef<'static>,
}

impl Pack {
    /// Loads a pack, checking its ABI version before anything else.
    ///
    /// # Safety
    /// `path` names a metor-fsw-3 pack built against this ABI; loading a
    /// library runs the code in its initializers.
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
        let lib = unsafe { Library::new(path) }.map_err(PackError::Open)?;
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
        // SAFETY: the version matched, so the descriptor is JSON in a slice the
        // library owns for as long as it is loaded.
        let bytes = unsafe { def_fn().as_bytes() }.to_vec();
        let def = serde_json::from_slice(Box::leak(bytes.into_boxed_slice()))
            .map_err(|_| PackError::Decode)?;
        Ok(Pack {
            lib: Rc::new(lib),
            fns,
            def,
        })
    }

    /// The systems this pack exports, in the order it registered them.
    pub fn systems(&self) -> impl Iterator<Item = &PackSystemDef<'static>> {
        self.def.systems.iter()
    }

    /// The descriptor this pack exported.
    pub fn def(&self) -> &PackDef<'static> {
        &self.def
    }

    /// The loaded library, shared with every instance created from it.
    pub fn library(&self) -> &Rc<Library> {
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
                schema: system.params.clone().map(Cow::into_owned),
                make: Box::new(move |params, inputs, outputs| {
                    let params = serde_json::to_vec(params.0)
                        .map_err(|e| ParamError::Decode(e.to_string()))?;
                    let edges: Vec<Vec<RawRing>> = inputs
                        .iter()
                        .map(|rings| rings.iter().map(raw).collect())
                        .collect();
                    let ports: Vec<RawPort> = edges.iter().map(|rings| port(rings)).collect();
                    let outs: Vec<RawRing> = outputs.iter().map(raw).collect();
                    let mut error = RawSlice::EMPTY;
                    // SAFETY: every array outlives the call, and the rings
                    // outlive the instance because the coordinator drops its
                    // entries before its rings.
                    let instance = unsafe {
                        (fns.create)(
                            RawSlice::of(ty.as_bytes()),
                            RawSlice::of(&params),
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
                }),
            };
            self.insert(&format!("{id}.{}", system.ty), entry);
        }
    }
}

/// The region one ring sits in.
fn raw(ring: &&metor_fsw_3_ring::RingBuffer) -> RawRing {
    let (base, len) = ring.region();
    RawRing { base, len }
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
    lib: Rc<Library>,
    latched: bool,
}

impl Step for DlStep {
    fn execute(&mut self, now: Timestamp) {
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
