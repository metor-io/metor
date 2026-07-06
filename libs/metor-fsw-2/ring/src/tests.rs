//! Tests for the ring buffer.
//!
//! The synchronous tests use only `try_*` APIs + `std::thread`, so they run
//! under Miri (which cannot drive the async runtime). They exercise the unsafe
//! pointer/atomic paths so Miri can check provenance, leaks, and data races.
//! See `libs/db/MIRI.md`; this crate follows the same strategy (`UnsafeCell`
//! backing, sync tests, Tree Borrows). Loop bounds shrink under `cfg!(miri)`.

use super::*;

// Region offsets of the header fields the corruption tests scribble on
// (`RegionHeader` sits at region offset 0).
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

/// Small power-of-two capacity with a payload that does not divide it, forcing
/// an 8-byte wrap gap. The reader (kept up, so the writer never blocks) must
/// skip the gap and reconstruct each message.
#[test]
fn wraparound_aligned() {
    // cap 64, payload 16 -> record 24. 24,48 fit; the third would straddle
    // (48+24 > 64), so the writer leaves a 16-byte gap and wraps to offset 0.
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
    // A gap must have been left at least once (cursor advanced past raw bytes).
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

/// The writer backpressures (`WouldBlock`) rather than overwrite a reader;
/// freeing space lets the next write through. No data is ever lost.
#[test]
fn backpressure() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();
    let mut buf = Vec::new();

    // record = 24; two fit (48 <= 64), the third would overwrite the idle reader.
    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    assert_eq!(w.try_write(&[3u8; 16]), Err(WriteError::WouldBlock));

    // The reader was never overwritten, so it reads the first record correctly.
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &[1u8; 16]);

    // Space freed -> the write now succeeds.
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
    // 64-byte payload -> record 72 > capacity 64.
    assert_eq!(w.try_write(&[0u8; 64]), Err(WriteError::InsufficientCapacity));
}

// ----- try_latest: the latest-wins read -----

/// `try_latest` consumes every older record, pins the newest (re-served across
/// calls with no new data), and follows the live edge as records land.
#[test]
fn latest_pins_newest() {
    let rb = ring(256, 1);
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();

    assert!(
        v.try_latest().unwrap().is_none(),
        "no record committed yet"
    );

    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    w.try_write(&[3u8; 16]).unwrap();

    // Newest wins; the two older records are consumed.
    assert_eq!(&v.try_latest().unwrap().expect("newest")[..], &[3u8; 16]);
    // No new data: the pinned record is served again.
    assert_eq!(&v.try_latest().unwrap().expect("re-served")[..], &[3u8; 16]);

    w.try_write(&[4u8; 16]).unwrap();
    assert_eq!(&v.try_latest().unwrap().expect("follows the edge")[..], &[4u8; 16]);
}

/// The pin is load-bearing: the writer cannot reclaim the pinned record's bytes
/// (`WouldBlock`), and the next `try_latest` moves the pin so the writer
/// proceeds. Geometry (cap 64, record 24): after two writes the reader pins the
/// record at abs 24..48; the third write wraps (gap at 48, record abs 64..88 =
/// phys 0..24) and fits, but the fourth (phys 24..48) would overwrite the pin.
#[test]
fn latest_pin_backpressures_writer() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake, NoWake).unwrap();
    let mut v = rb.view(NoWake, NoWake).unwrap();

    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    assert_eq!(&v.try_latest().unwrap().expect("pin record 2")[..], &[2u8; 16]);

    w.try_write(&[3u8; 16]).unwrap(); // wraps; does not touch the pin
    assert_eq!(
        w.try_write(&[4u8; 16]),
        Err(WriteError::WouldBlock),
        "the pinned record's bytes are protected"
    );

    // The reader moves its pin to the newest record, freeing the old one...
    assert_eq!(&v.try_latest().unwrap().expect("pin record 3")[..], &[3u8; 16]);
    // ...and the blocked write now goes through.
    w.try_write(&[4u8; 16]).unwrap();
    assert_eq!(&v.try_latest().unwrap().expect("record 4")[..], &[4u8; 16]);
}

// ----- Concurrency / Miri data-race coverage -----

use std::sync::atomic::{AtomicBool, Ordering as O};
use std::thread;

/// Delivery is guaranteed: a writer that spins on `WouldBlock` and a reader
/// draining concurrently must reconstruct the full ordered stream with zero
/// loss (the disruptor's invariant). Also Miri race coverage for the plain
/// read/write path (race-free via backpressure + `committed`).
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

/// Register/drop views from several threads while a writer runs. Stresses the
/// CAS slot claim and the wait-free slot free; at the end every slot is free.
/// A live churner view can backpressure the writer, so the writer tolerates
/// `WouldBlock` (that is the mode's contract, not a failure).
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

/// `write().await` must suspend when the buffer fills and resume as the reader
/// drains, losing nothing. A small capacity guarantees real backpressure; the
/// cooperative single-threaded executor ping-pongs writer and reader.
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

/// The borrowing twin: `read().await` suspends for data and hands out a grant;
/// dropping the grant frees space for the suspended writer.
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

/// Same-process attach round-trip (the dlopen scenario): a second, non-owning
/// `RingBuffer` reconstructed over the SAME region a heap-backed one allocated
/// sees the identical atomics — a record
/// written through one is read through the other, in both directions. Sync-only,
/// so it runs under Miri (provenance/leak/race check of the raw attach path).
#[test]
fn raw_attach_same_process_roundtrip() {
    let rb = ring(1024, 4);
    let (base, len) = rb.region();
    // Attach a non-owning handle over the SAME bytes `rb` owns.
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    let mut buf = Vec::new();

    // Box-side writer -> Raw-side view.
    {
        let mut vr = raw.view(NoWake, NoWake).unwrap();
        let mut wb = rb.writer(NoWake, NoWake).unwrap();
        wb.try_write(b"box->raw").unwrap();
        assert!(vr.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"box->raw");
    }
    // Raw-side writer -> Box-side view (same region, identical atomics).
    {
        let mut vb = rb.view(NoWake, NoWake).unwrap();
        let mut wr = raw.writer(NoWake, NoWake).unwrap();
        wr.try_write(b"raw->box").unwrap();
        assert!(vb.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"raw->box");
    }
}

/// Geometry is recovered from the region header, not passed to `attach_raw`:
/// committed position and capacity both match the original handle.
#[test]
fn raw_attach_recovers_geometry() {
    let rb = ring(256, 3);
    // Commit something so the recovered `committed` is non-trivial.
    rb.writer(NoWake, NoWake).unwrap().try_write(b"x").unwrap();

    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    // Committed came from the header region, not from arguments.
    assert_eq!(raw.committed(), rb.committed());

    // Capacity (256) recovered too: the `InsufficientCapacity` boundary is
    // exactly `frame_len(payload) > 256`, independent of how much is committed.
    // 249 -> record 264 (rejected); 248 -> record 256 (capacity-allowed, only
    // backpressured here because the region already holds a byte).
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

/// A bad region is rejected by header validation (the same `AttachError`
/// `attach_mmap` would yield) rather than UB or a panic: a zeroed (no magic)
/// region and a region too small to hold the header both fail cleanly.
#[test]
fn raw_attach_bad_region_rejected() {
    // Zeroed but header-sized region: magic word is 0 -> BadMagic.
    let zeros = Backing::heap(HEADER_SIZE);
    assert_eq!(
        unsafe { RingBuffer::attach_raw(zeros.base(), zeros.len()) }.err(),
        Some(AttachError::BadMagic),
    );

    // Too-short region: guarded before any header read, so no out-of-bounds load.
    let tiny = Backing::heap(8);
    assert_eq!(
        unsafe { RingBuffer::attach_raw(tiny.base(), tiny.len()) }.err(),
        Some(AttachError::TooSmall),
    );
}

// ----- slot-swap re-acquisition (the Load -> Stop -> Load cycle) -----

/// Slot-swap at the ring layer: dropping a writer+view and creating a fresh pair
/// over the SAME `RingBuffer` re-acquires the writer role and a reader slot, and
/// the new view starts at the live edge (it never sees the previous occupant's
/// data). This is exactly the Load -> Stop -> Load cycle a coordinator slot runs;
/// it works with no ring change because `View::drop` frees its slot and a
/// `Writer` holds no claim. `max_readers = 1` makes reclaim load-bearing: if the
/// slot were leaked, the second `view()` would return `FullReaderTable` — and
/// the leaked cursor would backpressure the new writer forever.
#[test]
fn swap_writer_and_reader_reacquire() {
    let rb = ring(256, 1);
    let mut buf = Vec::new();

    // Occupant 1: claim, write+read, then drop the pair (the slot teardown).
    {
        let mut w1 = rb.writer(NoWake, NoWake).unwrap();
        let mut v1 = rb.view(NoWake, NoWake).unwrap();
        assert_eq!(rb.reader_count(), 1);
        w1.try_write(b"occ1-a").unwrap();
        w1.try_write(b"occ1-b").unwrap();
        assert!(v1.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"occ1-a");
        // leave "occ1-b" unread, to prove occupant 2 does not inherit it.
    }
    assert_eq!(rb.reader_count(), 0, "occupant 1's reader slot freed on drop");

    // Occupant 2: a fresh pair re-acquires over the same region.
    let mut w2 = rb.writer(NoWake, NoWake).unwrap();
    let mut v2 = rb.view(NoWake, NoWake).unwrap();
    assert_eq!(
        rb.reader_count(),
        1,
        "the single reader slot was reclaimed, not exhausted"
    );
    // The new view starts at the live edge: occupant 1's unread tail is invisible.
    assert!(
        !v2.try_read_into(&mut buf).unwrap(),
        "fresh view sees only post-attach data"
    );
    w2.try_write(b"occ2").unwrap();
    assert!(v2.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"occ2");
}

/// The occupant-side swap a dlopen'd `.so` performs: each Load `attach_raw`s a
/// fresh non-owning `RingBuffer` over the host region, claims writer+view, and on
/// `fsw_destroy` drops them (and the handle); a later Load re-attaches and
/// re-acquires the freed reader slot over the SAME host-owned region. Sync-only,
/// so Miri checks the reclaim path's provenance/leaks under reuse.
#[test]
fn raw_attach_swap_reacquire() {
    let host = ring(256, 1); // host owns the region for the whole "mission"
    let (base, len) = host.region();
    let mut buf = Vec::new();

    // Occupant 1: attach, claim, use, then drop everything (fsw_destroy).
    {
        let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();
        let mut w = raw.writer(NoWake, NoWake).unwrap();
        let mut v = raw.view(NoWake, NoWake).unwrap();
        assert_eq!(host.reader_count(), 1);
        w.try_write(b"raw-occ1").unwrap();
        assert!(v.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"raw-occ1");
    }
    assert_eq!(
        host.reader_count(),
        0,
        "raw occupant's reader slot freed on drop"
    );

    // Occupant 2: a fresh attach re-acquires the reclaimed slot over the same region.
    let raw2 = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();
    let mut w2 = raw2.writer(NoWake, NoWake).unwrap();
    let mut v2 = raw2.view(NoWake, NoWake).unwrap();
    assert_eq!(
        host.reader_count(),
        1,
        "single reader slot reclaimed across raw re-attach"
    );
    assert!(
        !v2.try_read_into(&mut buf).unwrap(),
        "fresh raw view starts at the live edge"
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

// ----- R3/R6: garbage length is bounded, never an OOB copy/borrow -----

/// A record whose length field was scribbled to `0xFFFF_FFFF` is structural
/// corruption (the writer can never lap a reader, so nothing in-crate can
/// explain it) — both the copying read and the borrowing read must return
/// `Corrupt` instead of building an out-of-bounds slice. The u64 straddle
/// check bounds the length before any pointer math (on a 32-bit target
/// `frame_len(0xFFFF_FFFF)` would wrap `usize`); under Miri this proves no
/// out-of-bounds access.
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

    // Restore the true length: the record reads back intact (cursor never moved).
    // SAFETY: as above.
    unsafe { (rb.inner.data_ptr(0) as *mut u64).write(16) };
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &[5u8; 16]);
}

// ----- R6: view registration -----

/// `view()` returns with a *stable* cursor: equal to `committed` both on a
/// quiescent ring and after heavy prior write/drain traffic (exercising the
/// stabilization loop's convergence on the cold path).
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

// ----- R7: single-writer claim -----

/// The second writer over one region is rejected, from the same handle or a
/// clone; dropping the first frees the claim for a successor.
#[test]
fn second_writer_rejected() {
    let rb = ring(256, 1);
    let w1 = rb.writer(NoWake, NoWake).unwrap();
    assert!(rb.writer(NoWake, NoWake).is_err());
    assert!(rb.clone().writer(NoWake, NoWake).is_err());
    drop(w1);
    assert!(rb.writer(NoWake, NoWake).is_ok());
}

/// `Writer::drop` releases the claim and hands the region state to the next
/// claimer: the successor continues the same absolute stream.
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

/// The claim lives in the region, so it is enforced across attach handles: a
/// raw-attached handle over the same bytes sees the box handle's claim, and
/// vice versa once the first writer drops.
#[test]
fn writer_claim_shared_across_attach() {
    let rb = ring(256, 1);
    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    let w1 = rb.writer(NoWake, NoWake).unwrap();
    assert!(raw.writer(NoWake, NoWake).is_err(), "claim visible cross-handle");
    drop(w1);
    let mut w2 = raw.writer(NoWake, NoWake).unwrap();
    w2.try_write(b"raw side").unwrap();
    assert!(rb.writer(NoWake, NoWake).is_err(), "claim visible in reverse");
}

/// Crash reclamation escape hatch: a leaked claim (as a crashed process leaves
/// it — the word set, no live `Writer`) blocks `writer()` until the supervisor
/// force-releases it. The claim word is set directly, which is exactly the
/// region state a crash leaves behind.
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

/// Claim/drop churn from several threads: the CAS admits exactly one writer at a
/// time (Miri race coverage for the claim handoff), and the claim is always free
/// once every thread is done.
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
                            // No reader is registered, so the ring recycles
                            // freely and the write always fits.
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
    assert!(successes.load(O::Relaxed) > 0, "at least one claim succeeded");
    assert!(rb.writer(NoWake, NoWake).is_ok(), "claim free after churn");
}

// ----- B6: a reader parked on a wrap-gap start reads through the gap -----

/// cap 64: records 0..24 and 24..48 are drained (cursor = 48); a third 16-byte
/// payload forces the wrap gap (hwm = 48, record at 64..88). The reader parked
/// exactly on the gap start must skip it and read the post-wrap record — the
/// gap bytes were never data.
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

    w.try_write(&[3u8; 16]).unwrap(); // wraps: hwm = 48, record at abs 64..88
    assert!(v.try_read_into(&mut buf).unwrap(), "gap skipped, record read");
    assert_eq!(&buf[..], &[3u8; 16]);
    assert!(!v.try_read_into(&mut buf).unwrap());
}

// ----- R4: attach geometry validation -----

/// A handle to a valid region plus a byte-poke helper for corrupting its header.
fn valid_region() -> (RingBuffer, *mut u8, usize) {
    let rb = ring(64, 2);
    let (base, len) = rb.region();
    (rb, base, len)
}

/// A region whose backing is shorter than the header's self-declared
/// `total_size` (the truncated-file shape) is rejected as `RegionTruncated`.
#[test]
fn attach_rejects_truncated() {
    let (_rb, base, len) = valid_region();
    assert_eq!(
        unsafe { RingBuffer::attach_raw(base, len - 8) }.err(),
        Some(AttachError::RegionTruncated),
    );
}

/// Hostile capacities — zero, non-power-of-two, smaller than one record header,
/// or too big for this target's `usize` — are all `BadGeometry`.
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
    // Restore: the same region attaches cleanly again.
    // SAFETY: as above.
    unsafe { (base.add(OFF_CAPACITY) as *mut u64).write(64) };
    assert!(unsafe { RingBuffer::attach_raw(base, len) }.is_ok());
}

/// Out-of-bounds / overlapping offsets are `BadGeometry`: a reader table running
/// into the data region, a data region past `total_size`, a misaligned
/// `data_offset`, and an offset chosen so the bounds math would overflow.
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

    // Offset chosen to overflow `data_offset + capacity` (checked math catches it).
    unsafe { (base.add(OFF_DATA_OFFSET) as *mut u64).write(u64::MAX - 7) };
    assert_eq!(attach(base, len).err(), Some(AttachError::BadGeometry));

    // Restore: attaches cleanly again.
    unsafe { (base.add(OFF_DATA_OFFSET) as *mut u64).write(data_offset) };
    assert!(attach(base, len).is_ok());
}

/// `attach_raw` with a non-8-byte-aligned base is rejected before any header
/// read (the raw path takes an arbitrary pointer; mmap/Box are aligned by
/// construction).
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
/// even below the header) fails cleanly instead of mapping short and reading out
/// of bounds — the guard the shared `read_header` path adds to mmap.
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

    // Below total_size but above the header: RegionTruncated.
    truncate(512);
    let err = unsafe { RingBuffer::attach_mmap(&path) }
        .err()
        .expect("truncated file must be rejected");
    // The io::Error wraps the AttachError (its Display, no longer a Debug dump).
    assert!(format!("{err}").contains("exceeds the backing region"), "{err}");

    // Below even the header: TooSmall (previously an out-of-bounds header read).
    truncate(64);
    let err = unsafe { RingBuffer::attach_mmap(&path) }
        .err()
        .expect("truncated file must be rejected");
    assert!(format!("{err}").contains("shorter than the fixed header"), "{err}");
}

// ----- R6: view churn under a live writer -----

/// One bounded writer, one fast drainer, and a churner that repeatedly
/// registers a fresh view, borrow-reads one record with full content
/// validation, and drops it. Without the registration handshake, the writer's
/// cursor scan could miss the fresh claim and write past it, handing the borrow
/// overwritten bytes (UB Miri reports); with it, every borrowed record is
/// coherent. Bounded iterations, no free-running wrap stress.
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
