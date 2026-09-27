pub mod def;
pub mod raw;

use core::alloc::Layout;
use core::cell::OnceCell;
use core::ffi::c_void;
use std::borrow::Cow;
use std::cell::RefCell;

use metor_fsw_3_ring::RingBuffer;
use metor_proto::types::Timestamp;
use tracing_subscriber::layer::SubscriberExt;

use crate::coordinator::{
    EndReason, MakeCx, ParamError, Params, Step, Stepped, SystemTable, catch_step,
};
use crate::def::DefError;
use crate::system::SystemDef;
use crate::thread::Threads;
use def::{DefCxOwned, Instance, PackDef};
use raw::{RawRing, RawSlice};

/// The ABI the exports in this module are built against.
pub const ABI_VERSION: u32 = 9;

/// What [`execute`] returns: an unknown word is a panic.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok = 0,
    Panicked = 1,
    Completed = 2,
    Aborted = 3,
    /// The reason is a fault line on the system's `log`.
    Failed = 4,
}

impl Status {
    /// Reads a returned status word, `None` for a word no status has.
    pub fn try_from_raw(word: u32) -> Option<Status> {
        match word {
            0 => Some(Status::Ok),
            1 => Some(Status::Panicked),
            2 => Some(Status::Completed),
            3 => Some(Status::Aborted),
            4 => Some(Status::Failed),
            _ => None,
        }
    }

    /// Reads a returned status word; an unknown word is a panic.
    pub fn from_raw(word: u32) -> Status {
        Status::try_from_raw(word).unwrap_or(Status::Panicked)
    }

    /// The word a step that ended as `end` returns.
    pub fn of_end(end: Option<&EndReason>) -> Status {
        match end {
            None => Status::Ok,
            Some(EndReason::Completed) => Status::Completed,
            Some(EndReason::Aborted) => Status::Aborted,
            Some(EndReason::Failed(_) | EndReason::Unloaded) => Status::Failed,
            Some(EndReason::Panicked) => Status::Panicked,
        }
    }

    /// The end this word reports, `None` for a step still running.
    pub fn end(self) -> Option<EndReason> {
        match self {
            Status::Ok => None,
            Status::Panicked => Some(EndReason::Panicked),
            Status::Completed => Some(EndReason::Completed),
            Status::Aborted => Some(EndReason::Aborted),
            Status::Failed => Some(EndReason::Failed(Cow::Borrowed("see the system's log"))),
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
    /// The type refused the config; `dst` holds the [`DefError`] as JSON.
    Refused = 4,
}

thread_local! {
    static TABLE: OnceCell<SystemTable> = const { OnceCell::new() };
    /// The groups this pack runs its own async systems on.
    static THREADS: RefCell<Threads> = RefCell::new(Threads::new());
    static ERROR: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

fn with_table<R>(build: fn() -> SystemTable, f: impl FnOnce(&SystemTable) -> R) -> R {
    TABLE.with(|cell| {
        f(cell.get_or_init(|| {
            let subscriber = tracing_subscriber::registry().with(crate::log::LogLayer);
            let _ = tracing::subscriber::set_global_default(subscriber);
            crate::panic::log_panics();
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
        // SAFETY: the caller's contract.
        let mut out = unsafe { out_buf(dst, capacity) };
        with_table(build, |table| {
            write_json(
                &mut out,
                &PackDef::from_table(table),
                written,
                DefStatus::Ok,
            )
        })
    })
    .unwrap_or(DefStatus::Panicked) as u32
}

/// Writes one type's definition for the config in `cx` into caller-owned
/// storage. Failures leave `written` zero, except a refusal, which writes the
/// [`DefError`] as JSON.
///
/// # Safety
/// `ty` and `cx` meet [`RawSlice::as_bytes`]'s contract. `dst` points to
/// `capacity` writable bytes (or is null when capacity is zero). `written` is
/// writable and does not overlap `dst`. Neither pointer is retained.
pub unsafe fn system_def(
    build: fn() -> SystemTable,
    ty: RawSlice,
    cx: RawSlice,
    dst: *mut u8,
    capacity: usize,
    written: *mut usize,
) -> u32 {
    // SAFETY: the caller supplies a writable result pointer.
    unsafe { written.write(0) };
    crate::panic::catch(|| {
        // SAFETY: the caller's contract.
        let (ty, cx) = unsafe { (ty.as_bytes(), cx.as_bytes()) };
        // SAFETY: the caller's contract.
        let mut out = unsafe { out_buf(dst, capacity) };
        match with_table(build, |table| resolve_def(table, ty, cx)) {
            Ok(def) => write_json(&mut out, &def, written, DefStatus::Ok),
            Err(source) => write_json(&mut out, &source, written, DefStatus::Refused),
        }
    })
    .unwrap_or(DefStatus::Panicked) as u32
}

/// One type's definition under a decoded context.
fn resolve_def(table: &SystemTable, ty: &[u8], cx: &[u8]) -> Result<SystemDef, DefError> {
    let refused = |message: String| DefError::Pack { message };
    let ty = core::str::from_utf8(ty).map_err(|e| refused(e.to_string()))?;
    let owned: DefCxOwned = serde_json::from_slice(cx).map_err(|e| refused(e.to_string()))?;
    let entry = table
        .get(ty)
        .ok_or_else(|| refused(format!("unknown system type `{ty}`")))?;
    let edges = owned.edges();
    (entry.def_fn)(&owned.borrow(&edges))
}

/// The caller's output region, as a cursor a writer fills.
///
/// # Safety
/// `dst` points to `capacity` writable bytes no other reference touches for
/// the cursor's life, or `capacity` is zero.
unsafe fn out_buf<'a>(dst: *mut u8, capacity: usize) -> std::io::Cursor<&'a mut [u8]> {
    let buffer = if capacity == 0 {
        &mut []
    } else {
        // SAFETY: the caller's contract.
        unsafe { core::slice::from_raw_parts_mut(dst, capacity) }
    };
    std::io::Cursor::new(buffer)
}

/// Writes one value as JSON, reporting `ok` when it fit.
fn write_json<T: serde::Serialize>(
    out: &mut std::io::Cursor<&mut [u8]>,
    value: &T,
    written: *mut usize,
    ok: DefStatus,
) -> DefStatus {
    match serde_json::to_writer(&mut *out, value) {
        Ok(()) => {
            // SAFETY: the cursor wrote at most `capacity` bytes.
            unsafe { written.write(out.position() as usize) };
            ok
        }
        Err(error) if error.is_io() => DefStatus::TooSmall,
        Err(_) => DefStatus::Encode,
    }
}

/// Builds one system instance and binds it to the rings it was handed.
///
/// Returns null on a failure, writing the [`ParamError`] as JSON through
/// `error` when there is one and an empty slice when the failure was a panic.
///
/// # Safety
/// `ty`, `params`, `def`, `inputs` (over [`RawSlice`]s of [`RawRing`]) and
/// `outputs` (over [`RawRing`]) each meet [`RawSlice::as_slice`]'s contract and every ring has
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
    let instance: Instance =
        serde_json::from_slice(def).map_err(|e| ParamError::Decode(e.to_string()))?;
    // SAFETY: the caller's contract.
    let (ports, out_rings) = unsafe { (inputs.as_slice::<RawSlice>(), outputs.as_slice()) };
    let ins: Vec<Vec<RingBuffer>> = ports
        .iter()
        // SAFETY: the caller's contract.
        .map(|port| unsafe { port.as_slice() }.iter().map(attach).collect())
        .collect::<Result<_, _>>()?;
    let outs: Vec<RingBuffer> = out_rings.iter().map(attach).collect::<Result<_, _>>()?;

    let views = ins.iter().map(|rings| rings.iter().collect()).collect();
    let writers = outs.iter().collect();
    with_table(build, |table| {
        let entry = table
            .get(ty)
            .ok_or_else(|| ParamError::Decode(format!("unknown system type `{ty}`")))?;
        // A pack's async system runs on a thread inside the pack, on the group
        // the host's config placed it on.
        THREADS.with_borrow_mut(|threads| {
            (entry.make)(MakeCx {
                id: &instance.id,
                adapter: instance.adapter.as_ref(),
                def: &instance.def,
                params: Params(&value),
                inputs: views,
                outputs: writers,
                threads,
            })
        })
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
    let attached = match ring.owner {
        // SAFETY: `create` requires live, thread-safe ownership callbacks.
        Some(owner) => unsafe { RingBuffer::attach_owned(ring.base, ring.len, owner) },
        // SAFETY: an unowned ring lives in this module's own memory, which
        // outlives every handle in it.
        None => unsafe { RingBuffer::attach_raw(ring.base, ring.len) },
    };
    attached.map_err(|e| ParamError::Decode(e.to_string()))
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
        match catch_step(slot, Timestamp(now)) {
            Stepped::Unwound => Status::Panicked,
            Stepped::Ran | Stepped::Ended => Status::of_end(slot.as_ref().and_then(|s| s.ended())),
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

/// Allocates `len` zeroed bytes aligned to `align` for the host to write
/// into. Null on a zero length, an invalid layout, or no memory.
pub fn alloc(len: usize, align: usize) -> *mut u8 {
    match layout(len, align) {
        // SAFETY: `layout` refuses a zero size.
        Some(layout) => unsafe { std::alloc::alloc_zeroed(layout) },
        None => core::ptr::null_mut(),
    }
}

/// Frees what [`alloc`] returned. A null `ptr` or an invalid layout is ignored.
///
/// # Safety
/// A non-null `ptr` came from [`alloc`] with the same `len` and `align`,
/// and is freed once.
pub unsafe fn free(ptr: *mut u8, len: usize, align: usize) {
    if let Some(layout) = layout(len, align)
        && !ptr.is_null()
    {
        // SAFETY: the caller's contract.
        unsafe { std::alloc::dealloc(ptr, layout) };
    }
}

/// The layout [`alloc`] and [`free`] agree on, `None` for a zero length.
fn layout(len: usize, align: usize) -> Option<Layout> {
    if len == 0 {
        return None;
    }
    Layout::from_size_align(len, align).ok()
}

/// Exports `$build`'s table under the eight ABI names.
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
        pub unsafe extern "C" fn metor_fsw_def(
            ty: $crate::pack::raw::RawSlice,
            cx: $crate::pack::raw::RawSlice,
            dst: *mut u8,
            capacity: usize,
            written: *mut usize,
        ) -> u32 {
            unsafe { $crate::pack::system_def($build, ty, cx, dst, capacity, written) }
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

        #[unsafe(no_mangle)]
        pub extern "C" fn metor_fsw_alloc(len: usize, align: usize) -> *mut u8 {
            $crate::pack::alloc(len, align)
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn metor_fsw_free(ptr: *mut u8, len: usize, align: usize) {
            unsafe { $crate::pack::free(ptr, len, align) }
        }
    };
}

#[cfg(all(test, feature = "host"))]
mod tests;
