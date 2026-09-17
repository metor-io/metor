//! Process-local ownership across independently compiled libraries.

use std::ffi::c_void;

use crate::{Inner, sync::Arc};

/// Borrowed ownership callbacks. Copying this descriptor does not retain it.
///
/// The context must remain live through `retain`. Each retain acquires one
/// reference, balanced by one release. Callbacks must be thread-safe,
/// non-panicking, and remain callable through the final release. Only the
/// originating library interprets the context; its code must remain loaded.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RawOwner {
    pub context: *const c_void,
    pub retain: unsafe extern "C" fn(*const c_void),
    pub release: unsafe extern "C" fn(*const c_void),
}

pub(super) struct Lease(RawOwner);

impl Lease {
    /// # Safety
    /// `owner` satisfies `RawOwner`'s contract and is live for this call.
    pub(super) unsafe fn acquire(owner: RawOwner) -> Self {
        // SAFETY: the caller guarantees a live owner and valid callbacks.
        unsafe { (owner.retain)(owner.context) };
        Self(owner)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // SAFETY: this lease owns one reference, released exactly once.
        unsafe { (self.0.release)(self.0.context) };
    }
}

// SAFETY: creating a lease requires thread-safe retain/release callbacks.
unsafe impl Send for Lease {}
unsafe impl Sync for Lease {}

/// Keeps an exported ring alive until the receiver acquires its own reference.
pub struct RingExport {
    base: *mut u8,
    len: usize,
    lease: Lease,
}

impl RingExport {
    pub(super) fn new(inner: Arc<Inner>) -> Self {
        let base = inner.backing.base();
        let len = inner.backing.len();
        let owner = RawOwner {
            context: Arc::into_raw(inner).cast(),
            retain,
            release,
        };
        Self {
            base,
            len,
            lease: Lease(owner),
        }
    }

    /// Region address and size, valid while this export is alive.
    pub fn region(&self) -> (*mut u8, usize) {
        (self.base, self.len)
    }

    /// Borrowed owner descriptor, valid while this export is alive.
    pub fn owner(&self) -> RawOwner {
        self.lease.0
    }
}

// SAFETY: the lease retains the region; ring access is synchronized internally.
unsafe impl Send for RingExport {}
unsafe impl Sync for RingExport {}

unsafe extern "C" fn retain(context: *const c_void) {
    // SAFETY: the export created this pointer with Arc::into_raw; a reference is live.
    unsafe { Arc::increment_strong_count(context.cast::<Inner>()) };
}

unsafe extern "C" fn release(context: *const c_void) {
    // SAFETY: each call releases one reference acquired by export or retain.
    unsafe { Arc::decrement_strong_count(context.cast::<Inner>()) };
}

#[cfg(all(test, not(ring_loom)))]
mod tests;
