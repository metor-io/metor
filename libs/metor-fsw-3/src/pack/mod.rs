pub mod def;
pub mod raw;

use core::cell::OnceCell;
use core::ffi::c_void;
use std::cell::RefCell;

use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;
use tracing_subscriber::layer::SubscriberExt;

use crate::coordinator::{ParamError, Params, Step, SystemTable, catch_step};
use def::PackDef;
use raw::{RawPort, RawRing, RawSlice};

/// The ABI the exports in this module are built against.
pub const ABI_VERSION: u32 = 3;

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

/// Descriptor export status words.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefStatus {
    Ok = 0,
    TooSmall = 1,
    Encode = 2,
    Panicked = 3,
}

thread_local! {
    static TABLE: OnceCell<SystemTable> = const { OnceCell::new() };
    static ERROR: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn with_table<R>(build: fn() -> SystemTable, f: impl FnOnce(&SystemTable) -> R) -> R {
    TABLE.with(|cell| {
        f(cell.get_or_init(|| {
            let subscriber = tracing_subscriber::registry().with(crate::log::layer());
            let _ = tracing::subscriber::set_global_default(subscriber);
            build()
        }))
    })
}

/// Writes the descriptor into caller-owned storage. Failures leave `written` zero.
///
/// # Safety
/// `dst` points to `capacity` writable bytes (or is null when capacity is zero).
/// `written` is writable and does not overlap `dst`. Neither pointer is retained.
pub unsafe fn def(
    build: fn() -> SystemTable,
    dst: *mut u8,
    capacity: usize,
    written: *mut usize,
) -> u32 {
    // SAFETY: the caller supplies a writable result pointer.
    unsafe { written.write(0) };
    crate::panic::catch(|| {
        let buffer = if capacity == 0 {
            &mut []
        } else {
            // SAFETY: the caller supplies this writable, nonoverlapping region.
            unsafe { core::slice::from_raw_parts_mut(dst, capacity) }
        };
        let mut out = std::io::Cursor::new(buffer);
        let result = with_table(build, |table| {
            serde_json::to_writer(&mut out, &PackDef::from_table(table))
        });
        match result {
            Ok(()) => {
                // SAFETY: the cursor wrote at most `capacity` bytes.
                unsafe { written.write(out.position() as usize) };
                DefStatus::Ok
            }
            Err(error) if error.is_io() => DefStatus::TooSmall,
            Err(_) => DefStatus::Encode,
        }
    })
    .unwrap_or(DefStatus::Panicked) as u32
}

/// Builds one system instance and binds it to the rings it was handed.
///
/// Returns null on a failure, writing the [`ParamError`] as JSON through
/// `error` when there is one and an empty slice when the failure was a panic.
///
/// # Safety
/// `ty`, `params`, `def`, `inputs` (over [`RawPort`]) and `outputs` (over
/// [`RawRing`]) each meet [`RawSlice::as_slice`]'s contract and every ring has
/// live, valid ownership callbacks covering its region. `error` is writable and
/// does not alias the input arrays. Calls on this thread must not reenter `create`.
pub unsafe fn create(
    build: fn() -> SystemTable,
    ty: RawSlice,
    params: RawSlice,
    def: RawSlice,
    inputs: RawSlice,
    outputs: RawSlice,
    error: *mut RawSlice,
) -> *mut c_void {
    // SAFETY: the caller supplies a writable result pointer.
    unsafe { error.write(RawSlice::EMPTY) };
    crate::panic::catch(|| {
        // SAFETY: the caller's arrays and ownership callbacks remain valid here.
        match unsafe { make(build, ty, params, def, inputs, outputs) } {
            Ok(step) => Box::into_raw(Box::new(Some(step))).cast(),
            Err(source) => {
                // SAFETY: `error` remains writable for this call.
                unsafe { error.write(store_error(&source)) };
                core::ptr::null_mut()
            }
        }
    })
    .unwrap_or(core::ptr::null_mut())
}

/// Looks `ty` up, attaches every ring, and binds the entry to them.
///
/// # Safety
/// As [`create`].
unsafe fn make(
    build: fn() -> SystemTable,
    ty: RawSlice,
    params: RawSlice,
    def: RawSlice,
    inputs: RawSlice,
    outputs: RawSlice,
) -> Result<Box<dyn Step>, ParamError> {
    // SAFETY: the caller's contract.
    let (ty, params, def) = unsafe { (ty.as_bytes(), params.as_bytes(), def.as_bytes()) };
    let ty = core::str::from_utf8(ty).map_err(|e| ParamError::Decode(e.to_string()))?;

    let value = decode_params(params)?;
    let def: crate::SystemDef =
        serde_json::from_slice(def).map_err(|e| ParamError::Decode(e.to_string()))?;
    // SAFETY: the caller's contract.
    let (ports, out_rings) = unsafe { (inputs.as_slice::<RawPort>(), outputs.as_slice()) };
    let ins: Vec<Vec<RingBuffer>> = ports
        .iter()
        // SAFETY: the caller's contract.
        .map(|port| unsafe { port.rings() }.iter().map(attach).collect())
        .collect::<Result<_, _>>()?;
    let outs: Vec<RingBuffer> = out_rings.iter().map(attach).collect::<Result<_, _>>()?;

    let views = ins.iter().map(|rings| rings.iter().collect()).collect();
    let writers = outs.iter().collect();
    with_table(build, |table| {
        let entry = table
            .get(ty)
            .ok_or_else(|| ParamError::Decode(format!("unknown system type `{ty}`")))?;
        (entry.make)(Params(&value), &def, views, writers)
    })
}

/// Reads the params JSON, taking no bytes as no params.
fn decode_params(bytes: &[u8]) -> Result<serde_json::Value, ParamError> {
    if bytes.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(bytes).map_err(|e| ParamError::Decode(e.to_string()))
}

/// Attaches to one ring region the host allocated.
fn attach(ring: &RawRing) -> Result<RingBuffer, ParamError> {
    // SAFETY: `create` requires live, thread-safe ownership callbacks.
    unsafe { RingBuffer::attach_owned(ring.base, ring.len, ring.owner) }
        .map_err(|e| ParamError::Decode(e.to_string()))
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
    crate::panic::catch(|| {
        // SAFETY: `create` allocated this slot, and it is still live on this thread.
        let slot = unsafe { &mut *instance.cast::<Option<Box<dyn Step>>>() };
        if catch_step(slot, Timestamp(now)) {
            Status::Ok
        } else {
            Status::Panicked
        }
    })
    .unwrap_or(Status::Panicked) as u32
}

/// Drops one instance, freeing the reader slots and writer it claimed.
///
/// # Safety
/// `instance` came from [`create`] on this thread and is destroyed once.
pub unsafe fn destroy(instance: *mut c_void) {
    crate::panic::catch(|| {
        // SAFETY: the caller's contract; the box was made by `create`.
        drop(unsafe { Box::from_raw(instance.cast::<Option<Box<dyn Step>>>()) })
    });
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
        pub unsafe extern "C" fn metor_fsw_pack_def(
            dst: *mut u8,
            capacity: usize,
            written: *mut usize,
        ) -> u32 {
            unsafe { $crate::pack::def($build, dst, capacity, written) }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn metor_fsw_create(
            ty: $crate::pack::raw::RawSlice,
            params: $crate::pack::raw::RawSlice,
            def: $crate::pack::raw::RawSlice,
            inputs: $crate::pack::raw::RawSlice,
            outputs: $crate::pack::raw::RawSlice,
            error: *mut $crate::pack::raw::RawSlice,
        ) -> *mut ::core::ffi::c_void {
            unsafe { $crate::pack::create($build, ty, params, def, inputs, outputs, error) }
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
