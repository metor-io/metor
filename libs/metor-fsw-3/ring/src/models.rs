//! Test helpers and the concurrency bodies shared by the Loom models and the
//! ordinary tests, so Miri runs the same code Loom explores.
use super::*;
use crate::sync::thread;

pub(crate) const DEAD_PID: u64 = 999_999_999;

pub(crate) fn ring(capacity: usize, max_readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity,
        max_readers,
    })
}

/// Copies and consumes the next record.
pub(crate) fn read(view: &mut View<NoWake>) -> Option<Vec<u8>> {
    view.try_read().expect("never corrupt").map(|r| r.to_vec())
}

pub(crate) fn registration_during_padding_publication() {
    let ring = ring(64, 1);
    let writer = ring.writer(NoWake).unwrap();
    writer.try_write(&[1; 8]).unwrap();
    let consumer = ring.clone();
    let task = thread::spawn(move || {
        let mut reader = consumer.view(NoWake).unwrap();
        if let Some(grant) = reader.try_read().unwrap() {
            assert_eq!(&*grant, &[2; 48]);
        }
    });
    let result = writer.try_write(&[2; 48]);
    assert!(result.is_ok() || result == Err(WriteError::WouldBlock));
    task.join().unwrap();
}

pub(crate) fn reclaim_preserves_reused_slot() {
    let ring = ring(64, 1);
    let reader = ring.view(NoWake).unwrap();
    ring.inner.slot(0).owner.store(DEAD_PID, Release);
    drop(reader);
    assert_eq!(ring.inner.slot(0).owner.load(Acquire), 0);
    let consumer = ring.clone();
    let task = thread::spawn(move || {
        let reader = consumer.view(NoWake).unwrap();
        assert_ne!(consumer.inner.slot(0).cursor.load(Acquire), FREE_SLOT);
        assert_eq!(consumer.inner.slot(0).owner.load(Acquire), owner_tag());
        drop(reader);
    });
    // SAFETY: the synthetic owner has no live handles.
    unsafe { ring.reclaim_owner(DEAD_PID) };
    task.join().unwrap();
}

pub(crate) fn concurrent_reclaim_preserves_new_owner() {
    let ring = ring(64, 1);
    ring.inner.slot(0).cursor.store(0, Release);
    ring.inner.slot(0).owner.store(DEAD_PID, Release);
    let reclaimer = ring.clone();
    let task = thread::spawn(move || {
        // SAFETY: the synthetic owner has no live handles.
        unsafe { reclaimer.reclaim_owner(DEAD_PID) };
    });
    // SAFETY: the synthetic owner has no live handles.
    unsafe { ring.reclaim_owner(DEAD_PID) };
    if let Ok(reader) = ring.view(NoWake) {
        assert_ne!(ring.inner.slot(0).cursor.load(Acquire), FREE_SLOT);
        assert_eq!(ring.inner.slot(0).owner.load(Acquire), owner_tag());
        task.join().unwrap();
        assert_ne!(ring.inner.slot(0).cursor.load(Acquire), FREE_SLOT);
        drop(reader);
    } else {
        task.join().unwrap();
    }
}
