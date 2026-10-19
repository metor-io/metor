//! Ring behavior tests. Miri skips the mmap and executor tests. See `VERIFY.md`.
use super::*;
use crate::models::{self, DEAD_PID, read, ring};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, Ordering as O};
use std::thread;

fn reader_count(ring: &RingBuffer) -> usize {
    (0..ring.inner.geometry.max_readers)
        .filter(|&s| ring.inner.slot(s).cursor.load(Relaxed) != FREE_SLOT)
        .count()
}

/// A 16-byte aligned unit of storage that allows interior mutation.
#[repr(C, align(16))]
struct Word(UnsafeCell<[u64; 2]>);

/// Zeroed, 16-aligned storage of at least `len` bytes.
fn words(len: usize) -> Vec<Word> {
    (0..len.div_ceil(16))
        .map(|_| Word(UnsafeCell::new([0; 2])))
        .collect()
}

#[test]
fn test_alignment_rounding_boundary() {
    assert_eq!(frame_len(0), 16);
    assert_eq!(frame_len(1), 32);
    assert_eq!(frame_len(16), 32);
    assert_eq!(frame_len(usize::MAX - 31), usize::MAX - 15);
}

#[test]
fn test_read_write_roundtrip() {
    let rb = ring(1024, 4);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    w.try_write(b"hello world").unwrap();
    w.try_write(b"foo").unwrap();

    assert_eq!(read(&mut v).as_deref(), Some(&b"hello world"[..]));
    assert_eq!(read(&mut v).as_deref(), Some(&b"foo"[..]));
    assert_eq!(read(&mut v), None);
}

#[test]
fn test_wraparound_aligned() {
    // A 17-byte payload occupies 48 bytes and leaves a 16-byte wrap gap.
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    for i in 0u8..12 {
        let msg = [i; 17];
        w.try_write(&msg).unwrap();
        assert_eq!(read(&mut v).as_deref(), Some(&msg[..]), "message {i}");
        assert_eq!(read(&mut v), None);
    }
    assert!(v.cursor() > 64, "the stream wrapped");
}

#[test]
fn test_multiple_readers() {
    let rb = ring(1024, 4);
    let mut w = rb.writer(NoWake).unwrap();
    let mut a = rb.view(NoWake).unwrap();
    let mut b = rb.view(NoWake).unwrap();

    w.try_write(b"one").unwrap();
    w.try_write(b"two").unwrap();

    for v in [&mut a, &mut b] {
        assert_eq!(read(v).as_deref(), Some(&b"one"[..]));
        assert_eq!(read(v).as_deref(), Some(&b"two"[..]));
        assert_eq!(read(v), None);
    }
}

#[test]
fn test_reader_slot_reuse() {
    let rb = ring(256, 2);

    let a = rb.view(NoWake).unwrap();
    let b = rb.view(NoWake).unwrap();
    assert_eq!(rb.view(NoWake).err(), Some(FullReaderTable));

    drop(b);
    let _c = rb.view(NoWake).unwrap();
    assert_eq!(rb.view(NoWake).err(), Some(FullReaderTable));
    drop(a);
    let _d = rb.view(NoWake).unwrap();
}

#[test]
fn test_backpressure() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    // Each record is 32 bytes. Two fit, and a third would overwrite the idle reader.
    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    assert_eq!(w.try_write(&[3u8; 16]), Err(WriteError::WouldBlock));

    assert_eq!(read(&mut v).as_deref(), Some(&[1u8; 16][..]));
    w.try_write(&[3u8; 16]).unwrap();
    assert_eq!(read(&mut v).as_deref(), Some(&[2u8; 16][..]));
    assert_eq!(read(&mut v).as_deref(), Some(&[3u8; 16][..]));
}

#[test]
fn test_borrow_read() {
    let rb = ring(256, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    w.try_write(b"borrowed").unwrap();
    {
        let grant = v.try_read().unwrap().expect("a record");
        assert_eq!(&grant[..], b"borrowed");
    }
    assert!(v.try_read().unwrap().is_none());
}

#[test]
fn test_owned_grant_pins_until_returned() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let v = rb.view(NoWake).unwrap();
    let Err(v) = v.try_read_owned().unwrap() else {
        panic!("an empty ring yields no grant")
    };
    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    let Ok(Ok(mut grant)) = v.try_read_owned() else {
        panic!("a record")
    };
    assert_eq!(&grant[..], &[1u8; 16]);
    assert_eq!(
        w.try_write(&[3u8; 16]),
        Err(WriteError::WouldBlock),
        "pinned"
    );
    grant.skip(10);
    assert_eq!(&grant[..], &[1u8; 6]);
    grant.skip(100);
    assert!(grant.is_empty());
    let mut v = grant.into_view();
    w.try_write(&[3u8; 16]).expect("the record was consumed");
    assert_eq!(read(&mut v).as_deref(), Some(&[2u8; 16][..]));
    assert_eq!(read(&mut v).as_deref(), Some(&[3u8; 16][..]));
}

#[test]
fn test_dropped_owned_grant_frees_slot() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let v = rb.view(NoWake).unwrap();
    w.try_write(&[1u8; 16]).unwrap();
    let Ok(Ok(grant)) = v.try_read_owned() else {
        panic!("a record")
    };
    assert_eq!(reader_count(&rb), 1);
    assert!(rb.view(NoWake).is_err(), "the grant holds the only slot");
    drop(grant);
    assert_eq!(reader_count(&rb), 0);
    assert!(rb.view(NoWake).is_ok());
}

#[test]
fn test_oversize_message_rejected() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    // A 64-byte payload makes an 80-byte record, more than the whole region.
    assert_eq!(
        w.try_write(&[0u8; 64]),
        Err(WriteError::InsufficientCapacity)
    );
}

#[test]
fn test_latest_pins_newest() {
    let rb = ring(256, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    assert_eq!(v.try_latest(), Ok(None));

    w.try_write(&[1u8; 16]).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    w.try_write(&[3u8; 16]).unwrap();

    assert_eq!(v.try_latest(), Ok(Some(&[3u8; 16][..])));
    assert_eq!(v.try_latest(), Ok(Some(&[3u8; 16][..])), "re-served");

    w.try_write(&[4u8; 16]).unwrap();
    assert_eq!(v.try_latest(), Ok(Some(&[4u8; 16][..])));
}

#[test]
fn test_latest_pins_without_grant() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).expect("writer");
    let mut v = rb.view(NoWake).expect("reader");
    w.try_write(&[1; 16]).expect("fits");
    let bytes = v.try_latest().expect("valid").expect("record");
    w.try_write(&[2; 16]).expect("fits");
    assert_eq!(w.try_write(&[3; 16]), Err(WriteError::WouldBlock));
    assert_eq!(bytes, &[1; 16]);
    assert_eq!(v.try_latest(), Ok(Some(&[2; 16][..])));
    w.try_write(&[3; 16]).expect("old record released");
    assert_eq!(v.try_latest(), Ok(Some(&[3; 16][..])));
}

#[test]
fn test_latest_reader_never_blocks_writer() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    for round in 0..64u8 {
        w.try_write(&[round; 16]).expect("latest pin leaves room");
        assert_eq!(v.try_latest(), Ok(Some(&[round; 16][..])));
    }
}

#[test]
fn test_concurrent_full_stream() {
    let n: u64 = if cfg!(miri) { 48 } else { 4_000 };
    let rb = ring(128, 1);
    let mut v = rb.view(NoWake).unwrap();

    let consumer = thread::spawn(move || {
        let mut got = Vec::with_capacity(n as usize);
        while (got.len() as u64) < n {
            match read(&mut v) {
                Some(record) => got.push(u64::from_le_bytes(record[..8].try_into().unwrap())),
                None => thread::yield_now(),
            }
        }
        got
    });

    let producer = {
        let rb = rb.clone();
        thread::spawn(move || {
            let mut w = rb.writer(NoWake).unwrap();
            for i in 0..n {
                while w.try_write(&i.to_le_bytes()) == Err(WriteError::WouldBlock) {
                    thread::yield_now();
                }
            }
        })
    };

    producer.join().unwrap();
    let got = consumer.join().unwrap();
    assert_eq!(
        got,
        (0..n).collect::<Vec<_>>(),
        "reader lost or reordered data"
    );
}

#[test]
fn test_concurrent_reader_churn() {
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
                        for _ in 0..3 {
                            read(&mut v);
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
    assert_eq!(reader_count(&rb), 0);
}

#[test]
fn test_raw_attach_roundtrip() {
    let rb = ring(1024, 4);
    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();
    {
        let mut vr = raw.view(NoWake).unwrap();
        let mut wb = rb.writer(NoWake).unwrap();
        wb.try_write(b"box->raw").unwrap();
        assert_eq!(read(&mut vr).as_deref(), Some(&b"box->raw"[..]));
    }
    {
        let mut vb = rb.view(NoWake).unwrap();
        let mut wr = raw.writer(NoWake).unwrap();
        wr.try_write(b"raw->box").unwrap();
        assert_eq!(read(&mut vb).as_deref(), Some(&b"raw->box"[..]));
    }
}

#[test]
fn test_raw_attach_recovers_geometry() {
    let rb = ring(256, 3);
    rb.writer(NoWake).unwrap().try_write(b"x").unwrap();

    let (base, len) = rb.region();
    let raw = unsafe { RingBuffer::attach_raw(base, len) }.unwrap();

    assert_eq!(raw.committed(), rb.committed());

    // The recovered capacity accepts a 256-byte frame and rejects 272 bytes.
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
fn test_swap_writer_and_reader_reacquire() {
    // A single slot makes a leaked claim visible.
    let rb = ring(256, 1);
    {
        let mut w1 = rb.writer(NoWake).unwrap();
        let mut v1 = rb.view(NoWake).unwrap();
        assert_eq!(reader_count(&rb), 1);
        w1.try_write(b"occ1-a").unwrap();
        w1.try_write(b"occ1-b").unwrap();
        assert_eq!(read(&mut v1).as_deref(), Some(&b"occ1-a"[..]));
    }
    assert_eq!(reader_count(&rb), 0, "reader slot freed on drop");

    let mut w2 = rb.writer(NoWake).unwrap();
    let mut v2 = rb.view(NoWake).unwrap();
    assert_eq!(reader_count(&rb), 1);
    assert_eq!(read(&mut v2), None, "fresh view sees only new data");
    w2.try_write(b"occ2").unwrap();
    assert_eq!(read(&mut v2).as_deref(), Some(&b"occ2"[..]));
}

#[test]
#[cfg(all(feature = "mmap", not(miri)))]
fn test_mmap_roundtrip() {
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
    assert_eq!(read(&mut v).as_deref(), Some(&b"shared memory"[..]));

    let attached = unsafe { RingBuffer::attach_mmap(&path) }.unwrap();
    assert_eq!(attached.committed(), rb.committed());
}

#[test]
fn test_read_rejects_corrupt_length() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    w.try_write(&[5u8; 16]).unwrap();
    // SAFETY: offset 0 is in-bounds and nothing reads concurrently.
    unsafe { (rb.inner.data_ptr(0) as *mut u64).write(0xFFFF_FFFF) };
    assert_eq!(v.try_read().err(), Some(ReadError));

    // The cursor never moved, so the restored record reads back intact.
    // SAFETY: as above.
    unsafe { (rb.inner.data_ptr(0) as *mut u64).write(16) };
    assert_eq!(read(&mut v).as_deref(), Some(&[5u8; 16][..]));
}

#[test]
fn test_view_starts_stable() {
    let rb = ring(128, 2);
    let v = rb.view(NoWake).unwrap();
    assert_eq!(v.cursor(), rb.committed());
    drop(v);

    let mut w = rb.writer(NoWake).unwrap();
    let mut d = rb.view(NoWake).unwrap();
    for i in 0u8..40 {
        while w.try_write(&[i; 16]) == Err(WriteError::WouldBlock) {
            assert!(read(&mut d).is_some());
        }
    }
    let v2 = rb.view(NoWake).unwrap();
    assert_eq!(v2.cursor(), rb.committed());
}

#[test]
fn test_second_writer_rejected() {
    let rb = ring(256, 1);
    let w1 = rb.writer(NoWake).unwrap();
    assert!(rb.writer(NoWake).is_err());
    assert!(rb.clone().writer(NoWake).is_err());
    drop(w1);
    assert!(rb.writer(NoWake).is_ok());
}

#[test]
fn test_writer_claim_shared_across_attach() {
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
fn test_concurrent_writer_claim_churn() {
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
    assert!(successes.load(O::Relaxed) > 0);
    assert!(rb.writer(NoWake).is_ok(), "claim free after churn");
}

#[test]
fn test_reader_skips_wrap_gap() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();

    w.try_write(&[1u8; 17]).unwrap();
    assert!(read(&mut v).is_some());
    assert_eq!(v.cursor(), 48);

    w.try_write(&[3u8; 16]).unwrap(); // hwm 48, record at 64..96
    assert_eq!(read(&mut v).as_deref(), Some(&[3u8; 16][..]));
    assert_eq!(read(&mut v), None);
}

#[test]
fn test_attach_rejects_bad_regions() {
    use AttachError::*;
    let full = Config {
        capacity: 64,
        max_readers: 2,
    }
    .region_len()
    .unwrap();
    type Case = (fn(&mut RegionHeader), usize, usize, AttachError);
    let keep: fn(&mut RegionHeader) = |_| {};
    let cases: [Case; 18] = [
        (keep, 0, full - 8, RegionTruncated),
        (keep, 0, 8, TooSmall),
        (keep, 1, full - 1, Misaligned),
        (keep, 8, full - 8, Misaligned),
        (|h| h.magic = 0, 0, full, BadMagic),
        (|h| h.version -= 1, 0, full, BadVersion),
        (|h| h.arch_tag = 0, 0, full, ArchMismatch),
        (|h| h.capacity = 0, 0, full, BadGeometry),
        (|h| h.capacity = 8, 0, full, BadGeometry),
        (|h| h.capacity = 48, 0, full, BadGeometry),
        (|h| h.capacity = u64::MAX, 0, full, BadGeometry),
        (|h| h.reader_table_offset += 64, 0, full, BadGeometry),
        (
            |h| h.reader_table_offset = h.data_offset as u32,
            0,
            full,
            BadGeometry,
        ),
        (|h| h.data_offset = h.total_size, 0, full, BadGeometry),
        (|h| h.data_offset += 1, 0, full, BadGeometry),
        (|h| h.data_offset = u64::MAX - 7, 0, full, BadGeometry),
        (
            |h| {
                h.data_offset += 8;
                h.total_size += 8;
            },
            0,
            full + 8,
            BadGeometry,
        ),
        (|h| h.total_size += 16, 0, full, RegionTruncated),
    ];
    for (i, (poke, offset, len, expected)) in cases.into_iter().enumerate() {
        let rb = ring(64, 2);
        let mut region = words(full + 16);
        // SAFETY: `rb`'s region fits the larger `region`, which outlives the attach.
        let result = unsafe {
            let base = region.as_mut_ptr().cast::<u8>();
            std::ptr::copy_nonoverlapping(rb.region().0, base, full);
            let header = base.cast::<RegionHeader>();
            let mut h = header.read();
            poke(&mut h);
            header.write(h);
            RingBuffer::attach_raw(base.add(offset), len)
        };
        assert_eq!(result.err(), Some(expected), "case {i}");
    }
}

#[test]
#[cfg(all(feature = "mmap", not(miri)))]
fn test_attach_mmap_rejects_truncated_file() {
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

    for (len, message) in [
        (512, "exceeds the backing length"),
        (64, "shorter than its header"),
    ] {
        truncate(len);
        let err = unsafe { RingBuffer::attach_mmap(&path) }
            .err()
            .expect("truncated file must be rejected");
        assert!(err.to_string().contains(message), "{err}");
    }
}

#[test]
fn test_concurrent_view_churn() {
    let n: u64 = if cfg!(miri) { 32 } else { 2_000 };
    let churn_rounds = if cfg!(miri) { 8 } else { 300 };
    let rb = ring(128, 4);
    let mut drainer = rb.view(NoWake).unwrap();

    fn check(record: &[u8], n: u64) {
        assert_eq!(record.len(), 16);
        let lo = u64::from_le_bytes(record[..8].try_into().unwrap());
        let hi = u64::from_le_bytes(record[8..].try_into().unwrap());
        assert_eq!(lo, hi, "record torn");
        assert!(lo < n);
    }

    let consumer = thread::spawn(move || {
        let mut count = 0u64;
        while count < n {
            match read(&mut drainer) {
                Some(record) => {
                    check(&record, n);
                    count += 1;
                }
                None => thread::yield_now(),
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
                if let Some(grant) = v.try_read().expect("never corrupt") {
                    check(&grant, n);
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
                while w.try_write(&payload) == Err(WriteError::WouldBlock) {
                    thread::yield_now();
                }
            }
        })
    };

    producer.join().unwrap();
    churner.join().unwrap();
    consumer.join().unwrap();
}

#[test]
fn test_reclaim_frees_dead_reader() {
    let rb = ring(64, 2);
    let mut w = rb.writer(NoWake).unwrap();

    // A foreign reader that registered at 0 and died, with no local `View`.
    rb.inner.slot(0).cursor.store(0, Release);
    rb.inner.slot(0).owner.store(DEAD_PID, Release);

    while w.try_write(&[0u8; 16]).is_ok() {}
    assert_eq!(w.try_write(&[0u8; 16]), Err(WriteError::WouldBlock));

    assert_eq!(reader_count(&rb), 1);
    // SAFETY: no process with this pid holds the slot.
    unsafe { rb.reclaim_owner(DEAD_PID) };
    assert_eq!(reader_count(&rb), 0, "dead cursor freed");

    w.try_write(&[1u8; 16]).expect("writer unblocked");
    let mut v2 = rb.view(NoWake).unwrap();
    w.try_write(&[2u8; 16]).unwrap();
    assert_eq!(read(&mut v2).as_deref(), Some(&[2u8; 16][..]));
}

#[test]
fn test_reclaim_frees_dead_writer_claim() {
    let rb = ring(256, 2);
    let mut v = rb.view(NoWake).unwrap();
    rb.writer(NoWake).unwrap().try_write(b"before").unwrap();
    rb.inner.control().writer.store(DEAD_PID, Release);
    assert!(rb.writer(NoWake).is_err(), "claim held by the dead");

    // SAFETY: no live process holds this claim.
    unsafe { rb.reclaim_owner(DEAD_PID) };
    rb.writer(NoWake).unwrap().try_write(b"after").unwrap();

    assert_eq!(read(&mut v).as_deref(), Some(&b"before"[..]));
    assert_eq!(read(&mut v).as_deref(), Some(&b"after"[..]));
}

#[test]
fn test_reclaim_skips_other_owners() {
    let rb = ring(256, 2);
    let v = rb.view(NoWake).unwrap();
    rb.inner.slot(0).owner.store(DEAD_PID, Release);
    rb.inner.control().writer.store(DEAD_PID, Release);

    // SAFETY: reclaim by our pid; the planted foreign tags must not match.
    unsafe { rb.reclaim_owner(std::process::id() as u64) };
    assert_eq!(reader_count(&rb), 1, "foreign reader survives");
    assert!(rb.writer(NoWake).is_err(), "foreign writer claim survives");
    drop(v);
}

#[cfg(all(feature = "notify", not(miri)))]
#[stellarator::test]
async fn test_no_wake_resolves_immediately() {
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
fn test_create_raw_formats_region() {
    let cfg = Config {
        capacity: 1024,
        max_readers: 4,
    };
    let len = cfg.region_len().unwrap();
    // A region larger than the ring.
    let mut region = words(len + 64);
    let base = region.as_mut_ptr().cast::<u8>();

    // SAFETY: `region` outlives every handle below and nothing else reads it.
    let ring = unsafe { RingBuffer::create_raw(base, len + 64, cfg) }.expect("formats");
    let mut writer = ring.writer(NoWake).expect("sole writer");

    // SAFETY: same live region, still owned by `region`.
    let attached = unsafe { RingBuffer::attach_raw(base, len) }.expect("attaches");
    let mut view = attached.view(NoWake).expect("reader slot");

    writer.try_write(&[1u8; 16]).expect("record fits");
    assert_eq!(view.try_latest(), Ok(Some(&[1u8; 16][..])));
}

#[test]
fn test_create_raw_rejects_bad_regions() {
    let cfg = Config {
        capacity: 1024,
        max_readers: 4,
    };
    let len = cfg.region_len().unwrap();
    let mut region = words(len + 16);
    let base = region.as_mut_ptr().cast::<u8>();

    // SAFETY: a live region; the call fails before formatting anything.
    let misaligned = unsafe { RingBuffer::create_raw(base.wrapping_add(8), len, cfg) };
    assert_eq!(misaligned.err(), Some(AttachError::Misaligned));

    // SAFETY: same, one byte short of the computed layout.
    let small = unsafe { RingBuffer::create_raw(base, len - 1, cfg) };
    assert_eq!(small.err(), Some(AttachError::TooSmall));
}

#[test]
fn test_region_size_rejects_overflow() {
    for (capacity, max_readers) in [
        (8, usize::MAX),
        (1usize << (usize::BITS - 1), 1),
        (8, 0),
        (3, 1),
    ] {
        let cfg = Config {
            capacity,
            max_readers,
        };
        assert_eq!(cfg.region_len(), Err(InvalidConfig));
        assert!(matches!(
            RingBuffer::create_in_memory(cfg),
            Err(InvalidConfig)
        ));
        let mut region = words(64);
        // SAFETY: a live region; the call fails before touching it.
        let raw = unsafe { RingBuffer::create_raw(region.as_mut_ptr().cast(), 1024, cfg) };
        assert_eq!(raw.err(), Some(AttachError::InvalidConfig(InvalidConfig)));
    }
    let cfg = Config {
        capacity: 64,
        max_readers: 2,
    };
    assert_eq!(cfg.region_len(), Ok(0x80 + 2 * 64 + 64));
    assert!(RingBuffer::create_in_memory(cfg).is_ok());
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
fn test_heap_raw_alignment_after_wrap() {
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
fn test_mmap_alignment_after_wrap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aligned.bin");
    let cfg = Config {
        capacity: 64,
        max_readers: 1,
    };
    // SAFETY: this test exclusively owns the new file throughout its lifetime.
    let rb = unsafe { RingBuffer::create_mmap(&path, cfg) }.unwrap();
    check_payload_alignment(&rb);
    // SAFETY: rb keeps the exclusively owned file live throughout attachment.
    let attached = unsafe { RingBuffer::attach_mmap(&path) }.unwrap();
    check_payload_alignment(&attached);
}

#[test]
fn test_empty_ring_wraps_full_record() {
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
fn test_reader_skips_padding_before_retry() {
    let ring = ring(64, 1);
    let mut reader = ring.view(NoWake).unwrap();
    let mut writer = ring.writer(NoWake).unwrap();
    writer.try_write(&[1; 8]).unwrap();
    assert!(read(&mut reader).is_some());
    assert_eq!(writer.try_write(&[2; 48]), Err(WriteError::WouldBlock));
    assert_eq!(ring.committed(), 64);
    assert_eq!(read(&mut reader), None);
    writer.try_write(&[2; 48]).unwrap();
    assert_eq!(read(&mut reader).as_deref(), Some(&[2; 48][..]));
}

#[test]
fn test_padding_preserves_pins() {
    let ring = ring(64, 1);
    let mut reader = ring.view(NoWake).unwrap();
    let mut writer = ring.writer(NoWake).unwrap();
    writer.try_write(&[1; 8]).unwrap();
    let latest = reader.try_latest().unwrap().unwrap();
    assert_eq!(writer.try_write(&[2; 48]), Err(WriteError::WouldBlock));
    assert_eq!(latest, &[1; 8]);
    assert_eq!(reader.try_latest(), Ok(Some(&[1; 8][..])));
    assert_eq!(reader.cursor(), 0);
    assert!(read(&mut reader).is_some());
    assert_eq!(read(&mut reader), None);
    writer.try_write(&[2; 48]).unwrap();
    assert_eq!(reader.try_latest(), Ok(Some(&[2; 48][..])));
}

#[test]
fn test_registration_during_padding_publication() {
    models::registration_during_padding_publication();
}

#[test]
fn test_reclaim_preserves_reused_slot() {
    models::reclaim_preserves_reused_slot();
}

#[test]
fn test_concurrent_reclaim_preserves_new_owner() {
    models::concurrent_reclaim_preserves_new_owner();
}

#[test]
fn test_drain_excludes_later_records() {
    let ring = ring(256, 1);
    let mut writer = ring.writer(NoWake).expect("writer");
    let mut view = ring.view(NoWake).expect("reader");

    let mut batch = view.drain();
    writer.try_write(b"first").expect("room");
    assert_eq!(batch.next(), None, "an empty drain excludes a later write");

    let mut batch = view.drain();
    writer.try_write(b"later").expect("room");
    assert_eq!(batch.next(), Some(Ok(b"first".as_slice())));
    assert_eq!(batch.next(), None);
    assert_eq!(view.drain().next(), Some(Ok(b"later".as_slice())));
}

#[test]
fn test_drain_excludes_later_wrap() {
    let ring = ring(128, 1);
    let mut writer = ring.writer(NoWake).expect("writer");
    let mut view = ring.view(NoWake).expect("reader");
    writer.try_write(&[1; 32]).expect("room");
    writer.try_write(&[2; 32]).expect("room");
    assert!(read(&mut view).is_some());
    let mut batch = view.drain();
    writer.try_write(&[3; 32]).expect("room after wrap");
    assert_eq!(batch.next(), Some(Ok([2; 32].as_slice())));
    assert_eq!(batch.next(), None);
    view.settle();
    assert_eq!(view.cursor(), 96);
    writer.try_write(&[4; 32]).expect("room");
    let seen: Vec<u8> = view.drain().map(|r| r.unwrap()[0]).collect();
    assert_eq!(seen, [3, 4]);
}

#[test]
fn test_drain_reports_corrupt_once() {
    // An uncommitted length, then one past the data region.
    for len in [32, u64::MAX] {
        let ring = ring(64, 1);
        let mut writer = ring.writer(NoWake).expect("writer");
        let mut view = ring.view(NoWake).expect("reader");
        writer.try_write(&[1; 8]).expect("room");
        // SAFETY: the published header has no concurrent readers or writer.
        unsafe { ring.inner.data_ptr(0).cast::<u64>().write(len) };
        let mut batch = view.drain();
        assert_eq!(batch.next(), Some(Err(ReadError)));
        assert_eq!(batch.next(), None);
    }
}

#[test]
fn test_drain_defers_consumption() {
    let ring = ring(64, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    writer.try_write(&[0; 8]).unwrap();
    writer.try_write(&[1; 8]).unwrap();
    let records: Vec<&[u8]> = view.drain().map(|r| r.unwrap()).collect();
    // Slices outlive the iterator, so the records stay pinned.
    assert_eq!(writer.try_write(&[2; 8]), Err(WriteError::WouldBlock));
    assert_eq!(records, [&[0u8; 8][..], &[1u8; 8]]);
    assert_eq!(view.cursor(), 0);
    assert_eq!(read(&mut view), None);
    assert_eq!(view.cursor(), 2 * frame_len(8) as u64);
    writer.try_write(&[2; 8]).unwrap();
}

#[test]
fn test_drain_break_preserves_unread_records() {
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
fn test_empty_payload_at_ring_boundary() {
    let ring = ring(64, 1);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    writer.try_write(&[1; 17]).unwrap();
    assert!(read(&mut view).is_some());
    assert_eq!(view.cursor(), 48);
    writer.try_write(&[]).unwrap();

    let grant = view.try_read().unwrap().unwrap();
    assert!(grant.is_empty());
    // SAFETY: one-past the data region is valid for this empty payload.
    assert_eq!(grant.as_ptr(), unsafe { ring.inner.data_ptr(64) });
    drop(grant);
    assert_eq!(view.cursor(), 64);
    writer.try_write(b"next lap").unwrap();
    assert_eq!(read(&mut view).as_deref(), Some(&b"next lap"[..]));
}

#[test]
fn test_attach_accepts_extra_storage() {
    let cfg = Config {
        capacity: 64,
        max_readers: 2,
    };
    let mut geometry = Geometry::try_from(cfg).unwrap();
    geometry.data_offset += 32;
    geometry.total_size += 32;
    let backing = Backing::heap(geometry.total_size + 32);
    // SAFETY: fresh aligned storage; the data gap and trailing bytes fit.
    unsafe { init_region(&backing, &geometry) };
    // SAFETY: backing outlives every handle; all later access uses the ring.
    let ring = unsafe { RingBuffer::attach_raw(backing.base, backing.len) }.unwrap();
    let recovered = ring.config();
    assert_eq!(
        (recovered.capacity, recovered.max_readers),
        (cfg.capacity, cfg.max_readers)
    );
    assert_eq!(ring.region().1, backing.len);
    let mut writer = ring.writer(NoWake).unwrap();
    let mut view = ring.view(NoWake).unwrap();
    writer.try_write(b"padded layout").unwrap();
    assert_eq!(read(&mut view).as_deref(), Some(&b"padded layout"[..]));
}

#[test]
fn test_frame_length_rejects_overflow() {
    assert_eq!(geometry(16).frame_len(0), Ok(16));
    assert_eq!(geometry(64).frame_len(48), Ok(64));
    for len in [49, usize::MAX, usize::MAX - 15] {
        assert_eq!(
            geometry(64).frame_len(len),
            Err(WriteError::InsufficientCapacity)
        );
    }
}

fn geometry(capacity: usize) -> Geometry {
    Geometry::try_from(Config {
        capacity,
        max_readers: 1,
    })
    .unwrap()
}

#[cfg(target_pointer_width = "64")]
#[test]
fn test_frame_length_rejects_excess_length() {
    let geometry = geometry(1 << 33);
    assert_eq!(geometry.frame_len(u32::MAX as usize), Ok((1u64 << 32) + 16));
    assert_eq!(
        geometry.frame_len(u32::MAX as usize + 1),
        Err(WriteError::InsufficientCapacity)
    );
}

#[test]
fn test_has_record_with_deferred_drain() {
    let rb = ring(1024, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    assert!(!v.has_record());
    w.try_write(b"one").unwrap();
    assert!(v.has_record());
    assert_eq!(v.drain().count(), 1);
    assert!(
        v.has_record(),
        "a drain defers consumption until it settles"
    );
    v.settle();
    assert!(!v.has_record());
    v.settle();
    assert!(!v.has_record());
}

const fn assert_send<T: Send>() {}
const _: () = assert_send::<Writer<NoWake>>();
const _: () = assert_send::<View<NoWake>>();

fn peer_committed(rb: &RingBuffer, pos: u64) {
    rb.inner.control().committed.store(pos, Release);
}

fn peer_cursor(rb: &RingBuffer, slot: u32, pos: u64) {
    rb.inner.slot(slot).cursor.store(pos, Release);
}

#[test]
fn test_reader_cursor_ahead_of_writer_is_corrupt() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let _v = rb.view(NoWake).unwrap();
    w.try_write(&[1; 8]).unwrap();
    for cursor in [64, u64::MAX - 1] {
        peer_cursor(&rb, 0, cursor);
        assert_eq!(w.try_write(&[2; 8]), Err(WriteError::Corrupt));
    }
    assert_eq!(rb.committed(), 32, "nothing more published");

    peer_cursor(&rb, 0, 32);
    assert_eq!(w.try_write(&[2; 8]), Ok(()), "a cursor at committed fits");
}

#[test]
fn test_writer_ignores_peer_committed() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    w.try_write(&[1; 8]).unwrap();
    peer_committed(&rb, u64::MAX);
    assert_eq!(v.try_read().err(), Some(ReadError));

    w.try_write(&[2; 8]).unwrap();
    assert_eq!(
        rb.committed(),
        64,
        "the writer published from its own position"
    );
    assert_eq!(read(&mut v).as_deref(), Some(&[1; 8][..]));
    assert_eq!(read(&mut v).as_deref(), Some(&[2; 8][..]));
}

#[test]
fn test_misaligned_claim_is_corrupt() {
    let rb = ring(64, 1);
    peer_committed(&rb, 8);
    let mut w = rb.writer(NoWake).unwrap();
    assert_eq!(w.try_write(&[1; 8]), Err(WriteError::Corrupt));

    let mut v = rb.view(NoWake).unwrap();
    peer_committed(&rb, 40);
    assert_eq!(v.try_read().err(), Some(ReadError));
}

#[test]
fn test_peer_cursor_does_not_move_view() {
    let rb = ring(128, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    w.try_write(&[1; 8]).unwrap();
    w.try_write(&[2; 8]).unwrap();
    assert_eq!(read(&mut v).as_deref(), Some(&[1; 8][..]));

    peer_cursor(&rb, 0, 0);
    assert_eq!(v.cursor(), 32);
    assert_eq!(read(&mut v).as_deref(), Some(&[2; 8][..]));
    assert_eq!(
        rb.inner.slot(0).cursor.load(Acquire),
        64,
        "the view republished"
    );
}

#[test]
fn test_max_committed_never_panics() {
    let rb = ring(64, 1);
    let mut w = rb.writer(NoWake).unwrap();
    let mut v = rb.view(NoWake).unwrap();
    w.try_write(&[1; 8]).unwrap();
    for hwm in [HWM_NONE, u64::MAX - 1, 0] {
        peer_committed(&rb, u64::MAX);
        rb.inner.control().hwm.store(hwm, Release);
        assert!(!v.has_record());
        assert_eq!(v.try_read().err(), Some(ReadError));
        assert_eq!(v.try_latest(), Err(ReadError));
        assert_eq!(v.drain().collect::<Vec<_>>(), [Err(ReadError)]);
        v.settle();
        assert_eq!(v.cursor(), 0);
    }
}

#[test]
fn test_data_end_overflow_is_no_gap() {
    let rb = ring(64, 1);
    let v = rb.view(NoWake).unwrap();
    peer_committed(&rb, u64::MAX);
    rb.inner.control().hwm.store(u64::MAX - 16, Release);
    assert_eq!(v.data_end(), u64::MAX);
}
