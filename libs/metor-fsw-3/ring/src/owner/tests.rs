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
