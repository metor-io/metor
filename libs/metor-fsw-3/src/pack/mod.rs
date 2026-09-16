//! The export side of a pack: the descriptor, the five exports, and the macro
//! that names them.
//!
//! A pack is a `SystemTable` built inside a `cdylib`. [`export_pack!`] emits
//! the five `extern "C"` functions a host resolves; each is one call into this
//! module, and each catches an unwind so no panic crosses the boundary.

pub mod def;
pub mod raw;

use core::cell::OnceCell;
use core::ffi::c_void;
use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;
use tracing_subscriber::layer::SubscriberExt;

use crate::coordinator::{ParamError, Params, Step, SystemTable, catch_step};
use def::PackDef;
use raw::{RawPort, RawRing, RawSlice};

/// The ABI the exports in this module are built against.
pub const ABI_VERSION: u32 = 1;

/// What [`execute`] returns: an unknown word is a panic.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok = 0,
    Panicked = 1,
}

impl Status {
    /// Reads a returned status word.
    pub fn from_raw(word: u32) -> Status {
        match word {
            0 => Status::Ok,
            _ => Status::Panicked,
        }
    }
}

/// A pack's table and the descriptor bytes projected from it.
struct Exported {
    table: SystemTable,
    def: Vec<u8>,
}

thread_local! {
    /// The table this thread built. A `Step` is `!Send`, so a host that drives
    /// a pack from two threads gets two tables, one per thread.
    static EXPORTED: OnceCell<&'static Exported> = const { OnceCell::new() };

    /// The error `create` last wrote, alive until the next `create` here.
    static ERROR: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Builds the table on first use and installs this library's log layer.
fn exported(build: fn() -> SystemTable) -> &'static Exported {
    EXPORTED.with(|cell| {
        *cell.get_or_init(|| {
            let subscriber = tracing_subscriber::registry().with(crate::log::layer());
            // A second install returns the layer already in place.
            let _ = tracing::subscriber::set_global_default(subscriber);
            let table = build();
            let def = PackDef::from_table(&table);
            Box::leak(Box::new(Exported { table, def }))
        })
    })
}

/// The descriptor, owned by the pack for the life of the library.
pub fn def(build: fn() -> SystemTable) -> RawSlice {
    RawSlice::of(&exported(build).def)
}

/// Builds one system instance and binds it to the rings it was handed.
///
/// Returns null on a failure, writing the [`ParamError`] as JSON through
/// `error` when there is one and an empty slice when the failure was a panic.
///
/// # Safety
/// `ty`, `params`, `inputs` (over [`RawPort`]) and `outputs` (over [`RawRing`])
/// each meet [`RawSlice::as_slice`]'s contract, every named ring region
/// outlives the returned instance, and `error` is writable.
pub unsafe fn create(
    build: fn() -> SystemTable,
    ty: RawSlice,
    params: RawSlice,
    inputs: RawSlice,
    outputs: RawSlice,
    error: *mut RawSlice,
) -> *mut c_void {
    // SAFETY: the caller's contract carries into the closure unchanged.
    let made = catch_unwind(AssertUnwindSafe(|| unsafe {
        make(build, ty, params, inputs, outputs)
    }));
    let failed = |slice| {
        // SAFETY: the caller's contract says `error` is writable.
        unsafe { error.write(slice) };
        core::ptr::null_mut()
    };
    match made {
        Ok(Ok(step)) => Box::into_raw(Box::new(step)).cast(),
        Ok(Err(source)) => failed(store_error(&source)),
        Err(_) => failed(RawSlice::EMPTY),
    }
}

/// Looks `ty` up, attaches every ring, and binds the entry to them.
///
/// # Safety
/// As [`create`].
unsafe fn make(
    build: fn() -> SystemTable,
    ty: RawSlice,
    params: RawSlice,
    inputs: RawSlice,
    outputs: RawSlice,
) -> Result<Box<dyn Step>, ParamError> {
    let exported = exported(build);
    // SAFETY: the caller's contract.
    let (ty, params) = unsafe { (ty.as_bytes(), params.as_bytes()) };
    // PANIC Safety: the host only names a type the descriptor listed.
    let ty = core::str::from_utf8(ty).expect("a utf-8 type name");
    let entry = exported.table.get(ty).expect("a type the descriptor named");

    let value = decode_params(params)?;
    // SAFETY: the caller's contract.
    let (ports, out_rings) = unsafe { (inputs.as_slice::<RawPort>(), outputs.as_slice()) };
    let ins: Vec<Vec<RingBuffer>> = ports
        .iter()
        // SAFETY: the caller's contract.
        .map(|port| unsafe { port.rings() }.iter().map(attach).collect())
        .collect();
    let outs: Vec<RingBuffer> = out_rings.iter().map(attach).collect();

    let views = ins.iter().map(|rings| rings.iter().collect()).collect();
    let writers = outs.iter().collect();
    // The views and writers `make` claimed hold the region; these handles do not.
    (entry.make)(Params(&value), views, writers)
}

/// Reads the params JSON, taking no bytes as no params.
fn decode_params(bytes: &[u8]) -> Result<serde_json::Value, ParamError> {
    if bytes.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(bytes).map_err(|e| ParamError::Decode(e.to_string()))
}

/// Attaches to one ring region the host allocated.
fn attach(ring: &RawRing) -> RingBuffer {
    // PANIC Safety: the host hands over regions it formatted itself; a bad one
    // is caught by the export's `catch_unwind`.
    // SAFETY: `create`'s contract says the region outlives the instance.
    unsafe { RingBuffer::attach_raw(ring.base, ring.len) }.expect("a live ring region")
}

/// Serializes `source` into this thread's error buffer.
fn store_error(source: &ParamError) -> RawSlice {
    ERROR.with_borrow_mut(|buf| {
        buf.clear();
        // An error that does not serialize leaves the host with an empty slice.
        let _ = serde_json::to_writer(&mut *buf, source);
        RawSlice::of(buf)
    })
}

/// Runs one cycle of one instance.
///
/// # Safety
/// `instance` came from [`create`] on this thread and has not been destroyed.
pub unsafe fn execute(instance: *mut c_void, now: i64) -> u32 {
    let now = Timestamp(now);
    let ran = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller's contract; the instance is a `Box<dyn Step>`.
        let step = unsafe { &mut *instance.cast::<Box<dyn Step>>() };
        match catch_step(step.as_mut(), now) {
            Ok(()) => Status::Ok,
            Err(message) => {
                step.fault(now, &message);
                Status::Panicked
            }
        }
    }));
    ran.unwrap_or(Status::Panicked) as u32
}

/// Drops one instance, freeing the reader slots and writer it claimed.
///
/// # Safety
/// `instance` came from [`create`] on this thread and is destroyed once.
pub unsafe fn destroy(instance: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the caller's contract; the box was made by `create`.
        drop(unsafe { Box::from_raw(instance.cast::<Box<dyn Step>>()) })
    }));
}

/// Exports `$build`'s table under the five ABI names.
#[macro_export]
macro_rules! export_pack {
    ($build:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn metor_fsw_abi_version() -> u32 {
            $crate::pack::ABI_VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn metor_fsw_pack_def() -> $crate::pack::raw::RawSlice {
            $crate::pack::def($build)
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn metor_fsw_create(
            ty: $crate::pack::raw::RawSlice,
            params: $crate::pack::raw::RawSlice,
            inputs: $crate::pack::raw::RawSlice,
            outputs: $crate::pack::raw::RawSlice,
            error: *mut $crate::pack::raw::RawSlice,
        ) -> *mut ::core::ffi::c_void {
            unsafe { $crate::pack::create($build, ty, params, inputs, outputs, error) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn metor_fsw_execute(
            instance: *mut ::core::ffi::c_void,
            now: i64,
        ) -> u32 {
            unsafe { $crate::pack::execute(instance, now) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn metor_fsw_destroy(instance: *mut ::core::ffi::c_void) {
            unsafe { $crate::pack::destroy(instance) }
        }
    };
}

#[cfg(test)]
mod tests;
