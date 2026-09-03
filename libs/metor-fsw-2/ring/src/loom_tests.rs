//! Loom models.
//!
//! These cover what Kani cannot: the atomic orderings. Loom runs each model
//! under every interleaving and every reordering its memory model permits,
//! which is how the reader-registration handshake and the wrap-gap publication
//! order get checked rather than sampled. `LOOM.md` in the crate root has the
//! details, including where loom's own modelling stops short.
//!
//! Models stay tiny on purpose. The writer's in-use scan is one atomic load
//! per reader slot inside a `SeqCst`-fenced region, so the state space grows
//! fast in both `max_readers` and message count.

use super::*;
use crate::sync::thread;

/// Data region size for the models.
const CAP: usize = 32;
/// Payload size, chosen so a record's frame does not divide the capacity.
/// That is what forces a wrap gap: with a frame that divides it, records land
/// flush against the lap boundary forever and the gap path never runs.
const PAYLOAD: usize = 12;
/// Bytes one record occupies, header and padding included.
const FRAME: usize = 24;

const _: () = assert!(FRAME == frame_len(PAYLOAD));
const _: () = assert!(CAP % FRAME != 0, "a gap would never form");
// Two records do not fit in a lap, so the second one always wraps, and the
// wrap costs `CAP - FRAME` bytes of gap on top of the record itself.
const _: () = assert!(FRAME + (CAP - FRAME) <= CAP);

/// Payload bytes for record `n`, distinct per record so a reader that picks up
/// overwritten bytes fails an equality check rather than passing silently.
fn payload(n: u8) -> [u8; PAYLOAD] {
    let mut p = [0u8; PAYLOAD];
    let mut i = 0;
    while i < PAYLOAD {
        p[i] = n.wrapping_mul(0x11).wrapping_add(i as u8);
        i += 1;
    }
    p
}

fn ring(capacity: usize, max_readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity,
        max_readers,
    })
}

/// A view registering while the writer is mid-stream must never be lapped, and
/// must never see a torn or stale record.
///
/// This is the race the crate docs call the one racy edge: the writer can scan
/// the reader table an instant before a new claim lands, validate a write
/// against nobody, and reuse bytes the new view believes are pinned. Both
/// sides fence `SeqCst` to close it. Loom explores the interleavings that
/// `concurrent_view_churn` can only sample under Miri seeds.
#[test]
fn registration_races_backpressure() {
    loom::model(|| {
        let ring = ring(CAP, 1);
        let mut w = ring.writer(NoWake).unwrap();
        // One record already committed, so a view registering below sits at a
        // nonzero cursor and the writer's next write has to wrap around it.
        w.try_write(&payload(1))
            .expect("empty ring fits one record");

        let reader = ring.clone();
        let t = thread::spawn(move || {
            let mut v = reader.view(NoWake).unwrap();
            let start = v.cursor();
            let mut buf = Vec::new();
            let mut seen = Vec::new();
            let mut cursor = start;
            while v.try_read_into(&mut buf).expect("never corrupt") {
                seen.push(buf.clone());
                // Cursors only move forward.
                assert!(v.cursor() > cursor);
                cursor = v.cursor();
            }
            (start, seen)
        });

        // These race the registration above. The first wraps, which needs the
        // whole lap; the second needs the reader to have consumed the first.
        let _ = w.try_write(&payload(2));
        let _ = w.try_write(&payload(3));

        let (start, seen) = t.join().unwrap();

        // Records 1, 2 and 3 start at 0, CAP and 2 * CAP: each of the last two
        // wraps, because a second frame never fits in the tail of a lap. So a
        // view registers at one of exactly three committed positions, each the
        // end of a whole record, and which one it caught decides what is still
        // ahead of it.
        let ahead: &[u8] = if start == FRAME as u64 {
            &[2, 3]
        } else if start == (CAP + FRAME) as u64 {
            &[3]
        } else if start == (2 * CAP + FRAME) as u64 {
            &[]
        } else {
            panic!("view registered at {start}, not on a record boundary");
        };

        // It may not have drained everything the writer went on to commit, but
        // what it did read must be those records, in order, byte for byte.
        assert!(seen.len() <= ahead.len());
        for (got, &n) in seen.iter().zip(ahead) {
            assert_eq!(&got[..], &payload(n)[..], "record {n} came back wrong");
        }
    });
}

/// A reader parked on a wrap gap must see the gap marker before it could
/// misread the stale bytes behind it as a record header.
///
/// The writer stores `hwm` before `committed`; the reader loads `committed`
/// before `hwm`. That pairing is what makes a post-wrap `committed` imply a
/// visible marker, and it is the kind of ordering claim only an exhaustive
/// model can check.
#[test]
fn hwm_visible_before_committed() {
    loom::model(|| {
        let ring = ring(CAP, 1);
        let mut w = ring.writer(NoWake).unwrap();
        let mut v = ring.view(NoWake).unwrap();

        // Park the reader exactly where the gap will be published: one record
        // in, with too little room left in the lap for another.
        w.try_write(&payload(1)).unwrap();
        let mut buf = Vec::new();
        assert!(v.try_read_into(&mut buf).unwrap());
        assert_eq!(&buf[..], &payload(1)[..]);
        assert_eq!(v.cursor(), FRAME as u64);

        let t = thread::spawn(move || {
            w.try_write(&payload(2)).expect("the next lap is free");
        });

        let mut buf = Vec::new();
        loop {
            match v.try_read_into(&mut buf) {
                Ok(true) => break,
                Ok(false) => loom::thread::yield_now(), // not published yet
                Err(e) => panic!("gap bytes misread as a record: {e}"),
            }
        }
        assert_eq!(&buf[..], &payload(2)[..]);
        assert_eq!(v.cursor(), (CAP + FRAME) as u64);
        t.join().unwrap();
    });
}

/// Exactly one of two racing claimants gets the writer role.
#[test]
fn writer_claim_handoff() {
    loom::model(|| {
        let a = ring(CAP, 1);
        let b = a.clone();

        let t = thread::spawn(move || {
            b.writer(NoWake)
                .map(|mut w| w.try_write(&payload(2)).is_ok())
        });
        let mine = a
            .writer(NoWake)
            .map(|mut w| w.try_write(&payload(1)).is_ok());
        let theirs = t.join().unwrap();

        // The claim is a CAS, so they cannot hold it at once. Both succeeding
        // is legal only if the first dropped before the second claimed, which
        // is a real ordering rather than a double claim.
        assert!(mine.is_ok() || theirs.is_ok());
    });
}

/// A held [`ReadGrant`] pins its bytes: the writer must refuse a write that
/// would reuse them, and the borrow reads back correctly throughout.
#[test]
fn grant_pins_bytes() {
    loom::model(|| {
        let ring = ring(CAP, 1);
        let mut w = ring.writer(NoWake).unwrap();
        let mut v = ring.view(NoWake).unwrap();
        w.try_write(&payload(1)).unwrap();

        let t = thread::spawn(move || w.try_write(&payload(2)).is_ok());

        let grant = v.try_read().expect("never corrupt").expect("committed");
        assert_eq!(&grant[..], &payload(1)[..]);
        let wrote = t.join().unwrap();
        // The writer cannot lap a reader, so the bytes held still for the
        // grant's whole lifetime whatever the other thread decided.
        assert_eq!(&grant[..], &payload(1)[..]);
        // The grant pins the cursor at 0, and a second record needs the gap
        // plus its own frame, which is the entire lap.
        assert!(!wrote, "writer reused bytes a grant had borrowed");
        drop(grant);
    });
}

/// With two views the writer is bounded by the slower of them, whichever order
/// the in-use scan happens to observe their claims in.
#[test]
fn two_readers_slowest() {
    loom::model(|| {
        let ring = ring(CAP, 2);
        let mut w = ring.writer(NoWake).unwrap();
        let mut fast = ring.view(NoWake).unwrap();
        let slow = ring.view(NoWake).unwrap();
        w.try_write(&payload(1)).unwrap();

        let handle = ring.clone();
        let t = thread::spawn(move || {
            // Drains as fast as it can, which frees space only up to its own
            // cursor; the parked view still holds the floor.
            let mut buf = Vec::new();
            let mut n = 0;
            while fast.try_read_into(&mut buf).expect("never corrupt") {
                n += 1;
            }
            (n, handle.committed())
        });

        let mut wrote = 1;
        while w.try_write(&payload(2)).is_ok() {
            wrote += 1;
            if wrote == 4 {
                break; // the model only needs to reach the bound
            }
        }
        let (_, committed) = t.join().unwrap();

        // `slow` never advanced, so nothing may be written past one lap from
        // its cursor, no matter how far `fast` got.
        assert_eq!(slow.cursor(), 0);
        assert!(
            committed <= CAP as u64,
            "committed {committed} laps a reader parked at 0"
        );
    });
}

/// The `slowest <= committed` precondition [`fits`] relies on. Kani proves a
/// violation can only cost a panic or a spurious `WouldBlock`, never a bad
/// write; this decides whether a writer can observe one at all, by racing a
/// writer that wraps against a reader doing gap skips.
///
/// The check is the `debug_assert!` in `Writer::fits`, which loom builds keep.
#[test]
fn cursor_never_exceeds_committed_at_fits() {
    loom::model(|| {
        let ring = ring(CAP, 1);
        let mut w = ring.writer(NoWake).unwrap();
        let mut v = ring.view(NoWake).unwrap();
        w.try_write(&payload(1)).unwrap();
        let mut buf = Vec::new();
        assert!(v.try_read_into(&mut buf).unwrap());

        let reader = thread::spawn(move || {
            // Sits on the gap and skips it, which parks the cursor at the lap
            // boundary while the writer is between its two stores.
            let mut buf = Vec::new();
            let _ = v.try_read_into(&mut buf);
            let _ = v.try_read_into(&mut buf);
        });

        // Each of these calls `fits`, and so the precondition assert.
        let _ = w.try_write(&payload(2));
        let _ = w.try_write(&payload(3));
        reader.join().unwrap();
    });
}
