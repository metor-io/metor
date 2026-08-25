//! Kani proof harnesses.
//!
//! These cover the sequential half of the crate's correctness argument: the
//! position arithmetic and the geometry validation that every `unsafe` deref
//! in `lib.rs` cites. Kani is a bounded model checker with no notion of
//! threads, so nothing here says anything about the atomic orderings; those
//! belong to the loom models. `KANI.md` in the crate root has the full split.
//!
//! Symbolic capacities are capped at 2^20 rather than left open. The
//! predicates are all mask arithmetic, so every power of two behaves alike
//! above the point where a record fits, and the cap keeps the solver honest.

use super::*;

/// Capacity used by the harnesses that drive a real ring. Small enough that a
/// handful of writes wraps it, which is where the interesting paths are.
const CAP: usize = 32;
/// Bytes in a `CAP`-sized, single-reader region.
const REGION: usize = HEADER_SIZE + READER_SLOT_SIZE + CAP;

/// A symbolic power-of-two capacity in the range the ring supports.
fn any_capacity() -> u64 {
    let cap: u64 = kani::any();
    kani::assume(cap.is_power_of_two() && cap >= 8 && cap <= 1 << 20);
    cap
}

/// A symbolic record size: 8-aligned, at least a bare header, at most a lap.
fn any_record(cap: u64) -> u64 {
    let rec: u64 = kani::any();
    kani::assume(rec >= 8 && rec <= cap && rec.is_multiple_of(8));
    rec
}

// ---------------------------------------------------------------------------
// Tier A: position arithmetic
// ---------------------------------------------------------------------------

#[kani::proof]
fn round_up8_correct() {
    let n: usize = kani::any();
    kani::assume(n <= usize::MAX - 7);
    let r = round_up8(n);
    assert!(r.is_multiple_of(8));
    // Stated as a difference: at the top of the admissible range `n + 8` is
    // itself the overflow this function is being checked against.
    assert!(r >= n);
    assert!(r - n < 8);
}

#[kani::proof]
fn frame_len_correct() {
    let n: usize = kani::any();
    kani::assume(n <= usize::MAX - 15);
    let f = frame_len(n);
    assert_eq!(f, 8 + round_up8(n));
    assert!(f.is_multiple_of(8));
    assert!(f >= 8);
    assert!(f - 8 >= n);
    assert!(f - n < 16);
}

/// The bound `locate` puts on a length field read out of the region is
/// sufficient for the `slice::from_raw_parts` that follows it: header, payload
/// and padding all stay inside the data region. This is the safety contract of
/// [`View::try_read`] and [`View::try_latest`], discharged for every length a
/// corrupt region could present.
#[kani::proof]
fn straddle_bound_is_sufficient() {
    let cap = any_capacity();
    let phys: u64 = kani::any();
    kani::assume(phys < cap && phys.is_multiple_of(8));
    // `read_len` masks the header down to 32 bits, so that is the whole range
    // of lengths any region can produce.
    let len: u64 = kani::any();
    kani::assume(len <= u32::MAX as u64);

    if record_fits(len, phys, cap) {
        assert!(phys + 8 <= cap); // the header itself
        assert!(phys + 8 + len <= cap); // the payload slice
        assert!(phys + 8 + round_up8_u64(len) <= cap); // and its padding
    }
}

/// The same bound also keeps `frame_len` from wrapping on a 32-bit target,
/// where `usize` is exactly the `u32` the length field holds. Miri needed a
/// whole i686 run to execute this path for one input; here it holds for all of
/// them, on any host.
#[kani::proof]
fn straddle_bound_blocks_32bit_overflow() {
    let cap = any_capacity();
    let phys: u64 = kani::any();
    kani::assume(phys < cap && phys.is_multiple_of(8));
    let len: u64 = kani::any();
    kani::assume(len <= u32::MAX as u64);

    if record_fits(len, phys, cap) {
        // `round_up8` then `frame_len`, in 32-bit arithmetic.
        let n = len as u32;
        let padded = n.checked_add(7).expect("round_up8 would wrap") & !7u32;
        let frame = padded.checked_add(8).expect("frame_len would wrap");
        // And it agrees with the 64-bit computation the check was made in.
        assert_eq!(frame as u64, 8 + round_up8_u64(len));
    }
}

/// A reserved record never straddles the wrap, and reserving only ever moves
/// the write position forward, by less than one lap. This discharges
/// `Writer::commit`'s contiguity precondition.
#[kani::proof]
fn reserve_never_straddles() {
    let cap = any_capacity();
    let rec = any_record(cap);
    let committed: u64 = kani::any();
    kani::assume(committed.is_multiple_of(8) && committed <= u64::MAX - 2 * cap);

    let (start, gap) = reserve(committed, rec, cap);

    assert_eq!(start - committed, gap);
    assert!(gap < cap);
    assert!(start.is_multiple_of(8));
    // The record is contiguous: it fits between its own start and the end of
    // the lap, so `write_record` never runs off the data region.
    assert!((start & (cap - 1)) + rec <= cap);
}

/// Backpressure is sound and not spurious: a write that passes `fits` leaves
/// the slowest reader within one lap of the new committed position, so no byte
/// it has yet to consume is reused; and one that fails really did not fit.
#[kani::proof]
fn fits_implies_no_lap() {
    let cap = any_capacity();
    let rec = any_record(cap);
    let committed: u64 = kani::any();
    kani::assume(committed.is_multiple_of(8) && committed <= u64::MAX - 4 * cap);
    let slowest: u64 = kani::any();
    kani::assume(slowest <= committed);

    let (start, gap) = reserve(committed, rec, cap);
    let need = gap + rec;

    if fits(committed, slowest, need, cap) {
        assert!(start + rec - slowest <= cap);
    } else {
        assert!(committed - slowest + need > cap);
    }

    // With no readers registered the writer passes its own position as
    // `slowest`, leaving capacity as the only bound.
    assert_eq!(fits(committed, committed, need, cap), need <= cap);
}

/// [`fits`]'s `slowest <= committed` precondition is load-bearing, not
/// decorative. `locate`'s wrap-gap skip parks a cursor ahead of `committed` by
/// design, so a violation is a real edge rather than a hypothetical one.
///
/// What this proves is that violating it is never a *safety* problem: outside
/// the precondition the subtraction wraps, and the result either overflows the
/// sum (a panic in a debug build) or reads as "full" (a spurious `WouldBlock`).
/// It never reports space that is not there. Whether a writer can observe the
/// edge at all is a question about interleavings, which loom model
/// `cursor_never_exceeds_committed_at_fits` answers.
#[kani::proof]
fn fits_precondition_is_tight() {
    let cap = any_capacity();
    let committed: u64 = kani::any();
    kani::assume(committed <= u64::MAX - 4 * cap);
    let slowest: u64 = kani::any();
    // A cursor is a published position, never more than a lap out of step.
    kani::assume(slowest <= committed + cap);
    let need: u64 = kani::any();
    kani::assume(need >= 8 && need <= 2 * cap);

    if slowest <= committed {
        // Inside the precondition the arithmetic `fits` performs is total.
        assert!((committed - slowest).checked_add(need).is_some());
    } else {
        let raw = committed.wrapping_sub(slowest);
        assert!(raw.checked_add(need).is_none_or(|sum| sum > cap));
    }
}

/// `round_up8` in `u64`, so the lemmas above stay independent of the host's
/// pointer width.
fn round_up8_u64(n: u64) -> u64 {
    (n + 7) & !7
}

// ---------------------------------------------------------------------------
// Tier B: geometry, against a fully hostile header
// ---------------------------------------------------------------------------

/// A region header with every field independently symbolic: the whole space a
/// corrupt mapping or a hostile peer could present.
fn any_header() -> RegionHeader {
    RegionHeader {
        magic: kani::any(),
        version: kani::any(),
        flags: kani::any(),
        capacity: kani::any(),
        data_offset: kani::any(),
        max_readers: kani::any(),
        reader_table_offset: kani::any(),
        total_size: kani::any(),
        arch_tag: kani::any(),
    }
}

/// No header that [`validate_header`] accepts can put any offset the ring
/// dereferences outside the region. Together the assertions below are the
/// standing justification for `Inner::control`, `Inner::slot` and
/// `Inner::data_ptr`; the five `attach_rejects_*` unit tests sample this
/// property, and this proves it.
#[kani::proof]
fn validate_header_hostile() {
    let hdr = any_header();
    let region_len: usize = kani::any();

    let Ok(g) = validate_header(&hdr, region_len) else {
        return;
    };

    // Capacity is maskable, holds a record header, and fits this target.
    assert!(g.capacity.is_power_of_two());
    assert!(g.capacity >= 8);
    assert!(g.max_readers > 0);

    // The reader table sits behind the fixed header, 8-aligned, and ends at or
    // before the data region.
    assert!(g.reader_table_offset >= HEADER_SIZE);
    assert!(g.reader_table_offset.is_multiple_of(8));
    let table_end = g
        .reader_table_offset
        .checked_add((g.max_readers as usize).checked_mul(READER_SLOT_SIZE).unwrap())
        .unwrap();
    assert!(table_end <= g.data_offset);

    // The data region is 8-aligned and ends inside the backing.
    assert!(g.data_offset.is_multiple_of(8));
    let data_end = g.data_offset.checked_add(g.capacity as usize).unwrap();
    assert!(data_end <= region_len);

    // Which makes the whole region at least as large as the fixed header, so
    // the control block is addressable too.
    assert!(region_len >= HEADER_SIZE);
    assert!(OFF_CONTROL + size_of::<Control>() <= HEADER_SIZE);
}

/// Every reader slot the ring will index lies wholly inside the reader table,
/// and so inside the region. Discharges `Inner::slot`.
#[kani::proof]
fn slot_offsets_in_bounds() {
    let hdr = any_header();
    let region_len: usize = kani::any();
    let Ok(g) = validate_header(&hdr, region_len) else {
        return;
    };

    let slot: u32 = kani::any();
    kani::assume(slot < g.max_readers);

    let off = g.reader_table_offset + slot as usize * READER_SLOT_SIZE;
    assert!(off.is_multiple_of(8));
    assert!(off + READER_SLOT_SIZE <= g.data_offset);
    assert!(off + READER_SLOT_SIZE <= region_len);
}

/// Every physical data offset the ring will form lies inside the region.
/// Discharges `Inner::data_ptr`, and with it the `phys + 8 <= capacity`
/// contract `read_len` relies on for an 8-aligned record start.
#[kani::proof]
fn data_ptr_in_bounds() {
    let hdr = any_header();
    let region_len: usize = kani::any();
    let Ok(g) = validate_header(&hdr, region_len) else {
        return;
    };

    let phys: u64 = kani::any();
    kani::assume(phys < g.capacity);

    assert!(g.data_offset + (phys as usize) < region_len);
    if phys.is_multiple_of(8) {
        assert!(phys + 8 <= g.capacity);
    }
}

/// Creating and attaching agree: any config `layout` accepts produces a header
/// `validate_header` accepts, with the identical geometry.
#[kani::proof]
fn layout_roundtrip() {
    let capacity: usize = kani::any();
    kani::assume(capacity.is_power_of_two() && capacity >= 8 && capacity <= 1 << 20);
    let max_readers: usize = kani::any();
    kani::assume(max_readers >= 1 && max_readers <= 64);

    let cfg = Config {
        capacity,
        max_readers,
    };
    let (reader_table_offset, data_offset, total) = layout(&cfg);

    let hdr = RegionHeader {
        magic: MAGIC,
        version: VERSION,
        flags: 0,
        capacity: capacity as u64,
        data_offset: data_offset as u64,
        max_readers: max_readers as u32,
        reader_table_offset: reader_table_offset as u32,
        total_size: total as u64,
        arch_tag: arch_tag(),
    };

    let g = validate_header(&hdr, total).expect("a header we just laid out");
    assert_eq!(g.capacity, capacity as u64);
    assert_eq!(g.data_offset, data_offset);
    assert_eq!(g.reader_table_offset, reader_table_offset);
    assert_eq!(g.max_readers, max_readers as u32);
}

// ---------------------------------------------------------------------------
// Tier C: bounded operational proofs on a real ring
// ---------------------------------------------------------------------------

/// A region in the harness's own frame, 8-aligned and exactly the size
/// `layout` computes for `(CAP, 1)`.
///
/// The harnesses below attach to one of these rather than calling
/// `create_in_memory`. That constructor builds its backing by collecting an
/// iterator into a `Box<[Word]>`, and behind that collect sit `RawVec` growth,
/// reallocation and allocation-failure paths — all of which CBMC unrolls to
/// the harness's unwind bound. It costs far more than everything these proofs
/// are actually about. Attaching keeps the real geometry, the real header
/// validation and the real reader and writer paths, and leaves only the `Arc`
/// inside `RingBuffer` allocating at all.
#[repr(C, align(8))]
struct Region([u8; REGION]);

impl Region {
    fn new() -> Self {
        Region([0u8; REGION])
    }

    /// Lay out and attach. The ring holds a raw pointer into `self`, so the
    /// region has to outlive it: declare it first, and it drops last.
    fn attach(&mut self) -> RingBuffer {
        let cfg = Config {
            capacity: CAP,
            max_readers: 1,
        };
        let (reader_table_offset, data_offset, total) = layout(&cfg);
        assert_eq!(total, REGION);
        let base = self.0.as_mut_ptr();
        // SAFETY: `base` covers exactly `REGION` bytes and is 8-aligned by the
        // `repr(align(8))`, which is the region contract `Backing::raw` and
        // `attach_raw` require. It is exclusively borrowed here.
        unsafe {
            let backing = Backing::raw(base, REGION);
            init_region(&backing, &cfg, reader_table_offset, data_offset, total);
            RingBuffer::attach_raw(base, REGION).expect("a region we just laid out")
        }
    }
}

/// A record read back is the record written, byte for byte, and consuming it
/// advances the cursor by exactly its frame length.
#[kani::proof]
#[kani::unwind(12)]
fn write_read_roundtrip() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut w = ring.writer(NoWake).unwrap();
    let mut v = ring.view(NoWake).unwrap();

    let bytes: [u8; 8] = kani::any();
    let len: usize = kani::any();
    kani::assume(len <= 8);

    w.try_write(&bytes[..len]).expect("empty ring fits a record");
    let grant = v.try_read().unwrap().expect("a record was committed");
    assert_eq!(grant.len(), len);
    assert_eq!(&grant[..], &bytes[..len]);
    drop(grant);

    assert_eq!(v.cursor(), frame_len(len) as u64);
    assert!(v.try_read().unwrap().is_none());
}

/// Backpressure is exact at the byte level: a write succeeds precisely when
/// the record fits behind the reader, and a rejected write leaves the ring
/// untouched. Generalizes the single hard-coded case in `tests.rs`.
#[kani::proof]
#[kani::unwind(12)]
fn backpressure_is_exact() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut w = ring.writer(NoWake).unwrap();
    let v = ring.view(NoWake).unwrap();

    let a: usize = kani::any();
    let b: usize = kani::any();
    kani::assume(a <= CAP && b <= CAP);

    let first = w.try_write(&[0u8; CAP][..a]);
    let used = if frame_len(a) > CAP {
        assert_eq!(first, Err(WriteError::InsufficientCapacity));
        0
    } else {
        assert_eq!(first, Ok(()));
        frame_len(a)
    };
    assert_eq!(v.committed(), used as u64);

    // The reader has consumed nothing, so the second write is bounded by what
    // the first left behind.
    let second = w.try_write(&[0u8; CAP][..b]);
    let (start, gap) = reserve(used as u64, frame_len(b) as u64, CAP as u64);
    if frame_len(b) > CAP {
        assert_eq!(second, Err(WriteError::InsufficientCapacity));
        assert_eq!(v.committed(), used as u64);
    } else if used as u64 + gap + frame_len(b) as u64 <= CAP as u64 {
        assert_eq!(second, Ok(()));
        assert_eq!(v.committed(), start + frame_len(b) as u64);
    } else {
        assert_eq!(second, Err(WriteError::WouldBlock));
        assert_eq!(v.committed(), used as u64); // rejection is inert
    }
}

/// A reader whose cursor lands exactly on a wrap gap reads through it to the
/// record on the next lap, with the right bytes, for every record size that
/// can produce a gap. `tests.rs` covers the one size that fits its hard-coded
/// geometry; this covers all of them.
///
/// It also bounds `locate`'s gap-skip loop. Kani's unwind bound is per-harness
/// rather than per-loop, so the claim it discharges is the weaker "every loop
/// here closes", but a gap skip that could re-trigger would not close at all —
/// the doc comment at the skip argues it cannot, because `hwm` strictly
/// increases.
#[kani::proof]
#[kani::unwind(12)]
fn wrap_gap_skip_reads_through() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut w = ring.writer(NoWake).unwrap();
    let mut v = ring.view(NoWake).unwrap();

    // A first record of symbolic size, consumed, so the cursor sits mid-lap.
    let a: usize = kani::any();
    kani::assume(a <= 8);
    w.try_write(&[1u8; 8][..a]).unwrap();
    let mut buf = Vec::new();
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(v.cursor(), frame_len(a) as u64);

    // A second record sized so it cannot fit contiguously in what is left of
    // the lap, which is exactly the condition that publishes a gap.
    let b: usize = kani::any();
    kani::assume(b <= 8);
    let used = frame_len(a);
    let rem = CAP - used;
    kani::assume(frame_len(b) > rem);

    let payload = [2u8; 8];
    w.try_write(&payload[..b]).expect("the lap ahead is free");

    // The reader is parked on the gap. It must skip it and serve the record
    // from the next lap, not misread the gap bytes as a header.
    assert!(v.try_read_into(&mut buf).unwrap());
    assert_eq!(&buf[..], &payload[..b]);
    assert_eq!(v.cursor(), (CAP + frame_len(b)) as u64);
    assert!(!v.try_read_into(&mut buf).unwrap());
}

/// `try_latest` serves the newest record and pins it: the cursor parks at its
/// start, so calling again with nothing new committed returns the same bytes.
#[kani::proof]
#[kani::unwind(12)]
fn try_latest_pins() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut w = ring.writer(NoWake).unwrap();
    let mut v = ring.view(NoWake).unwrap();

    let x: u8 = kani::any();
    let y: u8 = kani::any();
    w.try_write(&[x]).unwrap();
    w.try_write(&[y]).unwrap();

    let g = v.try_latest().unwrap().expect("two records committed");
    assert_eq!(&g[..], &[y]);
    let pinned = g.end_abs;
    drop(g);
    assert_eq!(v.cursor(), pinned);

    let again = v.try_latest().unwrap().expect("still the newest");
    assert_eq!(&again[..], &[y]);
}

// ---------------------------------------------------------------------------
// Tier D: symbolic corruption
// ---------------------------------------------------------------------------

/// Overwrite the whole data region with symbolic bytes and drive the read
/// paths over it. Whatever a corrupt mapping contains, a read either reports
/// [`ReadError::Corrupt`], reports caught-up, or hands back a slice that lies
/// wholly inside the data region. It never reads out of bounds and never
/// panics: Kani's memory-safety checks carry the former, the assertions pin
/// the latter. This is the property `ReadError::Corrupt` exists for.
#[kani::proof]
#[kani::unwind(12)]
fn corrupt_data_never_ub() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut v = ring.view(NoWake).unwrap();
    let (base, region_len) = ring.region();
    assert_eq!(region_len, REGION);

    // SAFETY: we hold the only handle, and the offsets come from the geometry
    // this region was created with.
    unsafe {
        // A word at a time rather than a byte at a time: the same 32
        // arbitrary bytes, in a quarter of the loop iterations CBMC has to
        // unroll. The data region is 8-aligned and a multiple of 8 long.
        for i in 0..CAP / 8 {
            base.add(REGION - CAP)
                .cast::<u64>()
                .add(i)
                .write(kani::any());
        }
        // Claim a committed position somewhere in the region, as a writer that
        // died mid-lap would leave behind.
        let committed: u64 = kani::any();
        kani::assume(committed <= 4 * CAP as u64 && committed.is_multiple_of(8));
        let hwm: u64 = kani::any();
        kani::assume(hwm <= 4 * CAP as u64 || hwm == HWM_NONE);
        base.add(OFF_CONTROL).cast::<u64>().write(committed);
        base.add(OFF_CONTROL + 8).cast::<u64>().write(hwm);
    }

    match v.try_read() {
        Err(ReadError::Corrupt) | Ok(None) => {}
        Ok(Some(g)) => {
            // A served record fits in the region with room for its header.
            assert!(g.len() <= CAP - 8);
        }
    }
}

/// The same, with the data region left zeroed so the control words are the
/// whole adversary. Covers the `r == hwm` and `r >= c` branches of `locate`
/// against values no cooperating writer would publish.
///
/// Kept to the borrowing path. `try_read_into` copies through a `Vec`, which
/// drags the allocator model into the formula, and `try_latest` loops until it
/// reaches the newest record; either on top of three symbolic control words
/// puts this beyond CBMC's reach. `locate` is the whole of the pointer
/// arithmetic those three paths share, so bounding it bounds all of them.
#[kani::proof]
#[kani::unwind(12)]
fn corrupt_control_never_ub() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut v = ring.view(NoWake).unwrap();
    let (base, _) = ring.region();

    let committed: u64 = kani::any();
    let hwm: u64 = kani::any();
    let cursor: u64 = kani::any();
    kani::assume(committed <= 2 * CAP as u64 && committed.is_multiple_of(8));
    kani::assume(hwm <= 2 * CAP as u64 || hwm == HWM_NONE);
    kani::assume(cursor <= 2 * CAP as u64 && cursor.is_multiple_of(8));

    // SAFETY: sole handle; offsets from this region's own geometry.
    unsafe {
        base.add(OFF_CONTROL).cast::<u64>().write(committed);
        base.add(OFF_CONTROL + 8).cast::<u64>().write(hwm);
        base.add(HEADER_SIZE).cast::<u64>().write(cursor);
    }

    match v.try_read() {
        Err(ReadError::Corrupt) | Ok(None) => {}
        Ok(Some(g)) => assert!(g.len() <= CAP - 8),
    }
}

/// `try_latest` over a corrupt region, with the control words pinned to a
/// consistent pair so its skip-to-newest loop is the only symbolic thing left.
/// The data region stays fully symbolic, which is what its `locate` call has
/// to survive.
#[kani::proof]
#[kani::unwind(12)]
fn corrupt_latest_never_ub() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut v = ring.view(NoWake).unwrap();
    let (base, _) = ring.region();

    // SAFETY: sole handle; offsets from this region's own geometry.
    unsafe {
        // A word at a time rather than a byte at a time: the same 32
        // arbitrary bytes, in a quarter of the loop iterations CBMC has to
        // unroll. The data region is 8-aligned and a multiple of 8 long.
        for i in 0..CAP / 8 {
            base.add(REGION - CAP)
                .cast::<u64>()
                .add(i)
                .write(kani::any());
        }
        base.add(OFF_CONTROL).cast::<u64>().write(CAP as u64);
    }

    match v.try_latest() {
        Err(ReadError::Corrupt) | Ok(None) => {}
        Ok(Some(g)) => assert!(g.len() <= CAP - 8),
    }
}
