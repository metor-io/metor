//! Bounded concurrency models; see `VERIFY.md` for limits.
use super::*;
use crate::models::{self, read, ring};
use crate::sync::thread;

const CAP: usize = 64;
const PAYLOAD: usize = 17;
const FRAME: usize = 48;

const _: () = assert!(FRAME == frame_len(PAYLOAD));
const _: () = assert!(CAP % FRAME != 0, "a gap would never form");

fn payload(n: u8) -> [u8; PAYLOAD] {
    core::array::from_fn(|i| n.wrapping_mul(0x11).wrapping_add(i as u8))
}

#[test]
fn test_drain_snapshot_races_wrap() {
    loom::model(|| {
        let ring = ring(128, 1);
        let writer = ring.writer(NoWake).expect("writer");
        let mut view = ring.view(NoWake).expect("reader");
        writer.try_write(&[1; 32]).expect("room");
        writer.try_write(&[2; 32]).expect("room");
        drop(view.try_read().expect("valid").expect("first"));
        let task = thread::spawn(move || writer.try_write(&[3; 32]).expect("wrap fits"));
        let batch = view.drain();
        let end = batch.committed;
        let records = batch
            .collect::<Result<Vec<_>, _>>()
            .expect("valid snapshot");
        assert_eq!(records[0], &[2; 32]);
        assert_eq!(records.len(), if end == 96 { 1 } else { 2 });
        task.join().expect("writer finished");
        assert_eq!(records[0], &[2; 32]);
        if records.len() == 2 {
            assert_eq!(records[1], &[3; 32]);
        }
        let later = view
            .drain()
            .collect::<Result<Vec<_>, _>>()
            .expect("valid remainder");
        assert_eq!(later.len(), if end == 96 { 1 } else { 0 });
    });
}

#[test]
fn test_registration_races_backpressure() {
    loom::model(|| {
        let ring = ring(CAP, 1);
        let w = ring.writer(NoWake).unwrap();
        // The registering view sits at a nonzero cursor, so the next write wraps around it.
        w.try_write(&payload(1))
            .expect("empty ring fits one record");

        let reader = ring.clone();
        let t = thread::spawn(move || {
            let mut v = reader.view(NoWake).unwrap();
            let start = v.cursor();
            let mut seen = Vec::new();
            let mut cursor = start;
            while let Some(record) = read(&mut v) {
                seen.push(record);
                assert!(v.cursor() > cursor);
                cursor = v.cursor();
            }
            (start, seen)
        });

        // The first wraps and needs the whole lap; the second needs the first consumed.
        let _ = w.try_write(&payload(2));
        let _ = w.try_write(&payload(3));

        let (start, seen) = t.join().unwrap();

        // Registration observes a record end or published wrap padding.
        let ahead: &[u8] = if start == FRAME as u64 || start == CAP as u64 {
            &[2, 3]
        } else if start == (CAP + FRAME) as u64 || start == (2 * CAP) as u64 {
            &[3]
        } else if start == (2 * CAP + FRAME) as u64 {
            &[]
        } else {
            panic!("view registered at {start}, not on a published boundary");
        };

        assert!(seen.len() <= ahead.len());
        for (got, &n) in seen.iter().zip(ahead) {
            assert_eq!(&got[..], &payload(n)[..], "record {n} came back wrong");
        }
    });
}

#[test]
fn test_wrap_marker_precedes_commit() {
    loom::model(|| {
        let ring = ring(CAP, 1);
        let w = ring.writer(NoWake).unwrap();
        let mut v = ring.view(NoWake).unwrap();

        // Park the reader where the gap will be published.
        w.try_write(&payload(1)).unwrap();
        assert_eq!(read(&mut v).as_deref(), Some(&payload(1)[..]));
        assert_eq!(v.cursor(), FRAME as u64);

        let t = thread::spawn(move || {
            w.try_write(&payload(2)).expect("the next lap is free");
        });

        let record = loop {
            match read(&mut v) {
                Some(record) => break record,
                None => loom::thread::yield_now(),
            }
        };
        assert_eq!(&record[..], &payload(2)[..]);
        assert_eq!(v.cursor(), (CAP + FRAME) as u64);
        t.join().unwrap();
    });
}

#[test]
fn test_writer_claim_handoff() {
    loom::model(|| {
        let a = ring(CAP, 1);
        let b = a.clone();

        let t = thread::spawn(move || b.writer(NoWake).map(|w| w.try_write(&payload(2)).is_ok()));
        let mine = a.writer(NoWake).map(|w| w.try_write(&payload(1)).is_ok());
        let theirs = t.join().unwrap();

        // Both may succeed if the first claim is dropped before the second.
        assert!(mine.is_ok() || theirs.is_ok());
    });
}

#[test]
fn test_grant_pins_bytes() {
    loom::model(|| {
        let ring = ring(CAP, 1);
        let w = ring.writer(NoWake).unwrap();
        let mut v = ring.view(NoWake).unwrap();
        w.try_write(&payload(1)).unwrap();

        let t = thread::spawn(move || w.try_write(&payload(2)).is_ok());

        let grant = v.try_read().expect("never corrupt").expect("committed");
        assert_eq!(&grant[..], &payload(1)[..]);
        let wrote = t.join().unwrap();
        assert_eq!(&grant[..], &payload(1)[..]);
        // The grant pins the cursor at 0; a second record needs the gap plus its frame.
        assert!(!wrote, "writer reused bytes a grant had borrowed");
        drop(grant);
    });
}

#[test]
fn test_slowest_reader_backpressure() {
    loom::model(|| {
        let ring = ring(CAP, 2);
        let w = ring.writer(NoWake).unwrap();
        let mut fast = ring.view(NoWake).unwrap();
        let slow = ring.view(NoWake).unwrap();
        w.try_write(&payload(1)).unwrap();

        let handle = ring.clone();
        let t = thread::spawn(move || {
            while read(&mut fast).is_some() {}
            handle.committed()
        });

        let mut wrote = 1;
        while w.try_write(&payload(2)).is_ok() {
            wrote += 1;
            if wrote == 4 {
                break;
            }
        }
        let committed = t.join().unwrap();

        assert_eq!(slow.cursor(), 0);
        assert!(
            committed <= CAP as u64,
            "committed {committed} laps a reader parked at 0"
        );
    });
}

#[test]
fn test_cursor_within_committed() {
    loom::model(|| {
        let ring = ring(CAP, 1);
        let w = ring.writer(NoWake).unwrap();
        let mut v = ring.view(NoWake).unwrap();
        w.try_write(&payload(1)).unwrap();
        assert!(read(&mut v).is_some());

        // Skipping the gap parks the cursor at the lap boundary between the writer's stores.
        let reader = thread::spawn(move || {
            let _ = v.try_read().map(drop);
            let _ = v.try_read().map(drop);
        });

        // A cursor ahead of the writer would make `fits` report `Corrupt`.
        assert_ne!(w.try_write(&payload(2)), Err(WriteError::Corrupt));
        assert_ne!(w.try_write(&payload(3)), Err(WriteError::Corrupt));
        reader.join().unwrap();
    });
}

#[test]
fn test_registration_during_padding_publication() {
    loom::model(models::registration_during_padding_publication);
}

#[test]
fn test_latest_races_padding_publication() {
    loom::model(|| {
        let ring = ring(64, 1);
        let mut reader = ring.view(NoWake).unwrap();
        let writer = ring.writer(NoWake).unwrap();
        writer.try_write(&[1; 8]).unwrap();
        let task = thread::spawn(move || {
            assert_eq!(writer.try_write(&[2; 48]), Err(WriteError::WouldBlock));
        });
        let latest = reader.try_latest().unwrap().unwrap();
        assert_eq!(latest, &[1; 8]);
        task.join().unwrap();
        assert_eq!(latest, &[1; 8]);
        assert_eq!(reader.try_latest().unwrap(), Some(&[1; 8][..]));
    });
}

#[test]
fn test_reclaim_preserves_reused_slot() {
    loom::model(models::reclaim_preserves_reused_slot);
}

#[test]
fn test_concurrent_reclaim_preserves_new_owner() {
    loom::model(models::concurrent_reclaim_preserves_new_owner);
}
