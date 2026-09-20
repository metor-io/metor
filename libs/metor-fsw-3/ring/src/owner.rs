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
mod tests {
    use super::*;
    use crate::{AttachError, Config, NoWake, RingBuffer};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    fn ring() -> RingBuffer {
        RingBuffer::create_in_memory(Config {
            capacity: 64,
            max_readers: 2,
        })
    }

    #[test]
    fn export_and_derived_handles_retain_backing() -> TestResult {
        let host = ring();
        let weak = Arc::downgrade(&host.inner);
        let export = host.export();
        drop(host);
        assert!(weak.upgrade().is_some());

        let (base, len) = export.region();
        // SAFETY: the export owns this initialized region and its callback context.
        let guest = unsafe { RingBuffer::attach_owned(base, len, export.owner()) }?;
        let mut writer = guest.writer(NoWake)?;
        let mut view = guest.view(NoWake)?;
        drop(export);
        drop(guest);

        writer.try_write(b"retained")?;
        let grant = view.try_read()?;
        drop(writer);
        assert_eq!(grant.as_deref(), Some(b"retained".as_slice()));
        assert!(weak.upgrade().is_some());
        drop(grant);
        drop(view);
        assert!(weak.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn worker_can_release_the_last_attachment() -> TestResult {
        let host = ring();
        let weak = Arc::downgrade(&host.inner);
        let export = host.export();
        let (base, len) = export.region();
        // SAFETY: the export owns this initialized region and its callback context.
        let guest = unsafe { RingBuffer::attach_owned(base, len, export.owner()) }?;
        let mut writer = guest.writer(NoWake)?;
        let mut view = guest.view(NoWake)?;
        drop(guest);
        drop(export);
        drop(host);

        let result = std::thread::spawn(move || -> TestResult {
            writer.try_write(b"worker")?;
            assert_eq!(view.try_read()?.as_deref(), Some(b"worker".as_slice()));
            Ok(())
        })
        .join();
        let result = result.map_err(|_| std::io::Error::other("worker panicked"))?;
        result?;
        assert!(weak.upgrade().is_none());
        Ok(())
    }

    struct CountedOwner {
        _export: RingExport,
        retains: AtomicUsize,
        releases: AtomicUsize,
    }

    unsafe extern "C" fn counted_retain(context: *const c_void) {
        let pointer = context.cast::<CountedOwner>();
        // SAFETY: the caller holds the raw Arc reference until retain returns.
        unsafe {
            Arc::increment_strong_count(pointer);
            (*pointer).retains.fetch_add(1, Relaxed);
        }
    }

    unsafe extern "C" fn counted_release(context: *const c_void) {
        let pointer = context.cast::<CountedOwner>();
        // SAFETY: each callback owns one reference from counted_retain.
        unsafe {
            (*pointer).releases.fetch_add(1, Relaxed);
            Arc::decrement_strong_count(pointer);
        }
    }

    #[test]
    fn attachments_balance_callbacks_on_success_and_failure() -> TestResult {
        let host = ring();
        let export = host.export();
        let (base, len) = export.region();
        let counted = Arc::new(CountedOwner {
            _export: export,
            retains: AtomicUsize::new(0),
            releases: AtomicUsize::new(0),
        });
        let context = Arc::into_raw(counted.clone());
        let owner = RawOwner {
            context: context.cast(),
            retain: counted_retain,
            release: counted_release,
        };
        // SAFETY: counted retains this live ring; the short length fails validation.
        let failed = unsafe { RingBuffer::attach_owned(base, 0, owner) };
        assert_eq!(failed.err(), Some(AttachError::TooSmall));
        assert_eq!(counted.retains.load(Relaxed), 1);
        assert_eq!(counted.releases.load(Relaxed), 1);

        // SAFETY: counted retains the initialized region for both attachments.
        let first = unsafe { RingBuffer::attach_owned(base, len, owner) }?;
        let second = unsafe { RingBuffer::attach_owned(base, len, owner) }?;
        let clone = first.clone();
        assert_eq!(counted.retains.load(Relaxed), 3);
        drop(first);
        assert_eq!(counted.releases.load(Relaxed), 1);
        drop(second);
        assert_eq!(counted.releases.load(Relaxed), 2);
        drop(clone);
        assert_eq!(counted.releases.load(Relaxed), 3);
        // SAFETY: release the reference created by Arc::into_raw above.
        unsafe { Arc::decrement_strong_count(context) };
        assert_eq!(Arc::strong_count(&counted), 1);
        Ok(())
    }
}
