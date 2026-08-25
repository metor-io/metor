//! The ring half of the pack ABI, linked rather than reimplemented.
//!
//! A compiled Python pack must speak the coordinator's real ring format —
//! magic, version, reader registration's SeqCst handshake, Release-committed
//! positions — and that format is safety-critical concurrency code with a
//! loom model behind it. So the guest links the real `metor-fsw-ring` (its
//! `default-features = false` build has no mmap and no wake machinery) and
//! this module reduces it to the flat `pk_*` calls generated code can make:
//! open a view or writer over a bound region, ask a view for the newest
//! record, push one record. Handles are `Box`es surrendered to raw pointers;
//! they are created at bind time — before the host pins guest memory — and
//! the per-cycle calls (`pk_view_latest`, `pk_write`) allocate nothing.
//!
//! `fsw_pack_alloc` and `fsw_pack_ring_init` are exported here directly:
//! they are the guest-owned halves of ring provisioning and their bodies are
//! exactly the native `run_pack_alloc`/`run_pack_ring_init` minus the
//! `catch_unwind` (wasm32 with `panic = "abort"` cannot unwind; the config is
//! validated instead of caught). The rest of the pack ABI — describe, create,
//! bind, execute — is *generated* per module, since it depends on the
//! compiled systems.

use metor_fsw_ring::{Config, NoWake, RingBuffer, View, Writer, region_len};

/// `fsw_pack_alloc`: zeroed, 8-aligned bytes for the host to place params,
/// ring regions, and bind arrays in. Null on a zero length or failure.
#[unsafe(no_mangle)]
pub extern "C" fn fsw_pack_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return core::ptr::null_mut();
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(len, 8) else {
        return core::ptr::null_mut();
    };
    // SAFETY: `layout` is non-zero-sized and well-formed.
    unsafe { std::alloc::alloc_zeroed(layout) }
}

/// `fsw_pack_ring_init`: format a ring into a region from `fsw_pack_alloc`.
/// Returns `0`, or `-1` on a bad region or config. The config is validated
/// up front because the ring's own `layout` asserts on a bad one, and an
/// assert here would abort the guest without a message.
#[unsafe(no_mangle)]
pub extern "C" fn fsw_pack_ring_init(
    ptr: *mut u8,
    len: usize,
    capacity: u32,
    max_readers: u32,
) -> i32 {
    let capacity = capacity as usize;
    if ptr.is_null() || capacity == 0 || !capacity.is_power_of_two() || max_readers == 0 {
        return -1;
    }
    let cfg = Config {
        capacity,
        max_readers: max_readers as usize,
    };
    if len < region_len(&cfg) {
        return -1;
    }
    // SAFETY: the host hands a live region from `fsw_pack_alloc` that nothing
    // else is reading while it is formatted.
    match unsafe { RingBuffer::create_raw(ptr, len, cfg) } {
        Ok(ring) => {
            drop(ring);
            0
        }
        Err(_) => -1,
    }
}

/// `fsw_pack_set_now`: the ambient-clock publish point of the native ABI. A
/// compiled Python pack stamps records from `execute`'s own `now` argument
/// and logs nothing during bind, so there is no clock to publish.
#[unsafe(no_mangle)]
pub extern "C" fn fsw_pack_set_now(_now: u64) {}

/// Open a read view over a bound ring region, claiming a reader slot in it.
/// Returns a handle for the other `pk_view_*` calls, or `0` on a region that
/// does not attach or a full reader table.
#[unsafe(no_mangle)]
pub extern "C" fn pk_view_open(ptr: *mut u8, len: usize) -> u32 {
    // SAFETY: the host bound this region for the instance's lifetime; the
    // handle is closed (or the instance torn down whole) before it goes away.
    let Ok(ring) = (unsafe { RingBuffer::attach_raw(ptr, len) }) else {
        return 0;
    };
    let Ok(view) = ring.view(NoWake) else {
        return 0;
    };
    Box::into_raw(Box::new(view)) as usize as u32
}

/// Open the single writer over a bound ring region. Returns a handle for
/// `pk_write`, or `0` when the region does not attach or the writer role is
/// already claimed.
#[unsafe(no_mangle)]
pub extern "C" fn pk_writer_open(ptr: *mut u8, len: usize) -> u32 {
    // SAFETY: as `pk_view_open`.
    let Ok(ring) = (unsafe { RingBuffer::attach_raw(ptr, len) }) else {
        return 0;
    };
    let Ok(writer) = ring.writer(NoWake) else {
        return 0;
    };
    Box::into_raw(Box::new(writer)) as usize as u32
}

/// The ring's absolute committed position — what the generated run rule
/// compares against its held copy to decide whether the driving input moved.
///
/// # Safety
/// `view` is a live handle from [`pk_view_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pk_view_committed(view: u32) -> u64 {
    // SAFETY: caller passes a live `pk_view_open` handle.
    let view = unsafe { &mut *(view as usize as *mut View<NoWake>) };
    view.committed()
}

/// Borrow the newest committed record: writes `[payload_ptr, payload_len]`
/// into `out` and returns `1`, or returns `0` with nothing committed and
/// `-1` on a corrupt region. The grant is dropped before returning — its pin
/// parks the cursor at the record, and the record's bytes stay valid because
/// the host writes this ring only between executes.
///
/// # Safety
/// `view` is a live handle from [`pk_view_open`]; `out` names two writable
/// `u32` slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pk_view_latest(view: u32, out: *mut u32) -> i32 {
    // SAFETY: caller passes a live `pk_view_open` handle.
    let view = unsafe { &mut *(view as usize as *mut View<NoWake>) };
    match view.try_latest() {
        Ok(Some(grant)) => {
            let (ptr, len) = (grant.as_ptr() as usize as u32, grant.len() as u32);
            drop(grant);
            // SAFETY: caller asserts two writable u32 slots at `out`.
            unsafe {
                *out = ptr;
                *out.add(1) = len;
            }
            1
        }
        Ok(None) => 0,
        Err(_) => -1,
    }
}

/// Push one record. Returns `0`, or `-1` when the record does not fit (a
/// full ring drops the sample rather than blocking a cycle).
///
/// # Safety
/// `writer` is a live handle from [`pk_writer_open`]; `ptr..ptr+len` is
/// readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pk_write(writer: u32, ptr: *const u8, len: usize) -> i32 {
    // SAFETY: caller passes a live `pk_writer_open` handle.
    let writer = unsafe { &mut *(writer as usize as *mut Writer<NoWake>) };
    // SAFETY: caller asserts `ptr..ptr+len` is readable.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    match writer.try_write(bytes) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Drop a view handle, releasing its reader slot.
///
/// # Safety
/// `view` came from [`pk_view_open`] and is not used afterward.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pk_view_close(view: u32) {
    if view == 0 {
        return;
    }
    // SAFETY: caller transfers the live handle here exactly once.
    drop(unsafe { Box::from_raw(view as usize as *mut View<NoWake>) });
}

/// Drop a writer handle, releasing the writer claim.
///
/// # Safety
/// `writer` came from [`pk_writer_open`] and is not used afterward.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pk_writer_close(writer: u32) {
    if writer == 0 {
        return;
    }
    // SAFETY: caller transfers the live handle here exactly once.
    drop(unsafe { Box::from_raw(writer as usize as *mut Writer<NoWake>) });
}
