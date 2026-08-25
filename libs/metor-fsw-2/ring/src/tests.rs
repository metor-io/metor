//! The synchronous tests stick to the `try_*` APIs plus `std::thread` so they
//! run under Miri, which cannot drive the async runtime, and exercise the
//! unsafe pointer and atomic paths for provenance, leaks, and data races.
//! `MIRI.md` in the crate root describes the coverage. Loop bounds shrink
//! under `cfg!(miri)`.
//!
//! Two sections are `#[cfg(not(miri))]` because Miri cannot run them: the mmap
//! tests, which need a real temp directory and a real `mmap`, and the async
//! tests at the end, which need the executor. The async ones exist because
//! `View::read` and `View::read_into` are reachable from no other test here,
//! and Kani and loom cannot reach them either.

use super::*;

// Header-field offsets the corruption tests scribble on.
const OFF_CAPACITY: usize = core::mem::offset_of!(RegionHeader, capacity);
const OFF_DATA_OFFSET: usize = core::mem::offset_of!(RegionHeader, data_offset);
const OFF_READER_TABLE_OFFSET: usize = core::mem::offset_of!(RegionHeader, reader_table_offset);

fn ring(capacity: usize, max_readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity,
        max_readers,
    })
}

// ----- Basic single-threaded paths -----

#[test]
fn roundtrip() {
    let rb = ring(1024, 4);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    w.try_write(b"hello world").unwrap();
    w.try_write(b"foo").unwrap();

    let mut buf = Vec::new();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"hello world");
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"foo");
    assert!(!v.try_read_into(&mut buf).unwrap(), "nothing left");
}

/// A payload size that does not divide the capacity forces a wrap gap, and
/// the reader (kept caught up, so the writer never blocks) must skip it.
#[test]
fn wraparound_aligned() {
    // With capacity 64 and a 16-byte payload each record is 24 bytes. Two fit
    // at 0 and 24; the third would straddle (48 + 24 > 64), so the writer
    // leaves a 16-byte gap and wraps to offset 0.
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    let mut buf = Vec::new();

    for i in 0u8..12 {
        let msg = [i; 16];
        w.try_write(&msg).unwrap();
        assert!(v.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], &msg[..], "message {i} survived the wrap");
        assert!(!v.try_read_into(&mut buf).unwrap());
    }
    assert!(v.cursor() > 64, "the stream wrapped");
}

#[test]
fn multi_reader() {
    let rb = ring(1024, 4);
    let mut w = rb.writer(NoWake).unwrap();
    let mut a = rb.view(NoWake).unwrap();
    let mut b = rb.view(NoWake).unwrap();

    w.try_write(b"one").unwrap();
    w.try_write(b"two").unwrap();

    let mut buf = Vec::new();
    for v in [&mut a, &mut b] {
        assert!(v.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"one");
        assert!(v.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"two");
        assert!(!v.try_read_into(&mut buf).unwrap());
    }
}

#[test]
fn reader_table_claim_free() {
    let rb = ring(256, 2);

    let a = rb.view(NoWake).unwrap();
    let b = rb.view(NoWake).unwrap();
    // The table is full at `max_readers`, so a third claim is refused.
    assert_eq!(rb.view(NoWake).err(), Some(FullReaderTable));

    drop(b);
    // The freed slot is reused, and the table is full again.
    let _c = rb.view(NoWake).unwrap();
    assert_eq!(rb.view(NoWake).err(), Some(FullReaderTable));
    drop(a);
    // Dropping another view frees a slot for a fresh claim.
    let _d = rb.view(NoWake).unwrap();
}

// ----- Backpressure / borrow semantics -----

/// The writer returns `WouldBlock` rather than overwrite a reader, and
/// freeing space lets the next write through.
#[test]
fn backpressure() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    let mut buf = Vec::new();

    // Each record is 24 bytes. Two fit (48 <= 64), and a third would
    // overwrite the idle reader.
    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    assert_eq!(w.try_write(&[3u8; 16]), Err(WriteError::WouldBlock));

    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &[1u8; 16]);

    // Reading freed space, so the write now succeeds.
    w.try_write(&[3u8; 16]).unwrap();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &[2u8; 16]);
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &[3u8; 16]);
}

#[test]
fn borrow_read() {
    let rb = ring(256, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    w.try_write(b"borrowed").unwrap();
    {
        let grant = v.try_read().unwrap().expect("a record");
        assert_eq!(&grant[..], b"borrowed");
    } // drop advances the cursor
    assert!(v.try_read().unwrap().is_none());
}

#[test]
fn oversize_message_rejected() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    // A 64-byte payload makes a 72-byte record, more than the whole region.
    assert_eq!(
        w.try_write(&[0u8; 64]),
        Err(WriteError::InsufficientCapacity)
    );
}

// ----- try_latest -----

/// `try_latest` consumes every older record, pins the newest (serving it
/// again while no new data arrives), and follows the live edge as records
/// land.
#[test]
fn latest_pins_newest() {
    let rb = ring(256, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    assert!(v.try_latest().unwrap().is_none(), "no record committed yet");

    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    w.try_write(&[3u8; 16]).unwrap();

    assert_eq!(&v.try_latest().unwrap().expect("newest")[..], &[3u8; 16]);
    // No new data, so the pinned record is served again.
    assert_eq!(&v.try_latest().unwrap().expect("re-served")[..], &[3u8; 16]);

    w.try_write(&[4u8; 16]).unwrap();
    assert_eq!(
        &v.try_latest().unwrap().expect("follows the edge")[..],
        &[4u8; 16]
    );
}

/// The pin is load-bearing. The writer cannot reclaim the pinned record's
/// bytes, and moving the pin unblocks it.
///
/// With capacity 64 each record is 24 bytes. After two writes the reader pins
/// the record at absolute 24..48. The third write wraps (gap at 48, record at
/// absolute 64..88, physical 0..24) and fits, but the fourth (physical
/// 24..48) would overwrite the pin.
#[test]
fn latest_pin_backpressures_writer() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    assert_eq!(
        &v.try_latest().unwrap().expect("pin record 2")[..],
        &[2u8; 16]
    );

    w.try_write(&[3u8; 16]).unwrap(); // wraps; does not touch the pin
    assert_eq!(
        w.try_write(&[4u8; 16]),
        Err(WriteError::WouldBlock),
        "the pinned record's bytes are protected"
    );

    // The reader moves its pin to the newest record, freeing the old one...
    assert_eq!(
        &v.try_latest().unwrap().expect("pin record 3")[..],
        &[3u8; 16]
    );
    // ...and the blocked write now goes through.
    w.try_write(&[4u8; 16]).unwrap();
    assert_eq!(&v.try_latest().unwrap().expect("record 4")[..], &[4u8; 16]);
}

// ----- Concurrency / Miri data-race coverage -----

use std::sync::atomic::{AtomicBool, Ordering as O};
use std::thread;

/// A writer that spins on `WouldBlock` and a reader draining concurrently
/// must reconstruct the full ordered stream.
#[test]
fn concurrent_full_stream() {
    let n: u64 = if cfg!(miri) { 48 } else { 4_000 };
    let rb = ring(128, 1);
    let v = rb.view(NoWake).unwrap();

    let consumer = thread::spawn(move || {
        let mut v = v;
        let mut buf = Vec::new();
        let mut got = Vec::with_capacity(n as usize);
        while (got.len() as u64) < n {
            match v.try_read_into(&mut buf) {
                Ok(true) => got.push(u64::from_le_bytes(buf[..8].try_into().unwrap())),
                Ok(false) => thread::yield_now(),
                Err(e) => panic!("reader must never error: {e:?}"),
            }
        }
        got
    });

    let producer = {
        let rb = rb.clone();
        thread::spawn(move || {
            let mut w = rb.writer(NoWake).unwrap();
            for i in 0..n {
                loop {
                    match w.try_write(&i.to_le_bytes()) {
                        Ok(()) => break,
                        Err(WriteError::WouldBlock) => thread::yield_now(),
                        Err(e) => panic!("unexpected {e:?}"),
                    }
                }
            }
        })
    };

    producer.join().unwrap();
    let got = consumer.join().unwrap();
    let expected: Vec<u64> = (0..n).collect();
    assert_eq!(got, expected, "reader lost or reordered data");
}

/// Register and drop views from several threads while a writer runs. This
/// stresses the CAS slot claim and the wait-free slot free. A live churner
/// view can backpressure the writer, so `WouldBlock` is tolerated.
#[test]
fn concurrent_reader_churn() {
    let churn = if cfg!(miri) { 3 } else { 8 };
    let rounds = if cfg!(miri) { 4 } else { 200 };
    let rb = ring(128, 16);
    let stop = std::sync::Arc::new(AtomicBool::new(false));

    let writer = {
        let rb = rb.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut w = rb.writer(NoWake).unwrap();
            let mut i = 0u64;
            while !stop.load(O::Relaxed) {
                match w.try_write(&i.to_le_bytes()) {
                    Ok(()) => i = i.wrapping_add(1),
                    Err(WriteError::WouldBlock) => {}
                    Err(e) => panic!("unexpected {e:?}"),
                }
                thread::yield_now();
            }
        })
    };

    let churners: Vec<_> = (0..churn)
        .map(|_| {
            let rb = rb.clone();
            thread::spawn(move || {
                for _ in 0..rounds {
                    if let Ok(mut v) = rb.view(NoWake) {
                        let mut buf = Vec::new();
                        for _ in 0..3 {
                            let _ = v.try_read_into(&mut buf);
                        }
                    }
                }
            })
        })
        .collect();

    for c in churners {
        c.join().unwrap();
    }
    stop.store(true, O::Relaxed);
    writer.join().unwrap();
    assert_eq!(rb.reader_count(), 0);
}

// ----- attach_raw (non-owning, same-process) -----

/// A non-owning handle reconstructed over the same region sees the identical
/// atomics. Records written through one handle are read through the other, in
/// both directions.
#[test]
fn raw_attach_same_process_roundtrip() {
    let rb = ring(1024, 4);
    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    let mut buf = Vec::new();

    {
        let mut vr = raw.view(NoWake).unwrap();
        let mut wb = rb.writer(NoWake).unwrap();
        wb.try_write(b"box->raw").unwrap();
        assert!(vr.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"box->raw");
    }
    {
        let mut vb = rb.view(NoWake).unwrap();
        let mut wr = raw.writer(NoWake).unwrap();
        wr.try_write(b"raw->box").unwrap();
        assert!(vb.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"raw->box");
    }
}

/// Geometry is recovered from the region header, not passed to `attach_raw`.
/// The committed position and the capacity both match the original handle.
#[test]
fn raw_attach_recovers_geometry() {
    let rb = ring(256, 3);
    // Commit something so the recovered `committed` is non-trivial.
    rb.writer(NoWake).unwrap().try_write(b"x").unwrap();

    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    assert_eq!(raw.committed(), rb.committed());

    // The capacity of 256 was recovered too, since the
    // `InsufficientCapacity` boundary sits exactly at `frame_len(payload) >
    // 256`. A 249-byte payload makes a 264-byte record and is rejected; a
    // 248-byte payload makes a 256-byte record, which the capacity allows
    // (it is only backpressured here because the region already holds a
    // byte).
    let mut w = raw.writer(NoWake).unwrap();
    assert_eq!(
        w.try_write(&[0u8; 249]),
        Err(WriteError::InsufficientCapacity)
    );
    assert_ne!(
        w.try_write(&[0u8; 248]),
        Err(WriteError::InsufficientCapacity)
    );
}

/// A bad region is rejected by header validation rather than UB or a panic.
#[test]
fn raw_attach_bad_region_rejected() {
    // A zeroed but header-sized region has a zero magic word.
    let zeros = Backing::heap(HEADER_SIZE);
    assert_eq!(
        unsafe { RingBuffer::attach_raw(zeros.base(), zeros.len()) }.err(),
        Some(AttachError::BadMagic),
    );

    // A too-short region is rejected before any header read.
    let tiny = Backing::heap(8);
    assert_eq!(
        unsafe { RingBuffer::attach_raw(tiny.base(), tiny.len()) }.err(),
        Some(AttachError::TooSmall),
    );
}

// ----- Writer/view re-acquisition over a long-lived region -----

/// Dropping a writer and view and creating a fresh pair over the same
/// `RingBuffer` re-acquires the writer role and a reader slot, and the new
/// view starts at the live edge, never seeing the previous pair's data.
///
/// `max_readers = 1` makes the reclaim load-bearing. A leaked slot would make
/// the second `view()` fail, and its cursor would backpressure the new writer
/// forever.
#[test]
fn swap_writer_and_reader_reacquire() {
    let rb = ring(256, 1);
    let mut buf = Vec::new();

    // The first pair claims, writes and reads, then drops.
    {
        let mut w1 = rb.writer(NoWake).unwrap();
        let mut v1 = rb.view(NoWake).unwrap();
        assert_eq!(rb.reader_count(), 1);
        w1.try_write(b"occ1-a").unwrap();
        w1.try_write(b"occ1-b").unwrap();
        assert!(v1.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"occ1-a");
        // Leave "occ1-b" unread, to prove the next pair does not inherit it.
    }
    assert_eq!(rb.reader_count(), 0, "reader slot freed on drop");

    // A fresh pair re-acquires over the same region.
    let mut w2 = rb.writer(NoWake).unwrap();
    let mut v2 = rb.view(NoWake).unwrap();
    assert_eq!(
        rb.reader_count(),
        1,
        "the single reader slot was reclaimed, not exhausted"
    );
    assert!(
        !v2.try_read_into(&mut buf).unwrap(),
        "fresh view sees only new data"
    );
    w2.try_write(b"occ2").unwrap();
    assert!(v2.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"occ2");
}

/// The same cycle through `attach_raw`. Each round attaches a fresh
/// non-owning handle over a region that outlives it, claims a writer and a
/// view, and drops all three; the next round re-attaches and re-acquires the
/// freed slot.
#[test]
fn raw_attach_swap_reacquire() {
    let owner = ring(256, 1); // keeps the region alive across both rounds
    let (base, len) = owner.region();
    let mut buf = Vec::new();

    {
        let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();
        let mut w = raw.writer(NoWake).unwrap();
        let mut v = raw.view(NoWake).unwrap();
        assert_eq!(owner.reader_count(), 1);
        w.try_write(b"raw-occ1").unwrap();
        assert!(v.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"raw-occ1");
    }
    assert_eq!(owner.reader_count(), 0, "reader slot freed on drop");

    let raw2 = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();
    let mut w2 = raw2.writer(NoWake).unwrap();
    let mut v2 = raw2.view(NoWake).unwrap();
    assert_eq!(
        owner.reader_count(),
        1,
        "single reader slot reclaimed across re-attach"
    );
    assert!(
        !v2.try_read_into(&mut buf).unwrap(),
        "fresh view starts at the live edge"
    );
    w2.try_write(b"raw-occ2").unwrap();
    assert!(v2.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"raw-occ2");
}

// ----- mmap backing -----
//
// Excluded from Miri: these need a real temp directory and a real `mmap`,
// neither of which Miri provides. They used to be excluded by a feature gate
// that no longer exists.

#[test]
#[cfg(not(miri))]
fn mmap_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    let cfg = Config {
        capacity: 1024,
        max_readers: 4,
    };
    let rb = RingBuffer::create_mmap(&path, cfg).unwrap();
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    w.try_write(b"shared memory").unwrap();
    let mut buf = Vec::new();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"shared memory");

    // A second handle attaches to the same region and sees matching geometry.
    let attached = unsafe { RingBuffer::attach_mmap(&path) }.unwrap();
    assert_eq!(attached.committed(), rb.committed());
}

// ----- Corrupt length fields -----

/// A record whose length field was scribbled to `0xFFFF_FFFF` is structural
/// corruption. Both the copying and the borrowing read must return `Corrupt`
/// instead of building an out-of-bounds slice, and under Miri this proves no
/// out-of-bounds access happens.
#[test]
fn garbage_length_is_corrupt() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    let mut buf = Vec::new();

    w.try_write(&[5u8; 16]).unwrap(); // record at phys 0, len 16
    // SAFETY: phys 0 is in-bounds; headers are plain words and nothing reads
    // concurrently in this single-threaded test.
    unsafe { (rb.inner.data_ptr(0) as *mut u64).write(0xFFFF_FFFF) };
    assert_eq!(v.try_read_into(&mut buf), Err(ReadError::Corrupt));
    assert!(v.try_read().is_err());

    // Restore the true length. The record reads back intact, since the
    // cursor never moved.
    // SAFETY: as above.
    unsafe { (rb.inner.data_ptr(0) as *mut u64).write(16) };
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &[5u8; 16]);
}

// ----- View registration -----

/// `view()` returns with a stable cursor, equal to `committed` both on a
/// quiescent ring and after heavy prior write/drain traffic.
#[test]
fn view_starts_stable() {
    let rb = ring(128, 2);
    let v = rb.view(NoWake).unwrap();
    assert_eq!(v.cursor(), rb.committed());
    drop(v);

    let mut w = rb.writer(NoWake).unwrap();
    let mut d = rb.view(NoWake).unwrap();
    let mut buf = Vec::new();
    for i in 0u8..40 {
        loop {
            match w.try_write(&[i; 16]) {
                Ok(()) => break,
                Err(WriteError::WouldBlock) => {
                    assert!(d.try_read_into(&mut buf).unwrap());
                }
                Err(e) => panic!("unexpected {e:?}"),
            }
        }
    }
    let v2 = rb.view(NoWake).unwrap();
    assert_eq!(v2.cursor(), rb.committed());
}

// ----- Single-writer claim -----

/// A second writer over one region is rejected, from the same handle or a
/// clone, and dropping the first frees the claim.
#[test]
fn second_writer_rejected() {
    let rb = ring(256, 1);
    let w1 = rb.writer(NoWake).unwrap();
    assert!(rb.writer(NoWake).is_err());
    assert!(rb.clone().writer(NoWake).is_err());
    drop(w1);
    assert!(rb.writer(NoWake).is_ok());
}

/// `Writer::drop` hands the region state to the next claimer, which continues
/// the same absolute stream.
#[test]
fn writer_claim_freed_on_drop() {
    let rb = ring(256, 2);
    let mut v = rb.view(NoWake).unwrap();
    let mut buf = Vec::new();

    {
        let mut w1 = rb.writer(NoWake).unwrap();
        w1.try_write(b"first").unwrap();
    }
    let mut w2 = rb.writer(NoWake).unwrap();
    w2.try_write(b"second").unwrap();

    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"first");
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"second");
}

/// The claim lives in the region, so it is enforced across attached handles.
#[test]
fn writer_claim_shared_across_attach() {
    let rb = ring(256, 1);
    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    let w1 = rb.writer(NoWake).unwrap();
    assert!(
        raw.writer(NoWake).is_err(),
        "claim visible cross-handle"
    );
    drop(w1);
    let mut w2 = raw.writer(NoWake).unwrap();
    w2.try_write(b"raw side").unwrap();
    assert!(
        rb.writer(NoWake).is_err(),
        "claim visible in reverse"
    );
}

/// Claim/drop churn from several threads. The CAS admits exactly one writer
/// at a time, and the claim is free once every thread is done.
#[test]
fn concurrent_writer_claim_churn() {
    let threads: u64 = if cfg!(miri) { 3 } else { 8 };
    let rounds = if cfg!(miri) { 8 } else { 400 };
    let rb = ring(128, 1);
    let successes = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    let claimers: Vec<_> = (0..threads)
        .map(|t| {
            let rb = rb.clone();
            let successes = successes.clone();
            thread::spawn(move || {
                for i in 0..rounds {
                    match rb.writer(NoWake) {
                        Ok(mut w) => {
                            // No reader is registered, so the write always fits.
                            w.try_write(&(t * rounds + i).to_le_bytes()).unwrap();
                            successes.fetch_add(1, O::Relaxed);
                        }
                        Err(WriterClaimed) => thread::yield_now(),
                    }
                }
            })
        })
        .collect();
    for c in claimers {
        c.join().unwrap();
    }
    assert!(
        successes.load(O::Relaxed) > 0,
        "at least one claim succeeded"
    );
    assert!(rb.writer(NoWake).is_ok(), "claim free after churn");
}

// ----- Wrap-gap skip -----

/// A reader parked exactly on a wrap-gap start must skip it and read the
/// post-wrap record. With capacity 64, the records at 0..24 and 24..48 are
/// drained (cursor at 48), and a third 16-byte payload forces the gap
/// (high-water mark 48, record at 64..88).
#[test]
fn reader_on_gap_start_reads_through() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    let mut buf = Vec::new();

    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(v.cursor(), 48);

    w.try_write(&[3u8; 16]).unwrap(); // wraps; hwm 48, record at abs 64..88
    assert!(
        v.try_read_into(&mut buf).unwrap(),
        "gap skipped, record read"
    );
    assert_eq!(&buf[..], &[3u8; 16]);
    assert!(!v.try_read_into(&mut buf).unwrap());
}

// ----- Attach geometry validation -----

fn valid_region() -> (RingBuffer, *mut u8, usize) {
    let rb = ring(64, 2);
    let (base, len) = rb.region();
    (rb, base, len)
}

/// A backing shorter than the header's self-declared `total_size` (the
/// truncated-file shape) is rejected.
#[test]
fn attach_rejects_truncated() {
    let (_rb, base, len) = valid_region();
    assert_eq!(
        unsafe { RingBuffer::attach_raw(base, len - 8) }.err(),
        Some(AttachError::RegionTruncated),
    );
}

/// Hostile capacities (zero, not a power of two, smaller than one record
/// header, or too big for this target's `usize`) are all `BadGeometry`.
#[test]
fn attach_rejects_bad_capacity() {
    let (_rb, base, len) = valid_region();
    for bad in [0u64, 48, 4, u64::MAX] {
        // SAFETY: OFF_CAPACITY is inside the live header region.
        unsafe { (base.add(OFF_CAPACITY) as *mut u64).write(bad) };
        assert_eq!(
            unsafe { RingBuffer::attach_raw(base, len) }.err(),
            Some(AttachError::BadGeometry),
            "capacity {bad} must be rejected"
        );
    }
    // Restore, and the same region attaches cleanly again.
    // SAFETY: as above.
    unsafe { (base.add(OFF_CAPACITY) as *mut u64).write(64) };
    assert!(unsafe { RingBuffer::attach_raw(base, len) }.is_ok());
}

/// Out-of-bounds and overlapping offsets are `BadGeometry`. Covered here are
/// a reader table running into the data region, a data region past
/// `total_size`, a misaligned `data_offset`, and an offset chosen so the
/// bounds math would overflow.
#[test]
fn attach_rejects_oob_offsets() {
    let (_rb, base, len) = valid_region();
    let attach = |base, len| unsafe { RingBuffer::attach_raw(base, len) };
    let data_offset = HEADER_SIZE as u64 + 2 * READER_SLOT_SIZE as u64; // 0xC0

    // Reader table ends past the data region.
    // SAFETY (all pokes below): fixed header offsets inside the live region.
    unsafe { (base.add(OFF_READER_TABLE_OFFSET) as *mut u32).write(data_offset as u32) };
    assert_eq!(attach(base, len).err(), Some(AttachError::BadGeometry));
    unsafe { (base.add(OFF_READER_TABLE_OFFSET) as *mut u32).write(HEADER_SIZE as u32) };

    // Data region past total_size.
    unsafe { (base.add(OFF_DATA_OFFSET) as *mut u64).write(len as u64) };
    assert_eq!(attach(base, len).err(), Some(AttachError::BadGeometry));

    // Misaligned data offset.
    unsafe { (base.add(OFF_DATA_OFFSET) as *mut u64).write(data_offset + 1) };
    assert_eq!(attach(base, len).err(), Some(AttachError::BadGeometry));

    // Offset chosen to overflow `data_offset + capacity`.
    unsafe { (base.add(OFF_DATA_OFFSET) as *mut u64).write(u64::MAX - 7) };
    assert_eq!(attach(base, len).err(), Some(AttachError::BadGeometry));

    // Restore, and it attaches cleanly again.
    unsafe { (base.add(OFF_DATA_OFFSET) as *mut u64).write(data_offset) };
    assert!(attach(base, len).is_ok());
}

/// `attach_raw` with a non-8-byte-aligned base is rejected before any header
/// read.
#[test]
fn attach_rejects_misaligned() {
    let (_rb, base, len) = valid_region();
    assert_eq!(
        // SAFETY: base+1 .. base+len is still inside the live region.
        unsafe { RingBuffer::attach_raw(base.add(1), len - 1) }.err(),
        Some(AttachError::Misaligned),
    );
}

/// `attach_mmap` on a file truncated below its self-declared `total_size` (or
/// even below the header) fails cleanly instead of mapping short and reading
/// out of bounds.
#[test]
#[cfg(not(miri))]
fn attach_mmap_rejects_truncated_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    let cfg = Config {
        capacity: 1024,
        max_readers: 2,
    };
    drop(RingBuffer::create_mmap(&path, cfg).unwrap());

    let truncate = |n: u64| {
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(n)
            .unwrap();
    };

    // Below total_size but above the header.
    truncate(512);
    let err = unsafe { RingBuffer::attach_mmap(&path) }
        .err()
        .expect("truncated file must be rejected");
    assert!(
        format!("{err}").contains("exceeds the backing region"),
        "{err}"
    );

    // Below even the header.
    truncate(64);
    let err = unsafe { RingBuffer::attach_mmap(&path) }
        .err()
        .expect("truncated file must be rejected");
    assert!(
        format!("{err}").contains("shorter than the fixed header"),
        "{err}"
    );
}

// ----- View churn under a live writer -----

/// One bounded writer, one fast drainer, and a churner that repeatedly
/// registers a fresh view, borrow-reads one record with content validation,
/// and drops it. Without the registration handshake the writer's cursor scan
/// could miss the fresh claim and write past it, handing the borrow
/// overwritten bytes. With it, every borrowed record is coherent.
#[test]
fn concurrent_view_churn() {
    let n: u64 = if cfg!(miri) { 32 } else { 2_000 };
    let churn_rounds = if cfg!(miri) { 8 } else { 300 };
    let rb = ring(128, 4);
    let drainer = rb.view(NoWake).unwrap();

    let consumer = thread::spawn(move || {
        let mut v = drainer;
        let mut buf = Vec::new();
        let mut count = 0u64;
        while count < n {
            match v.try_read_into(&mut buf) {
                Ok(true) => {
                    let lo = u64::from_le_bytes(buf[..8].try_into().unwrap());
                    let hi = u64::from_le_bytes(buf[8..16].try_into().unwrap());
                    assert_eq!(lo, hi, "drained record torn");
                    assert!(lo < n);
                    count += 1;
                }
                Ok(false) => thread::yield_now(),
                Err(e) => panic!("drainer must never error: {e:?}"),
            }
        }
    });

    let churner = {
        let rb = rb.clone();
        thread::spawn(move || {
            for _ in 0..churn_rounds {
                let Ok(mut v) = rb.view(NoWake) else {
                    thread::yield_now();
                    continue;
                };
                match v.try_read() {
                    Ok(Some(grant)) => {
                        assert_eq!(grant.len(), 16);
                        let lo = u64::from_le_bytes(grant[..8].try_into().unwrap());
                        let hi = u64::from_le_bytes(grant[8..16].try_into().unwrap());
                        assert_eq!(lo, hi, "borrowed record torn");
                        assert!(lo < n);
                    }
                    Ok(None) => {}
                    Err(e) => panic!("churn view must never error: {e:?}"),
                }
            }
        })
    };

    let producer = {
        let rb = rb.clone();
        thread::spawn(move || {
            let mut w = rb.writer(NoWake).unwrap();
            for i in 0..n {
                let mut payload = [0u8; 16];
                payload[..8].copy_from_slice(&i.to_le_bytes());
                payload[8..].copy_from_slice(&i.to_le_bytes());
                loop {
                    match w.try_write(&payload) {
                        Ok(()) => break,
                        Err(WriteError::WouldBlock) => thread::yield_now(),
                        Err(e) => panic!("unexpected {e:?}"),
                    }
                }
            }
        })
    };

    producer.join().unwrap();
    churner.join().unwrap();
    consumer.join().unwrap();
}

// ----- Owner reclamation (dead-process cleanup) -----

/// Reclaiming a dead reader's pinned cursor unblocks a backpressured writer,
/// and the freed slot is claimable by a fresh view.
#[test]
fn reclaim_frees_dead_reader() {
    const DEAD_PID: u64 = 4242;
    let rb = ring(64, 2);
    let mut w = rb.writer(NoWake).unwrap();

    // Plant a foreign reader claim directly, as if another process
    // registered a view at position 0 and then died: a pinned cursor with a
    // dead owner and no local `View` object at all.
    rb.inner.slot_cursor(0).store(0, Release);
    rb.inner.slot_owner(0).store(DEAD_PID, Release);

    // The dead cursor backpressures the writer once a lap fills.
    while w.try_write(&[0u8; 16]).is_ok() {}
    assert_eq!(w.try_write(&[0u8; 16]), Err(WriteError::WouldBlock));

    assert_eq!(rb.reader_count(), 1);
    // SAFETY: no process with this pid holds the slot; nothing stores through it.
    unsafe { rb.reclaim_owner(DEAD_PID) };
    assert_eq!(rb.reader_count(), 0, "dead cursor freed");

    w.try_write(&[1u8; 16]).expect("writer unblocked");
    // The freed slot re-registers cleanly (the handshake still converges).
    let mut v2 = rb.view(NoWake).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    let g = v2.try_read().unwrap().expect("fresh view reads new data");
    assert_eq!(&g[..], &[2u8; 16]);
}

/// A dead writer's claim is reclaimed by owner tag, and a new writer
/// continues the same absolute stream. The dead owner is a *foreign* pid —
/// reclaiming one's own pid would also free this process's live views,
/// which the reclaim contract forbids.
#[test]
fn reclaim_frees_dead_writer_claim() {
    const DEAD_PID: u64 = 4242;
    let rb = ring(256, 2);
    let mut v = rb.view(NoWake).unwrap();
    {
        let mut w = rb.writer(NoWake).unwrap();
        w.try_write(b"before").unwrap();
    }
    // Re-plant the (now free) claim as if a foreign process took it and died.
    rb.inner.writer_claim().store(DEAD_PID, Release);
    assert!(rb.writer(NoWake).is_err(), "claim held by the dead");

    // SAFETY: no live process holds this claim; nothing stores through it.
    unsafe { rb.reclaim_owner(DEAD_PID) };
    let mut w2 = rb.writer(NoWake).unwrap();
    w2.try_write(b"after").unwrap();

    let mut buf = Vec::new();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"before");
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"after");
}

/// Reclaim is scoped to the given owner tag: a reader slot and a writer
/// claim held by some other process are untouched.
#[test]
fn reclaim_skips_other_owners() {
    let rb = ring(256, 2);
    let v = rb.view(NoWake).unwrap();
    // Plant a foreign owner on the view's slot (the first free one) and a
    // foreign writer claim, as if another process held both.
    rb.inner.slot_owner(0).store(999_999_999, Release);
    rb.inner.writer_claim().store(999_999_999, Release);

    // SAFETY: reclaim by our pid; the planted foreign tags must not match.
    unsafe { rb.reclaim_owner(std::process::id() as u64) };
    assert_eq!(rb.reader_count(), 1, "foreign reader survives");
    assert!(
        rb.writer(NoWake).is_err(),
        "foreign writer claim survives"
    );
    drop(v);
}

// ----- Async paths -----
//
// Excluded from Miri, which cannot drive the executor. These are the only
// tests that reach `View::read`, `View::read_into` and `Notifier`; everything
// above deliberately sticks to `try_*` so it stays Miri-checkable, which left
// the awaiting paths — and `port.rs`'s `view.read().await` in `metor-fsw-2` —
// covered only indirectly.
//
// Each spawns the peer and yields first, so the waiting side is already armed
// when the commit lands. That is what puts the wake path, rather than a record
// that happened to be ready, on the tested route.

#[cfg(not(miri))]
#[stellarator::test]
async fn read_into_awaits_a_commit() {
    let ring = ring(64, 2);
    let notifier = Notifier::default();
    let mut w = ring.writer(notifier.clone()).unwrap();
    let mut v = ring.view(notifier.clone()).unwrap();
    assert_eq!(v.committed(), 0);

    let reader = stellarator::spawn(async move {
        let mut buf = Vec::new();
        v.read_into(&mut buf).await.expect("never corrupt");
        (buf, v.cursor())
    });

    stellarator::yield_now().await;
    w.try_write(b"awaited!").unwrap();

    let (buf, cursor) = reader.await.unwrap();
    assert_eq!(&buf[..], b"awaited!");
    assert_eq!(cursor, frame_len(8) as u64);
}

#[cfg(not(miri))]
#[stellarator::test]
async fn read_awaits_a_commit_and_the_grant_consumes_it() {
    let ring = ring(64, 2);
    let notifier = Notifier::default();
    let mut w = ring.writer(notifier.clone()).unwrap();
    let mut v = ring.view(notifier.clone()).unwrap();

    let writer = stellarator::spawn(async move {
        stellarator::yield_now().await;
        w.try_write(b"borrowed").unwrap();
    });

    let grant = v.read().await.expect("never corrupt");
    assert_eq!(&grant[..], b"borrowed");
    drop(grant);
    // Dropping the grant is what advances the cursor past the record.
    assert_eq!(v.cursor(), frame_len(8) as u64);
    assert_eq!(v.committed(), frame_len(8) as u64);
    writer.await.unwrap();
}

#[cfg(not(miri))]
#[stellarator::test]
async fn read_into_streams_records_in_order() {
    let ring = ring(64, 2);
    let notifier = Notifier::default();
    let mut w = ring.writer(notifier.clone()).unwrap();
    let mut v = ring.view(notifier.clone()).unwrap();

    let reader = stellarator::spawn(async move {
        let mut seen = Vec::new();
        let mut buf = Vec::new();
        for _ in 0..4u8 {
            v.read_into(&mut buf).await.expect("never corrupt");
            seen.push(buf[0]);
        }
        seen
    });

    // One at a time, so the reader waits on every one of them rather than
    // draining a backlog.
    for n in 0..4u8 {
        stellarator::yield_now().await;
        w.try_write(&[n; 8]).unwrap();
    }

    assert_eq!(reader.await.unwrap(), vec![0, 1, 2, 3]);
}

#[cfg(not(miri))]
#[stellarator::test]
async fn read_returns_a_ready_record_without_waiting() {
    let ring = ring(64, 2);
    let notifier = Notifier::default();
    let mut w = ring.writer(notifier.clone()).unwrap();
    let mut v = ring.view(notifier).unwrap();

    // Committed before the first poll, so the loop returns on its first pass
    // and never arms the wait.
    w.try_write(b"ready").unwrap();
    let mut buf = Vec::new();
    v.read_into(&mut buf).await.expect("never corrupt");
    assert_eq!(&buf[..], b"ready");
}

/// [`NoWake`] resolves as soon as it is polled, which is what makes the async
/// paths degenerate to caller-driven polling under it. Called directly: a view
/// using it would spin without yielding, so it never reaches the wait in a
/// test that has data to find.
#[cfg(not(miri))]
#[stellarator::test]
async fn no_wake_resolves_immediately() {
    let mut polled = false;
    NoWake
        .wait_until(|| {
            polled = true;
            true
        })
        .await;
    assert!(polled, "NoWake must poll its readiness predicate");
}

/// `create_raw` formats a ring into memory the caller owns, and the result is
/// a normal ring: a record written through it reads back through a fresh
/// `attach_raw` over the same bytes. This is the path a wasm host uses to put
/// a region inside guest linear memory.
#[test]
fn create_raw_formats_a_caller_owned_region() {
    let cfg = Config {
        capacity: 1024,
        max_readers: 4,
    };
    let len = region_len(&cfg);
    // Over-allocate so the "region may be larger than the ring" case is live.
    let mut region = vec![0u8; len + 64];
    let base = region.as_mut_ptr();

    // SAFETY: `region` outlives every handle below and nothing else reads it.
    let ring = unsafe { RingBuffer::create_raw(base, region.len(), cfg) }.expect("formats");
    let mut writer = ring.writer(NoWake).expect("sole writer");

    // A second attach over the same bytes is what the guest does after the
    // host formats the region: it reads the geometry back out of the header
    // and registers its own reader slot. Register before writing, since a
    // slot joins at the live edge rather than replaying a backlog.
    // SAFETY: same live region, still owned by `region`.
    let attached = unsafe { RingBuffer::attach_raw(base, len) }.expect("attaches");
    let mut view = attached.view(NoWake).expect("reader slot");

    writer.try_write(&[1u8; 16]).expect("record fits");
    assert_eq!(&view.try_latest().unwrap().expect("the record")[..], &[1u8; 16]);
}

/// The two rejections `create_raw` owns: a base the ring cannot attach to, and
/// a region too small for the configured geometry.
#[test]
fn create_raw_rejects_bad_regions() {
    let cfg = Config {
        capacity: 1024,
        max_readers: 4,
    };
    let len = region_len(&cfg);
    let mut region = vec![0u8; len + 16];
    let base = region.as_mut_ptr();

    // SAFETY: a live region; the call fails before formatting anything.
    let misaligned = unsafe { RingBuffer::create_raw(base.wrapping_add(1), len, cfg) };
    assert!(matches!(misaligned, Err(AttachError::Misaligned)));

    // SAFETY: same, one byte short of the computed layout.
    let small = unsafe { RingBuffer::create_raw(base, len - 1, cfg) };
    assert!(matches!(small, Err(AttachError::TooSmall)));
}
