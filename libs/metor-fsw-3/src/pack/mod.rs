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
use crate::io::region::Region;
use crate::io::{End, SharedIo};
use crate::system::SystemDef;
use crate::thread::Threads;
use def::{DefCxOwned, Instance, PackDef};
use raw::{RawRing, RawSlice};

/// The ABI version of the exports in this module.
///
/// The host refuses to load a pack whose version does not match its own.
pub const ABI_VERSION: u32 = 10;

/// The status word [`execute`] returns.
///
/// The host treats an unknown word as a panic.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok = 0,
    Panicked = 1,
    Completed = 2,
    Aborted = 3,
    /// The system failed. The reason is logged as a fault on its `log` output.
    Failed = 4,
}

impl Status {
    /// Converts a status word, returning `None` if the word is unknown.
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

    /// Converts a status word, treating an unknown word as a panic.
    pub fn from_raw(word: u32) -> Status {
        Status::try_from_raw(word).unwrap_or(Status::Panicked)
    }

    /// Returns the status for a step whose end reason is `end`.
    pub fn of_end(end: Option<&EndReason>) -> Status {
        match end {
            None => Status::Ok,
            Some(EndReason::Completed) => Status::Completed,
            Some(EndReason::Aborted) => Status::Aborted,
            Some(EndReason::Failed(_) | EndReason::Unloaded) => Status::Failed,
            Some(EndReason::Panicked) => Status::Panicked,
        }
    }

    /// Returns the end reason this status reports, or `None` if the step is
    /// still running.
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

/// The status words returned by the descriptor and definition exports.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefStatus {
    Ok = 0,
    TooSmall = 1,
    Encode = 2,
    Panicked = 3,
    /// The type rejected the config. `dst` holds the [`DefError`] as JSON.
    Refused = 4,
}

thread_local! {
    static TABLE: OnceCell<SystemTable> = const { OnceCell::new() };
    /// The threads this pack runs its own async systems on.
    static THREADS: RefCell<Threads> = RefCell::new(Threads::new());
    /// What the pack's own IO systems share.
    static IO: crate::io::SharedIo = crate::io::SharedIo::default();
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

/// Writes the pack's descriptor as JSON into a buffer the caller owns.
///
/// On failure, `written` is left at zero.
///
/// # Safety
/// `dst` must point to `capacity` writable bytes, or be null when `capacity` is
/// zero. `written` must be writable and must not overlap `dst`. Neither pointer
/// is kept after the call.
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

/// Writes the definition of type `ty` for the config in `cx` into a buffer the
/// caller owns.
///
/// If the type rejects the config, the [`DefError`] is written as JSON instead.
/// On any other failure, `written` is left at zero.
///
/// # Safety
/// `ty` and `cx` must meet the contract of [`RawSlice::as_bytes`]. `dst` must
/// point to `capacity` writable bytes, or be null when `capacity` is zero.
/// `written` must be writable and must not overlap `dst`. Neither pointer is
/// kept after the call.
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

/// Decodes `ty` and `cx`, then computes that type's definition.
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

/// Wraps the caller's output buffer in a cursor.
///
/// # Safety
/// `dst` must point to `capacity` writable bytes that nothing else touches
/// while the cursor lives, or `capacity` must be zero.
unsafe fn out_buf<'a>(dst: *mut u8, capacity: usize) -> std::io::Cursor<&'a mut [u8]> {
    let buffer = if capacity == 0 {
        &mut []
    } else {
        // SAFETY: the caller's contract.
        unsafe { core::slice::from_raw_parts_mut(dst, capacity) }
    };
    std::io::Cursor::new(buffer)
}

/// Writes one value as JSON and returns `ok` if it fit.
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

/// The arguments of `metor_fsw_create`, but its error slot.
pub struct CreateArgs {
    pub ty: RawSlice,
    pub params: RawSlice,
    pub def: RawSlice,
    /// A slice of [`RawSlice`]s of [`RawRing`]s.
    pub inputs: RawSlice,
    /// A slice of [`RawRing`]s.
    pub outputs: RawSlice,
    /// Read and written. In, the host's copy of the system's IO region, or
    /// empty on its first make; out, the region in use, or empty for a
    /// system with no IO.
    pub table: *mut RawRing,
}

/// Creates one system instance on the rings the host passed in.
///
/// Returns null on failure. If the failure has a [`ParamError`], it is written
/// through `error` as JSON. If the failure was a panic, `error` is an empty
/// slice.
///
/// # Safety
/// Each slice in `args` meets the contract of [`RawSlice::as_slice`]. Every
/// ring, and a non-empty `table`, has live, valid ownership callbacks
/// covering its region. `table` and `error` are writable and do not alias
/// the input arrays. Calls on this thread must not reenter `create`.
pub unsafe fn create(
    build: fn() -> SystemTable,
    args: CreateArgs,
    error: *mut RawSlice,
) -> *mut c_void {
    // SAFETY: the caller supplies a writable result pointer.
    unsafe { error.write(RawSlice::EMPTY) };
    crate::panic::catch(|| {
        // SAFETY: the caller's arrays, region, and ownership callbacks remain
        // valid here.
        match unsafe { make(build, args) } {
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

/// Looks up `ty`, attaches every ring, and creates the system on them.
///
/// # Safety
/// The same as for [`create`].
unsafe fn make(build: fn() -> SystemTable, args: CreateArgs) -> Result<Box<dyn Step>, ParamError> {
    let CreateArgs {
        ty,
        params,
        def,
        inputs,
        outputs,
        table,
    } = args;
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
    let io = IO.with(Clone::clone);
    // SAFETY: the caller's contract.
    let given = unsafe { table.read() };
    // SAFETY: as above; the region and its owner are live.
    unsafe { adopt(&io, &instance, given) }?;
    let step = with_table(build, |table| {
        let entry = table
            .get(ty)
            .ok_or_else(|| ParamError::Decode(format!("unknown system type `{ty}`")))?;
        // A pack's async systems run on threads inside the pack. The host's
        // config picks which thread.
        THREADS.with_borrow_mut(|threads| {
            (entry.make)(MakeCx {
                id: &instance.id,
                adapter: instance.adapter.as_ref(),
                def: &instance.def,
                params: Params(&value),
                inputs: views,
                outputs: writers,
                threads,
                io: &io,
                seed: None,
                namespace: "",
            })
        })
    })?;
    // SAFETY: the caller's contract.
    unsafe { table.write(region_of(&io, &instance.id)) };
    Ok(step)
}

/// Makes the system's member over the host's region `given`, unless this
/// pack already has one. An empty region leaves the first make to lay out
/// its own.
///
/// # Safety
/// A non-empty `given` is live, and its owner meets `RawOwner`'s contract.
unsafe fn adopt(io: &SharedIo, instance: &Instance, given: RawRing) -> Result<(), ParamError> {
    let id = &instance.id;
    let caps: Vec<String> = host_caps(&instance.def).collect();
    let known = io.borrow().find(id).is_some();
    let (Some(base), Some(owner)) = (core::ptr::NonNull::new(given.base), given.owner) else {
        return Ok(());
    };
    if known || caps.is_empty() || given.len == 0 {
        return Ok(());
    }
    // SAFETY: the caller's contract.
    let region = unsafe { Region::shared(base, given.len, owner) };
    let corrupt = |_| ParamError::CorruptTable { system: id.clone() };
    io.borrow_mut()
        .adopt_region(id, caps, region)
        .map_err(corrupt)?;
    Ok(())
}

/// The region of system `id`'s member, with an owner that keeps it alive, or
/// an empty one when it has no IO.
fn region_of(io: &SharedIo, id: &str) -> RawRing {
    let member = io.borrow().find(id);
    let export = member.and_then(|m| m.table.bufs.export());
    export.map_or(RawRing::EMPTY, |(span, owner)| RawRing {
        base: span.cast().as_ptr(),
        len: span.len(),
        owner: Some(owner),
    })
}

/// The names of the `Cap` parameters in `def`.
pub(crate) fn host_caps(def: &SystemDef) -> impl Iterator<Item = String> + '_ {
    let host = def.grants.iter().filter(|g| g.end == End::Host);
    host.map(|g| g.param.to_string())
}

/// Decodes the params JSON. Empty bytes mean no params.
fn decode_params(bytes: &[u8]) -> Result<serde_json::Value, ParamError> {
    if bytes.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(bytes).map_err(|e| ParamError::Decode(e.to_string()))
}

/// Attaches to a ring the host allocated.
fn attach(ring: &RawRing) -> Result<RingBuffer, ParamError> {
    let attached = match ring.owner {
        // SAFETY: `create` requires live, thread-safe ownership callbacks.
        Some(owner) => unsafe { RingBuffer::attach_owned(ring.base, ring.len, owner) },
        // SAFETY: a ring with no owner lives in this module's own memory, which
        // outlives every handle to it.
        None => unsafe { RingBuffer::attach_raw(ring.base, ring.len) },
    };
    attached.map_err(|e| ParamError::Decode(e.to_string()))
}

/// Serializes `source` into this thread's error buffer.
fn store_error(source: &ParamError) -> RawSlice {
    ERROR.with_borrow_mut(|buf| {
        buf.clear();
        // If the error fails to serialize, the host gets an empty slice.
        let _ = serde_json::to_writer(&mut *buf, source);
        RawSlice::of(buf)
    })
}

/// Runs one cycle of an instance.
///
/// # Safety
/// `instance` must come from [`create`] on this thread and must not have been
/// destroyed.
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

/// Drops an instance, which frees its ring readers and writers.
///
/// # Safety
/// `instance` must come from [`create`] on this thread, and must be destroyed
/// only once.
pub unsafe fn destroy(instance: *mut c_void) {
    crate::panic::catch(|| {
        // SAFETY: the caller's contract; the box was made by `create`.
        drop(unsafe { Box::from_raw(instance.cast::<Option<Box<dyn Step>>>()) })
    });
}

/// Allocates `len` zeroed bytes, aligned to `align`, for the host to write into.
///
/// Returns null if `len` is zero, the layout is invalid, or memory runs out.
pub fn alloc(len: usize, align: usize) -> *mut u8 {
    match layout(len, align) {
        // SAFETY: `layout` refuses a zero size.
        Some(layout) => unsafe { std::alloc::alloc_zeroed(layout) },
        None => core::ptr::null_mut(),
    }
}

/// Frees memory returned by [`alloc`].
///
/// A null `ptr` or an invalid layout is ignored.
///
/// # Safety
/// A non-null `ptr` must come from [`alloc`] with the same `len` and `align`,
/// and must be freed only once.
pub unsafe fn free(ptr: *mut u8, len: usize, align: usize) {
    if let Some(layout) = layout(len, align)
        && !ptr.is_null()
    {
        // SAFETY: the caller's contract.
        unsafe { std::alloc::dealloc(ptr, layout) };
    }
}

/// Returns the layout that [`alloc`] and [`free`] share, or `None` if `len` is
/// zero.
fn layout(len: usize, align: usize) -> Option<Layout> {
    if len == 0 {
        return None;
    }
    Layout::from_size_align(len, align).ok()
}

/// Exports a pack's systems through the C ABI.
///
/// `$build` is a function that returns the pack's [`SystemTable`]. The macro
/// defines the eight `extern "C"` functions the host looks up when it loads the
/// pack.
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
            table: *mut $crate::pack::raw::RawRing,
            error: *mut $crate::pack::raw::RawSlice,
        ) -> *mut ::core::ffi::c_void {
            let args = $crate::pack::CreateArgs {
                ty,
                params,
                def,
                inputs,
                outputs,
                table,
            };
            unsafe { $crate::pack::create($build, args, error) }
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
