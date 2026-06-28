//! Tests for the ring buffer.
//!
//! The synchronous tests use only `try_*` APIs + `std::thread`, so they run
//! under Miri (which cannot drive the async runtime). They exercise the unsafe
//! pointer/atomic paths so Miri can check provenance, leaks, and data races —
//! especially the overwrite-mode concurrent read/write. See `libs/db/MIRI.md`;
//! this crate follows the same strategy (`UnsafeCell` backing, sync tests,
//! Tree Borrows). Loop bounds shrink under `cfg!(miri)`.

use super::*;

fn overwrite(capacity: usize, max_readers: usize) -> RingBuffer<BoxBacking> {
    RingBuffer::create_in_memory(Config {
        capacity,
        max_readers,
        overrun: Overrun::Overwrite,
    })
}

fn lossless(capacity: usize, max_readers: usize) -> RingBuffer<BoxBacking> {
    RingBuffer::create_in_memory(Config {
        capacity,
        max_readers,
        overrun: Overrun::Lossless,
    })
}

// ----- Basic single-threaded paths -----

#[test]
fn roundtrip() {
    let rb = overwrite(1024, 4);
    let mut w = rb.writer(NoWake, NoWake);
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
/// an 8-byte wrap gap. The reader (kept up, so never lapped) must skip the gap
/// and reconstruct each message.
#[test]
fn wraparound_aligned() {
    // cap 64, payload 16 -> record 24. 24,48 fit; the third would straddle
    // (48+24 > 64), so the writer leaves a 16-byte gap and wraps to offset 0.
    let rb = overwrite(64, 1);
    let mut w = rb.writer(NoWake, NoWake);
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
    let rb = lossless(1024, 4);
    let mut w = rb.writer(NoWake, NoWake);
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
    let rb = overwrite(256, 2);
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

// ----- The two policy-specific behaviors -----

/// Overwrite mode: a reader left behind is lapped and gets `Err(Lapped)` — never
/// torn or garbage data. Earlier in-range reads returned the correct bytes.
#[test]
fn overwrite_slow_reader_lapped() {
    let rb = overwrite(64, 1);
    let mut w = rb.writer(NoWake, NoWake);
    let mut v = rb.view(NoWake, NoWake).unwrap();
    let mut buf = Vec::new();

    // Two records (record = 24) fit in 64 with no overwrite; read them.
    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &[1u8; 16]);
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &[2u8; 16]);
    assert!(!v.is_lapped());

    // Now flood the buffer, lapping the (idle) reader.
    for _ in 0..16 {
        w.try_write(&[9u8; 16]).unwrap();
    }
    assert!(v.is_lapped());
    assert_eq!(v.try_read_into(&mut buf), Err(ReadError::Lapped));

    // After resync the reader resumes from the live edge.
    v.resync();
    assert!(!v.is_lapped());
    w.try_write(&[7u8; 16]).unwrap();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &[7u8; 16]);
}

/// Lossless mode: the writer backpressures (`WouldBlock`) rather than lapping a
/// reader; freeing space lets the next write through. No data is ever lost.
#[test]
fn lossless_backpressure() {
    let rb = lossless(64, 1);
    let mut w = rb.writer(NoWake, NoWake);
    let mut v = rb.view(NoWake, NoWake).unwrap();
    let mut buf = Vec::new();

    // record = 24; two fit (48 <= 64), the third would overwrite the idle reader.
    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    assert_eq!(w.try_write(&[3u8; 16]), Err(WriteError::WouldBlock));

    // The reader is never lapped, so it reads the first record correctly.
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
fn lossless_borrow_read() {
    let rb = lossless(256, 1);
    let mut w = rb.writer(NoWake, NoWake);
    let mut v = rb.view(NoWake, NoWake).unwrap();

    w.try_write(b"borrowed").unwrap();
    {
        let grant = v.try_read().unwrap().expect("a record");
        assert_eq!(&grant[..], b"borrowed");
    } // drop advances the cursor
    assert!(v.try_read().unwrap().is_none());
}

#[test]
fn overwrite_borrow_unsupported() {
    let rb = overwrite(256, 1);
    let mut v = rb.view(NoWake, NoWake).unwrap();
    assert_eq!(v.try_read().err(), Some(ReadError::BorrowNotSupported));
}

#[test]
fn oversize_message_rejected() {
    let rb = overwrite(64, 1);
    let mut w = rb.writer(NoWake, NoWake);
    // 64-byte payload -> record 72 > capacity 64.
    assert_eq!(w.try_write(&[0u8; 64]), Err(WriteError::InsufficientCapacity));
}

// ----- Concurrency / Miri data-race coverage -----

use std::sync::atomic::{AtomicBool, Ordering as O};
use std::thread;

/// Writer thread overwrites a tiny buffer as fast as it can while a reader copies
/// records out. This is the data-race coverage for Miri: the writer's relaxed
/// atomic stores and the reader's relaxed atomic loads on the *same* bytes,
/// resolved by the whole-buffer lap recheck. Every non-lapped record the reader
/// observes must be a value the writer actually wrote (never torn); on a lap the
/// reader resyncs. Loop bounds shrink under Miri.
#[test]
fn concurrent_overwrite_no_ub() {
    let n: u64 = if cfg!(miri) { 64 } else { 5_000 };
    let rb = overwrite(64, 1);
    let v = rb.view(NoWake, NoWake).unwrap();
    let done = std::sync::Arc::new(AtomicBool::new(false));

    let consumer = {
        let done = done.clone();
        thread::spawn(move || {
            let mut v = v;
            let mut buf = Vec::new();
            let mut seen = 0u64;
            loop {
                match v.try_read_into(&mut buf) {
                    Ok(true) => {
                        let val = u64::from_le_bytes(buf[..8].try_into().unwrap());
                        assert!(val < n, "torn/garbage value {val}");
                        seen += 1;
                    }
                    Ok(false) => {
                        if done.load(O::Relaxed) {
                            break;
                        }
                        thread::yield_now();
                    }
                    Err(ReadError::Lapped) => v.resync(),
                    Err(e) => panic!("unexpected {e:?}"),
                }
            }
            seen
        })
    };

    let producer = {
        let rb = rb.clone();
        let done = done.clone();
        thread::spawn(move || {
            let mut w = rb.writer(NoWake, NoWake);
            for i in 0..n {
                w.try_write(&i.to_le_bytes()).unwrap();
            }
            done.store(true, O::Relaxed);
        })
    };

    producer.join().unwrap();
    let _seen = consumer.join().unwrap();
}

/// Lossless mode guarantees delivery: a writer that spins on `WouldBlock` and a
/// reader draining concurrently must reconstruct the full ordered stream with
/// zero loss (the disruptor's invariant). Also Miri race coverage for the
/// lossless plain read/write path (race-free via backpressure + `committed`).
#[test]
fn concurrent_lossless_full_stream() {
    let n: u64 = if cfg!(miri) { 48 } else { 4_000 };
    let rb = lossless(128, 1);
    let v = rb.view(NoWake, NoWake).unwrap();

    let consumer = thread::spawn(move || {
        let mut v = v;
        let mut buf = Vec::new();
        let mut got = Vec::with_capacity(n as usize);
        while (got.len() as u64) < n {
            match v.try_read_into(&mut buf) {
                Ok(true) => got.push(u64::from_le_bytes(buf[..8].try_into().unwrap())),
                Ok(false) => thread::yield_now(),
                Err(e) => panic!("lossless reader must never error: {e:?}"),
            }
        }
        got
    });

    let producer = {
        let rb = rb.clone();
        thread::spawn(move || {
            let mut w = rb.writer(NoWake, NoWake);
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
#[test]
fn concurrent_reader_churn() {
    let churn = if cfg!(miri) { 3 } else { 8 };
    let rounds = if cfg!(miri) { 4 } else { 200 };
    let rb = overwrite(128, 16);
    let stop = std::sync::Arc::new(AtomicBool::new(false));

    let writer = {
        let rb = rb.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            let mut w = rb.writer(NoWake, NoWake);
            let mut i = 0u64;
            while !stop.load(O::Relaxed) {
                w.try_write(&i.to_le_bytes()).unwrap();
                i = i.wrapping_add(1);
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

/// Lossless `write().await` must suspend when the buffer fills and resume as the
/// reader drains, losing nothing. A small capacity guarantees real backpressure;
/// the cooperative single-threaded executor ping-pongs writer and reader.
#[cfg(all(feature = "async", not(miri)))]
#[stellarator::test]
async fn wait_writer_backpressures() {
    let rb = lossless(64, 1);
    let data = Notifier::default();
    let space = Notifier::default();
    let mut view = rb.view(data.clone(), space.clone()).unwrap();
    let n: u64 = 50;

    let writer = {
        let rb = rb.clone();
        let data = data.clone();
        let space = space.clone();
        stellarator::spawn(async move {
            let mut w = rb.writer(data, space);
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

// ----- RawBacking / attach_raw (non-owning, same-process) -----

/// Same-process attach round-trip (the dlopen scenario): a second
/// `RingBuffer<RawBacking>` reconstructed over the SAME region a
/// `RingBuffer<BoxBacking>` allocated sees the identical atomics — a record
/// written through one is read through the other, in both directions. Sync-only,
/// so it runs under Miri (provenance/leak/race check of the raw attach path).
#[test]
fn raw_attach_same_process_roundtrip() {
    let rb = overwrite(1024, 4);
    let (base, len) = rb.region();
    // Attach a non-owning handle over the SAME bytes `rb` owns.
    let raw = unsafe { RingBuffer::<RawBacking>::attach_raw(base, len) }.unwrap();

    let mut buf = Vec::new();

    // Box-side writer -> Raw-side view.
    {
        let mut vr = raw.view(NoWake, NoWake).unwrap();
        let mut wb = rb.writer(NoWake, NoWake);
        wb.try_write(b"box->raw").unwrap();
        assert!(vr.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"box->raw");
    }
    // Raw-side writer -> Box-side view (same region, identical atomics).
    {
        let mut vb = rb.view(NoWake, NoWake).unwrap();
        let mut wr = raw.writer(NoWake, NoWake);
        wr.try_write(b"raw->box").unwrap();
        assert!(vb.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], b"raw->box");
    }
}

/// Geometry is recovered from the region header, not passed to `attach_raw`:
/// overrun, committed position, and capacity all match the original handle.
#[test]
fn raw_attach_recovers_geometry() {
    let rb = RingBuffer::create_in_memory(Config {
        capacity: 256,
        max_readers: 3,
        overrun: Overrun::Lossless,
    });
    // Commit something so the recovered `committed` is non-trivial.
    rb.writer(NoWake, NoWake).try_write(b"x").unwrap();

    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::<RawBacking>::attach_raw(base, len) }.unwrap();

    // Overrun + committed came from the header, not from arguments.
    assert_eq!(raw.overrun(), Overrun::Lossless);
    assert_eq!(raw.committed(), rb.committed());

    // Capacity (256) recovered too: the `InsufficientCapacity` boundary is
    // exactly `frame_len(payload) > 256`, independent of how much is committed.
    // 249 -> record 264 (rejected); 248 -> record 256 (capacity-allowed, only
    // backpressured here because the region already holds a byte).
    let mut w = raw.writer(NoWake, NoWake);
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
    let zeros = BoxBacking::new(HEADER_SIZE);
    assert_eq!(
        unsafe { RingBuffer::<RawBacking>::attach_raw(zeros.base(), zeros.len()) }.err(),
        Some(AttachError::BadMagic),
    );

    // Too-short region: guarded before any header read, so no out-of-bounds load.
    let tiny = BoxBacking::new(8);
    assert_eq!(
        unsafe { RingBuffer::<RawBacking>::attach_raw(tiny.base(), tiny.len()) }.err(),
        Some(AttachError::BadMagic),
    );
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
        overrun: Overrun::Lossless,
    };
    let rb = RingBuffer::create_mmap(&path, cfg).unwrap();
    let mut w = rb.writer(NoWake, NoWake);
    let mut v = rb.view(NoWake, NoWake).unwrap();

    w.try_write(b"shared memory").unwrap();
    let mut buf = Vec::new();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], b"shared memory");

    // A second handle attaches to the same region and sees matching geometry.
    let attached = unsafe { RingBuffer::attach_mmap(&path) }.unwrap();
    assert_eq!(attached.overrun(), Overrun::Lossless);
    assert_eq!(attached.committed(), rb.committed());
}
