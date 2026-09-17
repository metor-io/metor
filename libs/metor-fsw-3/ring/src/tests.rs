//! Ring behavior tests. Miri skips mmap and executor tests.
//! See `MIRI.md` for interpreter coverage.
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

#[test]
fn alignment_rounding_at_usize_boundary() {
    assert_eq!(round_up_16(0), 0);
    assert_eq!(round_up_16(1), 16);
    assert_eq!(round_up_16(16), 16);
    assert_eq!(round_up_16(usize::MAX - 15), usize::MAX - 15);
    assert_eq!(frame_len(usize::MAX - 31), usize::MAX - 15);
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

#[test]
fn wraparound_aligned() {
    // A 17-byte payload occupies 48 bytes and leaves a 16-byte wrap gap.
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    let mut buf = Vec::new();

    for i in 0u8..12 {
        let msg = [i; 17];
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

#[test]
fn backpressure() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    let mut buf = Vec::new();

    // Each record is 32 bytes. Two fit (64 <= 64), and a third would
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

#[test]
fn latest_bytes_remain_pinned_without_a_grant() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).expect("writer");
    let mut v = rb.view(NoWake).expect("reader");
    assert_eq!(v.try_latest_bytes(), Ok(None));
    w.try_write(&[1; 16]).expect("fits");
    let bytes = v.try_latest_bytes().expect("valid").expect("record");
    w.try_write(&[2; 16]).expect("fits");
    assert_eq!(w.try_write(&[3; 16]), Err(WriteError::WouldBlock));
    assert_eq!(bytes, &[1; 16]);
    assert_eq!(v.try_latest_bytes(), Ok(Some(&[2; 16][..])));
    w.try_write(&[3; 16]).expect("old record released");
    assert_eq!(v.try_latest_bytes(), Ok(Some(&[3; 16][..])));
}

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

#[test]
fn raw_attach_recovers_geometry() {
    let rb = ring(256, 3);
    // Commit something so the recovered `committed` is non-trivial.
    rb.writer(NoWake).unwrap().try_write(b"x").unwrap();

    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    assert_eq!(raw.committed(), rb.committed());

    // The recovered capacity must accept a 256-byte frame and reject 272 bytes.
    let mut w = raw.writer(NoWake).unwrap();
    assert_eq!(
        w.try_write(&[0u8; 241]),
        Err(WriteError::InsufficientCapacity)
    );
    assert_ne!(
        w.try_write(&[0u8; 240]),
        Err(WriteError::InsufficientCapacity)
    );
}

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

#[test]
fn swap_writer_and_reader_reacquire() {
    // A single slot makes leaks visible:
    // fail the second `view()` and backpressure the new writer forever.
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
// neither of which Miri provides.

#[test]
#[cfg(all(feature = "mmap", not(miri)))]
fn mmap_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    let cfg = Config {
        capacity: 1024,
        max_readers: 4,
    };
    // SAFETY: this test exclusively owns the new file and its mappings.
    let rb = unsafe { RingBuffer::create_mmap(&path, cfg) }.unwrap();
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

#[test]
fn second_writer_rejected() {
    let rb = ring(256, 1);
    let w1 = rb.writer(NoWake).unwrap();
    assert!(rb.writer(NoWake).is_err());
    assert!(rb.clone().writer(NoWake).is_err());
    drop(w1);
    assert!(rb.writer(NoWake).is_ok());
}

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

#[test]
fn writer_claim_shared_across_attach() {
    let rb = ring(256, 1);
    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    let w1 = rb.writer(NoWake).unwrap();
    assert!(raw.writer(NoWake).is_err(), "claim visible cross-handle");
    drop(w1);
    let mut w2 = raw.writer(NoWake).unwrap();
    w2.try_write(b"raw side").unwrap();
    assert!(rb.writer(NoWake).is_err(), "claim visible in reverse");
}

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

#[test]
fn reader_on_gap_start_reads_through() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    let mut buf = Vec::new();

    w.try_write(&[1u8; 17]).unwrap();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(v.cursor(), 48);

    w.try_write(&[3u8; 16]).unwrap(); // wraps; hwm 48, record at abs 64..96
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

#[test]
fn attach_rejects_truncated() {
    let (_rb, base, len) = valid_region();
    assert_eq!(
        unsafe { RingBuffer::attach_raw(base, len - 8) }.err(),
        Some(AttachError::RegionTruncated),
    );
}

#[test]
fn attach_rejects_bad_capacity() {
    let (_rb, base, len) = valid_region();
    for bad in [0u64, 48, 4, 8, u64::MAX] {
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

#[test]
fn attach_rejects_misaligned() {
    let (_rb, base, len) = valid_region();
    assert_eq!(
        // SAFETY: base+1 .. base+len is still inside the live region.
        unsafe { RingBuffer::attach_raw(base.add(1), len - 1) }.err(),
        Some(AttachError::Misaligned),
    );
}

#[test]
#[cfg(all(feature = "mmap", not(miri)))]
fn attach_mmap_rejects_truncated_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    let cfg = Config {
        capacity: 1024,
        max_readers: 2,
    };
    // SAFETY: the file is new, and the ring drops before truncation.
    drop(unsafe { RingBuffer::create_mmap(&path, cfg) }.unwrap());

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

#[test]
fn reclaim_frees_dead_reader() {
    const DEAD_PID: u64 = 4242;
    let rb = ring(64, 2);
    let mut w = rb.writer(NoWake).unwrap();

    // Plant a foreign reader claim directly, as if another process
    // registered a view at position 0 and then died: a pinned cursor with a
    // dead owner and no local `View` object at all.
    rb.inner.slot(0).cursor.store(0, Release);
    rb.inner.slot(0).owner.store(DEAD_PID, Release);

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
    rb.inner.control().writer.store(DEAD_PID, Release);
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

#[test]
fn reclaim_skips_other_owners() {
    let rb = ring(256, 2);
    let v = rb.view(NoWake).unwrap();
    // Plant a foreign owner on the view's slot (the first free one) and a
    // foreign writer claim, as if another process held both.
    rb.inner.slot(0).owner.store(999_999_999, Release);
    rb.inner.control().writer.store(999_999_999, Release);

    // SAFETY: reclaim by our pid; the planted foreign tags must not match.
    unsafe { rb.reclaim_owner(std::process::id() as u64) };
    assert_eq!(rb.reader_count(), 1, "foreign reader survives");
    assert!(rb.writer(NoWake).is_err(), "foreign writer claim survives");
    drop(v);
}

// ----- Async paths -----
//
// Executor tests require notification support and run outside Miri.

#[cfg(all(feature = "notify", not(miri)))]
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

#[cfg(all(feature = "notify", not(miri)))]
#[stellarator::test]
async fn read_returns_a_ready_record_without_waiting() {
    let ring = ring(64, 2);
    let notifier = Notifier::default();
    let mut w = ring.writer(notifier.clone()).unwrap();
    let mut v = ring.view(notifier).unwrap();

    // Committed before the first poll, so the loop returns on its first pass
    // and never arms the wait.
    w.try_write(b"ready").unwrap();
    let grant = v.read().await.expect("never corrupt");
    assert_eq!(&grant[..], b"ready");
}

#[cfg(all(feature = "notify", not(miri)))]
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

#[test]
fn create_raw_formats_a_caller_owned_region() {
    let cfg = Config {
        capacity: 1024,
        max_readers: 4,
    };
    let len = region_len(&cfg);
    // Over-allocate so the "region may be larger than the ring" case is live.
    // `Word` keeps the region 16-aligned on every allocator.
    let mut region: Vec<Word> = (0..(len + 64) / 16)
        .map(|_| Word(UnsafeCell::new([0; 2])))
        .collect();
    let base = region.as_mut_ptr().cast::<u8>();
    let region_len = region.len() * size_of::<Word>();

    // SAFETY: `region` outlives every handle below and nothing else reads it.
    let ring = unsafe { RingBuffer::create_raw(base, region_len, cfg) }.expect("formats");
    let mut writer = ring.writer(NoWake).expect("sole writer");

    // Attach independently and register before publication to receive the record.
    // SAFETY: same live region, still owned by `region`.
    let attached = unsafe { RingBuffer::attach_raw(base, len) }.expect("attaches");
    let mut view = attached.view(NoWake).expect("reader slot");

    writer.try_write(&[1u8; 16]).expect("record fits");
    assert_eq!(
        &view.try_latest().unwrap().expect("the record")[..],
        &[1u8; 16]
    );
}

#[test]
fn create_raw_rejects_bad_regions() {
    let cfg = Config {
        capacity: 1024,
        max_readers: 4,
    };
    let len = region_len(&cfg);
    let mut region: Vec<Word> = (0..(len + 16) / 16)
        .map(|_| Word(UnsafeCell::new([0; 2])))
        .collect();
    let base = region.as_mut_ptr().cast::<u8>();

    // SAFETY: a live region; the call fails before formatting anything.
    let misaligned = unsafe { RingBuffer::create_raw(base.wrapping_add(8), len, cfg) };
    assert!(matches!(misaligned, Err(AttachError::Misaligned)));

    // SAFETY: same, one byte short of the computed layout.
    let small = unsafe { RingBuffer::create_raw(base, len - 1, cfg) };
    assert!(matches!(small, Err(AttachError::TooSmall)));
}

#[test]
fn checked_region_size_rejects_unrepresentable_geometry() {
    for cfg in [
        Config {
            capacity: 8,
            max_readers: usize::MAX,
        },
        Config {
            capacity: 1usize << (usize::BITS - 1),
            max_readers: 1,
        },
        Config {
            capacity: 8,
            max_readers: 0,
        },
        Config {
            capacity: 3,
            max_readers: 1,
        },
    ] {
        assert_eq!(checked_region_len(&cfg), None);
    }
    let cfg = Config {
        capacity: 64,
        max_readers: 2,
    };
    assert_eq!(checked_region_len(&cfg), Some(region_len(&cfg)));
}

fn check_payload_alignment(rb: &RingBuffer) {
    let mut writer = rb.writer(NoWake).unwrap();
    let mut reader = rb.view(NoWake).unwrap();
    for len in [0, 1, 8, 16, 17, 31, 1, 16] {
        let payload = [42u8; 31];
        writer.try_write(&payload[..len]).unwrap();
        let grant = reader.try_read().unwrap().unwrap();
        assert_eq!(&*grant, &payload[..len]);
        assert!((grant.as_ptr() as usize).is_multiple_of(PAYLOAD_ALIGNMENT));
    }
    assert!(rb.committed() > 64);
}

#[test]
fn heap_and_raw_payloads_are_aligned_across_wraps() {
    let rb = ring(64, 1);
    check_payload_alignment(&rb);
    let (base, len) = rb.region();
    assert!((base as usize).is_multiple_of(PAYLOAD_ALIGNMENT));
    // SAFETY: rb owns the region and outlives the attached handle.
    let attached = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();
    check_payload_alignment(&attached);
}

#[test]
#[cfg(all(feature = "mmap", not(miri)))]
fn mmap_payloads_are_aligned_across_wraps() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aligned.bin");
    // SAFETY: this test exclusively owns the new file throughout its lifetime.
    let rb = unsafe {
        RingBuffer::create_mmap(
            &path,
            Config {
                capacity: 64,
                max_readers: 1,
            },
        )
    }
    .unwrap();
    check_payload_alignment(&rb);
    // SAFETY: rb keeps the exclusively owned file live throughout attachment.
    let attached = unsafe { RingBuffer::attach_mmap(&path) }.unwrap();
    check_payload_alignment(&attached);
}

#[test]
fn attach_rejects_old_version_and_eight_byte_alignment() {
    let (rb, base, len) = valid_region();
    // SAFETY: this offset stays inside the allocation and fails before reading.
    assert_eq!(
        unsafe { RingBuffer::attach_raw(base.add(8), len - 8) }.err(),
        Some(AttachError::Misaligned)
    );
    let mut header = RegionHeader {
        magic: MAGIC,
        version: VERSION - 1,
        flags: 0,
        capacity: 64,
        data_offset: rb.inner.geometry.data_offset as u64,
        max_readers: 2,
        reader_table_offset: HEADER_SIZE as u32,
        total_size: len as u64,
        arch_tag: arch_tag(),
    };
    assert!(matches!(
        validate_header(&header, len),
        Err(AttachError::BadVersion)
    ));
    header.version = VERSION;
    header.data_offset += 8;
    header.total_size += 8;
    assert!(matches!(
        validate_header(&header, len + 8),
        Err(AttachError::BadGeometry)
    ));
}

#[test]
fn empty_ring_wraps_for_a_full_record() {
    let ring = ring(64, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    for _ in 0..2 {
        writer.try_write(&[1; 8]).unwrap();
        writer.try_write(&[2; 48]).unwrap();
    }
    assert_eq!(ring.committed(), 256);
    assert_eq!(
        writer.try_write(&[0; 49]),
        Err(WriteError::InsufficientCapacity)
    );
    assert_eq!(ring.committed(), 256);
}

#[test]
fn caught_up_reader_skips_padding_before_retry() {
    let ring = ring(64, 1);
    let mut reader = ring.view(NoWake).unwrap();
    let mut writer = ring.writer(NoWake).unwrap();
    writer.try_write(&[1; 8]).unwrap();
    drop(reader.try_read().unwrap().unwrap());
    assert_eq!(writer.try_write(&[2; 48]), Err(WriteError::WouldBlock));
    assert_eq!(ring.committed(), 64);
    assert!(reader.try_read().unwrap().is_none());
    writer.try_write(&[2; 48]).unwrap();
    assert_eq!(&*reader.try_read().unwrap().unwrap(), &[2; 48]);
}

#[test]
fn padding_preserves_latest_and_held_grants() {
    let ring = ring(64, 1);
    let mut reader = ring.view(NoWake).unwrap();
    let mut writer = ring.writer(NoWake).unwrap();
    writer.try_write(&[1; 8]).unwrap();
    let grant = reader.try_latest().unwrap().unwrap();
    assert_eq!(writer.try_write(&[2; 48]), Err(WriteError::WouldBlock));
    assert_eq!(&*grant, &[1; 8]);
    drop(grant);
    assert_eq!(&*reader.try_latest().unwrap().unwrap(), &[1; 8]);
    assert_eq!(reader.cursor(), 0);
    drop(reader.try_read().unwrap().unwrap());
    assert!(reader.try_read().unwrap().is_none());
    writer.try_write(&[2; 48]).unwrap();
    assert_eq!(&*reader.try_latest().unwrap().unwrap(), &[2; 48]);
}

#[test]
fn registration_during_padding_publication() {
    let ring = ring(64, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    writer.try_write(&[1; 8]).unwrap();
    let consumer = ring.clone();
    let task = std::thread::spawn(move || {
        let mut reader = consumer.view(NoWake).unwrap();
        if let Some(grant) = reader.try_read().unwrap() {
            assert_eq!(&*grant, &[2; 48]);
        }
    });
    let result = writer.try_write(&[2; 48]);
    assert!(result.is_ok() || result == Err(WriteError::WouldBlock));
    task.join().unwrap();
}

const DEAD_PID: u64 = 999_999_999;

#[test]
fn reclaim_does_not_free_a_reused_slot() {
    let ring = ring(64, 1);
    let reader = ring.view(NoWake).unwrap();
    ring.inner.slot(0).owner.store(DEAD_PID, Release);
    drop(reader);
    assert_eq!(ring.inner.slot(0).owner.load(Acquire), 0);
    let consumer = ring.clone();
    let task = std::thread::spawn(move || {
        let reader = consumer.view(NoWake).unwrap();
        assert_ne!(consumer.inner.slot(0).cursor.load(Acquire), FREE_SLOT);
        assert_eq!(consumer.inner.slot(0).owner.load(Acquire), owner_tag());
        drop(reader);
    });
    // SAFETY: the synthetic owner has no live handles.
    unsafe { ring.reclaim_owner(DEAD_PID) };
    task.join().unwrap();
}

#[test]
fn concurrent_reclaimers_do_not_free_a_new_owner() {
    let ring = ring(64, 1);
    ring.inner.slot(0).cursor.store(0, Release);
    ring.inner.slot(0).owner.store(DEAD_PID, Release);
    let reclaimer = ring.clone();
    let task = std::thread::spawn(move || {
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

#[test]
fn drain_yields_every_record_and_consumes_on_the_next_read() {
    let ring = ring(256, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    for i in 0..3u8 {
        writer.try_write(&[i; 8]).unwrap();
    }
    let records: Vec<&[u8]> = view.drain().map(|r| r.unwrap()).collect();
    assert_eq!(records, [&[0u8; 8][..], &[1u8; 8], &[2u8; 8]]);
    // Slices outlive the iterator; the cursor has not moved yet.
    assert_eq!(view.cursor(), 0);
    assert!(view.try_read().unwrap().is_none());
    assert_eq!(view.cursor(), 3 * frame_len(8) as u64);
    assert!(view.drain().next().is_none());
}

#[test]
fn drain_break_consumes_only_the_records_yielded() {
    let ring = ring(256, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    for i in 0..3u8 {
        writer.try_write(&[i; 8]).unwrap();
    }
    let first = view.drain().next().unwrap().unwrap().to_vec();
    assert_eq!(first, [0u8; 8]);
    let rest: Vec<u8> = view.drain().map(|r| r.unwrap()[0]).collect();
    assert_eq!(rest, [1, 2]);
}

#[test]
fn drain_pins_its_records_until_settled() {
    let ring = ring(64, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    writer.try_write(&[1; 8]).unwrap();
    writer.try_write(&[2; 8]).unwrap();
    let drained: Vec<&[u8]> = view.drain().map(|r| r.unwrap()).collect();
    assert_eq!(writer.try_write(&[3; 8]), Err(WriteError::WouldBlock));
    assert_eq!(drained[0], &[1; 8]);
    assert!(view.try_read().unwrap().is_none());
    writer.try_write(&[3; 8]).unwrap();
}

#[test]
fn drain_reads_through_a_wrap_gap() {
    let ring = ring(128, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    // 48-byte records: two fit a lap, the third wraps.
    for i in 0..2u8 {
        writer.try_write(&[i; 32]).unwrap();
    }
    assert!(view.drain().count() == 2);
    assert!(view.try_read().unwrap().is_none());
    writer.try_write(&[7; 32]).unwrap();
    writer.try_write(&[8; 32]).unwrap();
    let seen: Vec<u8> = view.drain().map(|r| r.unwrap()[0]).collect();
    assert_eq!(seen, [7, 8]);
}

#[test]
fn drain_reports_corrupt_once() {
    let ring = ring(64, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    writer.try_write(&[1; 8]).unwrap();
    // SAFETY: scribbling the length field of a published record.
    unsafe { (ring.inner.data_ptr(0) as *mut u64).write(u64::MAX) };
    let mut drain = view.drain();
    assert_eq!(drain.next(), Some(Err(ReadError::Corrupt)));
    assert!(drain.next().is_none());
}

#[test]
fn copy_reuses_reserved_storage_and_preserves_it_without_a_record() {
    let ring = ring(64, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    let mut buffer = Vec::with_capacity(48);
    buffer.extend_from_slice(b"unchanged");
    let pointer = buffer.as_ptr();
    let capacity = buffer.capacity();

    assert_eq!(view.try_read_into(&mut buffer), Ok(false));
    assert_eq!(buffer, b"unchanged");
    writer.try_write(b"copied").unwrap();
    // SAFETY: this published header has no concurrent readers or writer.
    unsafe { ring.inner.data_ptr(0).cast::<u64>().write(u32::MAX as u64) };
    assert_eq!(view.try_read_into(&mut buffer), Err(ReadError::Corrupt));
    assert_eq!(buffer, b"unchanged");
    assert_eq!(view.cursor(), 0);

    // SAFETY: restore the header before the next read; no borrow is live.
    unsafe { ring.inner.data_ptr(0).cast::<u64>().write(6) };
    assert_eq!(view.try_read_into(&mut buffer), Ok(true));
    assert_eq!(buffer, b"copied");
    assert_eq!(buffer.as_ptr(), pointer);
    assert_eq!(buffer.capacity(), capacity);
    assert_eq!(view.cursor(), frame_len(6) as u64);
    assert_eq!(view.try_read_into(&mut buffer), Ok(false));
    assert_eq!(buffer, b"copied");
}

#[test]
fn empty_payload_after_the_final_header() {
    let ring = ring(64, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    writer.try_write(&[1; 17]).unwrap();
    drop(view.try_read().unwrap().unwrap());
    assert_eq!(view.cursor(), 48);
    writer.try_write(&[]).unwrap();

    let grant = view.try_read().unwrap().unwrap();
    assert!(grant.is_empty());
    // SAFETY: one-past the data region is valid for this empty payload.
    assert_eq!(grant.as_ptr(), unsafe { ring.inner.data_ptr(64) });
    drop(grant);
    assert_eq!(view.cursor(), 64);
    writer.try_write(b"next lap").unwrap();
    assert_eq!(&*view.try_read().unwrap().unwrap(), b"next lap");
}

#[test]
fn config_of_rejects_invalid_regions() {
    let backing = Backing::heap(HEADER_SIZE);
    // SAFETY: each call stays inside this live backing and creates no handles.
    unsafe {
        assert!(matches!(
            config_of(backing.base(), backing.len()),
            Err(AttachError::BadMagic)
        ));
        assert!(matches!(
            config_of(backing.base(), 8),
            Err(AttachError::TooSmall)
        ));
        assert!(matches!(
            config_of(backing.base().add(8), backing.len() - 8),
            Err(AttachError::Misaligned)
        ));
    }
    let (ring, base, len) = valid_region();
    // SAFETY: the immutable header is changed only in this single-threaded test.
    unsafe { base.add(OFF_CAPACITY).cast::<u64>().write(3) };
    // SAFETY: the backing remains live and no peer accesses its header.
    assert!(matches!(
        unsafe { config_of(base, len) },
        Err(AttachError::BadGeometry)
    ));
    drop(ring);
}

#[test]
fn metadata_and_attach_accept_gaps_and_trailing_storage() {
    let cfg = Config {
        capacity: 64,
        max_readers: 2,
    };
    let mut geometry = layout(&cfg);
    geometry.reader_table_offset += 16;
    geometry.data_offset += 32;
    geometry.total_size += 48;
    let backing = Backing::heap(geometry.total_size + 32);
    // SAFETY: fresh aligned storage; both section gaps and trailing bytes fit.
    unsafe { init_region(&backing, &geometry) };
    // SAFETY: backing outlives every handle; all later access uses the ring.
    let recovered = unsafe { config_of(backing.base(), backing.len()) }.unwrap();
    assert_eq!(recovered.capacity, cfg.capacity);
    assert_eq!(recovered.max_readers, cfg.max_readers);
    // SAFETY: same backing and lifetime as the metadata call.
    let ring = unsafe { RingBuffer::attach_raw(backing.base(), backing.len()) }.unwrap();
    assert_eq!(ring.region().1, backing.len());
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    writer.try_write(b"padded layout").unwrap();
    assert_eq!(&*view.try_read().unwrap().unwrap(), b"padded layout");
}

#[test]
fn async_read_remains_send_with_a_non_sync_sink() {
    use std::cell::Cell;
    use std::future::Future;

    struct Sink(Cell<u32>);

    impl WakeSink for Sink {
        fn wait_until<F: FnMut() -> bool>(&self, mut ready: F) -> impl Future<Output = ()> {
            self.0.set(self.0.get() + 1);
            async move {
                let _ = ready();
            }
        }
    }

    fn assert_send(_: impl Send) {}

    let ring = ring(64, 1);
    let mut view = ring.view(Sink(Cell::new(0))).unwrap();
    assert_send(view.read());
}

#[test]
fn async_read_rechecks_after_spurious_and_padding_wakes() {
    use std::cell::{Cell, RefCell};
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    struct ScriptedWake<'a> {
        writer: RefCell<Writer<NoWake>>,
        calls: &'a Cell<usize>,
        payload: &'static [u8],
        padding: bool,
    }

    impl WakeSink for ScriptedWake<'_> {
        async fn wait_until<F: FnMut() -> bool>(&self, mut ready: F) {
            let call = self.calls.get() + 1;
            self.calls.set(call);
            assert!(!ready());
            match call {
                1 if self.padding => assert_eq!(
                    self.writer.borrow_mut().try_write(self.payload),
                    Err(WriteError::WouldBlock)
                ),
                1 => {}
                2 => self.writer.borrow_mut().try_write(self.payload).unwrap(),
                _ => panic!("read did not observe the published record"),
            }
            if call == 2 || self.padding {
                assert!(ready());
            }
        }
    }

    for padding in [false, true] {
        let ring = ring(64, 1);
        let mut writer = ring.writer(NoWake).unwrap();
        writer.try_write(&[1; 8]).unwrap();
        let calls = Cell::new(0);
        let payload: &'static [u8] = if padding { &[2; 48] } else { b"ready" };
        let mut view = ring
            .view(ScriptedWake {
                writer: RefCell::new(writer),
                calls: &calls,
                payload,
                padding,
            })
            .unwrap();
        let mut context = Context::from_waker(Waker::noop());
        {
            let future = view.read();
            let mut future = std::pin::pin!(future);
            let Poll::Ready(Ok(grant)) = future.as_mut().poll(&mut context) else {
                panic!("scripted wake should complete the read in one poll");
            };
            assert_eq!(&*grant, payload);
        }
        assert_eq!(calls.get(), 2);
        assert_eq!(view.cursor(), ring.committed());
    }
}

#[test]
fn checked_frame_length_rejects_unrepresentable_payloads() {
    assert_eq!(checked_frame_len(0, 16), Ok(16));
    assert_eq!(checked_frame_len(48, 64), Ok(64));
    assert_eq!(
        checked_frame_len(49, 64),
        Err(WriteError::InsufficientCapacity)
    );
    assert_eq!(
        checked_frame_len(usize::MAX, 64),
        Err(WriteError::InsufficientCapacity)
    );
    assert_eq!(
        checked_frame_len(usize::MAX - 15, 64),
        Err(WriteError::InsufficientCapacity)
    );
}

#[cfg(target_pointer_width = "64")]
#[test]
fn checked_frame_length_respects_the_stored_length_width() {
    let capacity = 1u64 << 33;
    assert_eq!(
        checked_frame_len(u32::MAX as usize, capacity),
        Ok((1u64 << 32) + 16)
    );
    assert_eq!(
        checked_frame_len(u32::MAX as usize + 1, capacity),
        Err(WriteError::InsufficientCapacity)
    );
}
