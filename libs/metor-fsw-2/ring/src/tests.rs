//! The synchronous tests use only the `try_*` APIs plus `std::thread`, so
//! they run under Miri (which cannot drive the async runtime) and exercise
//! the unsafe pointer and atomic paths for provenance, leaks, and data races.
//! `MIRI.md` in the crate root describes the coverage. Loop bounds shrink
//! under `cfg!(miri)`.

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
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();

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
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();
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
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut a = rb.view(NoWake, NoWake).unwrap();
    let mut b = rb.view(NoWake, NoWake).unwrap();

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
    assert_eq!(rb.reader_count(), 0);

    let a = rb.view(NoWake, NoWake).unwrap();
    let b = rb.view(NoWake, NoWake).unwrap();
    assert_eq!(rb.reader_count(), 2);
    assert_eq!(rb.view(NoWake, NoWake).err(), Some(FullReaderTable));

    drop(b);
    assert_eq!(rb.reader_count(), 1);
    // The freed slot is reused.
    let _c = rb.view(NoWake, NoWake).unwrap();
    assert_eq!(rb.reader_count(), 2);
    drop(a);
    assert_eq!(rb.reader_count(), 1);
}

// ----- Backpressure / borrow semantics -----

/// The writer backpressures with `WouldBlock` rather than overwrite a reader,
/// and freeing space lets the next write through.
#[test]
fn backpressure() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();
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
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();

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
    let mut w = rb.writer(NoWake, NoWake).unwrap();
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
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();

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
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();

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
    let v = rb.view(NoWake, NoWake).unwrap();

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
            let mut w = rb.writer(NoWake, NoWake).unwrap();
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
            let mut w = rb.writer(NoWake, NoWake).unwrap();
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
                    if let Ok(mut v) = rb.view(NoWake, NoWake) {
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

// ----- Async (needs the runtime; skipped under Miri) -----

/// `write().await` suspends when the buffer fills and resumes as the reader
/// drains; the full stream arrives in order.
#[cfg(all(feature = "async", not(miri)))]
#[stellarator::test]
async fn wait_writer_backpressures() {
    let rb = ring(64, 1);
    let data = Notifier::default();
    let space = Notifier::default();
    let mut view = rb.view(data.clone(), space.clone()).unwrap();
    let n: u64 = 50;

    let writer = {
        let rb = rb.clone();
        let data = data.clone();
        let space = space.clone();
        stellarator::spawn(async move {
            let mut w = rb.writer(data, space).unwrap();
            for i in 0..n {
                w.write(&i.to_le_bytes()).await.unwrap();
            }
        })
    };

    let mut buf = Vec::new();
    let mut got = Vec::with_capacity(n as usize);
    for _ in 0..n {
        view.read_into(&mut buf).await.unwrap();
        got.push(u64::from_le_bytes(buf[..8].try_into().unwrap()));
    }
    let _ = writer.await;

    let expected: Vec<u64> = (0..n).collect();
    assert_eq!(got, expected);
}

/// The borrowing twin. `read().await` suspends for data and hands out a
/// grant, and dropping the grant frees space for the suspended writer.
#[cfg(all(feature = "async", not(miri)))]
#[stellarator::test]
async fn wait_reader_borrows() {
    let rb = ring(64, 1);
    let data = Notifier::default();
    let space = Notifier::default();
    let mut view = rb.view(data.clone(), space.clone()).unwrap();
    let n: u64 = 50;

    let writer = {
        let rb = rb.clone();
        let data = data.clone();
        let space = space.clone();
        stellarator::spawn(async move {
            let mut w = rb.writer(data, space).unwrap();
            for i in 0..n {
                w.write(&i.to_le_bytes()).await.unwrap();
            }
        })
    };

    let mut got = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let grant = view.read().await.unwrap();
        got.push(u64::from_le_bytes(grant[..8].try_into().unwrap()));
    }
    let _ = writer.await;

    let expected: Vec<u64> = (0..n).collect();
    assert_eq!(got, expected);
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
        let mut vr = raw.view(NoWake, NoWake).unwrap();
        let mut wb = rb.writer(NoWake, NoWake).unwrap();
        wb.try_write(b"box->raw").unwrap();
        assert!(vr.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"box->raw");
    }
    {
        let mut vb = rb.view(NoWake, NoWake).unwrap();
        let mut wr = raw.writer(NoWake, NoWake).unwrap();
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
    rb.writer(NoWake, NoWake).unwrap().try_write(b"x").unwrap();

    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    assert_eq!(raw.committed(), rb.committed());

    // The capacity of 256 was recovered too, since the
    // `InsufficientCapacity` boundary sits exactly at `frame_len(payload) >
    // 256`. A 249-byte payload makes a 264-byte record and is rejected; a
    // 248-byte payload makes a 256-byte record, which the capacity allows
    // (it is only backpressured here because the region already holds a
    // byte).
    let mut w = raw.writer(NoWake, NoWake).unwrap();
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
        let mut w1 = rb.writer(NoWake, NoWake).unwrap();
        let mut v1 = rb.view(NoWake, NoWake).unwrap();
        assert_eq!(rb.reader_count(), 1);
        w1.try_write(b"occ1-a").unwrap();
        w1.try_write(b"occ1-b").unwrap();
        assert!(v1.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"occ1-a");
        // Leave "occ1-b" unread, to prove the next pair does not inherit it.
    }
    assert_eq!(rb.reader_count(), 0, "reader slot freed on drop");

    // A fresh pair re-acquires over the same region.
    let mut w2 = rb.writer(NoWake, NoWake).unwrap();
    let mut v2 = rb.view(NoWake, NoWake).unwrap();
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
        let mut w = raw.writer(NoWake, NoWake).unwrap();
        let mut v = raw.view(NoWake, NoWake).unwrap();
        assert_eq!(owner.reader_count(), 1);
        w.try_write(b"raw-occ1").unwrap();
        assert!(v.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"raw-occ1");
    }
    assert_eq!(owner.reader_count(), 0, "reader slot freed on drop");

    let raw2 = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();
    let mut w2 = raw2.writer(NoWake, NoWake).unwrap();
    let mut v2 = raw2.view(NoWake, NoWake).unwrap();
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

// ----- mmap backing (feature-gated) -----

#[cfg(feature = "mmap")]
#[test]
fn mmap_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    let cfg = Config {
        capacity: 1024,
        max_readers: 4,
    };
    let rb = RingBuffer::create_mmap(&path, cfg).unwrap();
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();

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
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();
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
    let v = rb.view(NoWake, NoWake).unwrap();
    assert_eq!(v.cursor(), rb.committed());
    drop(v);

    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut d = rb.view(NoWake, NoWake).unwrap();
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
    let v2 = rb.view(NoWake, NoWake).unwrap();
    assert_eq!(v2.cursor(), rb.committed());
}

// ----- Single-writer claim -----

/// A second writer over one region is rejected, from the same handle or a
/// clone, and dropping the first frees the claim.
#[test]
fn second_writer_rejected() {
    let rb = ring(256, 1);
    let w1 = rb.writer(NoWake, NoWake).unwrap();
    assert!(rb.writer(NoWake, NoWake).is_err());
    assert!(rb.clone().writer(NoWake, NoWake).is_err());
    drop(w1);
    assert!(rb.writer(NoWake, NoWake).is_ok());
}

/// `Writer::drop` hands the region state to the next claimer, which continues
/// the same absolute stream.
#[test]
fn writer_claim_freed_on_drop() {
    let rb = ring(256, 2);
    let mut v = rb.view(NoWake, NoWake).unwrap();
    let mut buf = Vec::new();

    {
        let mut w1 = rb.writer(NoWake, NoWake).unwrap();
        w1.try_write(b"first").unwrap();
    }
    let mut w2 = rb.writer(NoWake, NoWake).unwrap();
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

    let w1 = rb.writer(NoWake, NoWake).unwrap();
    assert!(
        raw.writer(NoWake, NoWake).is_err(),
        "claim visible cross-handle"
    );
    drop(w1);
    let mut w2 = raw.writer(NoWake, NoWake).unwrap();
    w2.try_write(b"raw side").unwrap();
    assert!(
        rb.writer(NoWake, NoWake).is_err(),
        "claim visible in reverse"
    );
}

/// A leaked claim (the word set with no live `Writer`, exactly what a crash
/// leaves behind) blocks `writer()` until it is force-released.
#[test]
fn force_release_writer_reclaims() {
    let rb = ring(256, 1);
    rb.inner.writer_claim().store(1, Release);
    assert!(rb.writer(NoWake, NoWake).is_err());

    // SAFETY: no live writer exists for this region (the claim was planted).
    unsafe { rb.force_release_writer() };
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    w.try_write(b"reclaimed").unwrap();
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
                    match rb.writer(NoWake, NoWake) {
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
    assert!(rb.writer(NoWake, NoWake).is_ok(), "claim free after churn");
}

// ----- Wrap-gap skip -----

/// A reader parked exactly on a wrap-gap start must skip it and read the
/// post-wrap record. With capacity 64, the records at 0..24 and 24..48 are
/// drained (cursor at 48), and a third 16-byte payload forces the gap
/// (high-water mark 48, record at 64..88).
#[test]
fn reader_on_gap_start_reads_through() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();
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
#[cfg(feature = "mmap")]
#[test]
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
    let drainer = rb.view(NoWake, NoWake).unwrap();

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
                let Ok(mut v) = rb.view(NoWake, NoWake) else {
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
            let mut w = rb.writer(NoWake, NoWake).unwrap();
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
