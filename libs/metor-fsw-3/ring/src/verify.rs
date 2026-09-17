//! Sequential Kani harnesses; see `KANI.md` for assumptions and bounds.
use super::*;

const CAP: usize = 64;
const REGION: usize = HEADER_SIZE + READER_SLOT_SIZE + CAP;

fn any_capacity() -> u64 {
    let cap: u64 = kani::any();
    kani::assume(cap.is_power_of_two() && cap >= 16 && cap <= 1 << 20);
    cap
}

fn any_record(cap: u64) -> u64 {
    let rec: u64 = kani::any();
    kani::assume(rec >= 16 && rec <= cap && rec.is_multiple_of(16));
    rec
}

// ---------------------------------------------------------------------------
// Tier A: position arithmetic
// ---------------------------------------------------------------------------

#[kani::proof]
fn round_up16_correct() {
    let n: usize = kani::any();
    kani::assume(n <= usize::MAX - 15);
    let r = round_up16(n);
    assert!(r.is_multiple_of(16));
    // Subtraction avoids overflow in the assertion near `usize::MAX`.
    assert!(r >= n);
    assert!(r - n < 16);
}

#[kani::proof]
fn frame_len_correct() {
    let n: usize = kani::any();
    kani::assume(n <= usize::MAX - 31);
    let f = frame_len(n);
    assert_eq!(f, 16 + round_up16(n));
    assert!(f.is_multiple_of(16));
    assert!(f >= 16);
    assert!(f - 16 >= n);
    assert!(f - n < 32);
}

#[kani::proof]
fn straddle_bound_is_sufficient() {
    let cap = any_capacity();
    let phys: u64 = kani::any();
    kani::assume(phys < cap && phys.is_multiple_of(16));
    // The reader takes the low 32 bits of the length word.
    let len: u64 = kani::any();
    kani::assume(len <= u32::MAX as u64);

    if record_fits(len, phys, cap) {
        assert!(phys + 16 <= cap); // the header itself
        assert!(phys + 16 + len <= cap); // the payload slice
        assert!(phys + 16 + round_up16_u64(len) <= cap); // and its padding
    }
}

#[kani::proof]
fn straddle_bound_blocks_32bit_overflow() {
    let cap = any_capacity();
    let phys: u64 = kani::any();
    kani::assume(phys < cap && phys.is_multiple_of(16));
    let len: u64 = kani::any();
    kani::assume(len <= u32::MAX as u64);

    if record_fits(len, phys, cap) {
        // `round_up16` then `frame_len`, in 32-bit arithmetic.
        let n = len as u32;
        let padded = n.checked_add(15).expect("round_up16 would wrap") & !15u32;
        let frame = padded.checked_add(16).expect("frame_len would wrap");
        // And it agrees with the 64-bit computation the check was made in.
        assert_eq!(frame as u64, 16 + round_up16_u64(len));
    }
}

#[kani::proof]
fn reserve_never_straddles() {
    let cap = any_capacity();
    let rec = any_record(cap);
    let committed: u64 = kani::any();
    kani::assume(committed.is_multiple_of(16) && committed <= u64::MAX - 2 * cap);

    let (start, gap) = reserve(committed, rec, cap);

    assert_eq!(start - committed, gap);
    assert!(gap < cap);
    assert!(start.is_multiple_of(16));
    // The whole frame fits before the physical end.
    assert!((start & (cap - 1)) + rec <= cap);
}

#[kani::proof]
fn padding_allows_empty_ring_progress() {
    let cap = any_capacity();
    let rec = any_record(cap);
    let committed: u64 = kani::any();
    kani::assume(committed.is_multiple_of(16) && committed <= u64::MAX - 2 * cap);
    let (start, gap) = reserve(committed, rec, cap);
    if !fits(committed, committed, gap + rec, cap) {
        assert!(gap > 0);
        assert!(fits(committed, committed, gap, cap));
        assert_eq!(start & (cap - 1), 0);
        assert!(fits(start, start, rec, cap));
    }
}

#[kani::proof]
fn fits_implies_no_lap() {
    let cap = any_capacity();
    let rec = any_record(cap);
    let committed: u64 = kani::any();
    kani::assume(committed.is_multiple_of(16) && committed <= u64::MAX - 4 * cap);
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

#[kani::proof]
fn fits_checked_arithmetic_outside_precondition() {
    let cap = any_capacity();
    let committed: u64 = kani::any();
    kani::assume(committed <= u64::MAX - 4 * cap);
    let slowest: u64 = kani::any();
    // A cursor is a published position, never more than a lap out of step.
    kani::assume(slowest <= committed + cap);
    let need: u64 = kani::any();
    kani::assume(need >= 16 && need <= 2 * cap);

    if slowest <= committed {
        // Inside the precondition the arithmetic `fits` performs is total.
        assert!((committed - slowest).checked_add(need).is_some());
    } else {
        // Overflow here can wrap to a successful fit in release builds.
        // This checks checked arithmetic, not the protocol precondition.
        let raw = committed.wrapping_sub(slowest);
        assert!(raw.checked_add(need).is_none_or(|sum| sum > cap));
    }
}

fn round_up16_u64(n: u64) -> u64 {
    (n + 15) & !15
}

// ---------------------------------------------------------------------------
// Tier B: geometry, against a fully hostile header
// ---------------------------------------------------------------------------

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

#[kani::proof]
fn validate_header_hostile() {
    let hdr = any_header();
    let region_len: usize = kani::any();

    let Ok(g) = validate_header(&hdr, region_len) else {
        return;
    };

    // Capacity is maskable, holds a record header, and fits this target.
    assert!(g.capacity.is_power_of_two());
    assert!(g.capacity >= 16);
    assert!(g.max_readers > 0);

    // The reader table sits behind the fixed header, 8-aligned, and ends at or
    // before the data region.
    assert!(g.reader_table_offset >= HEADER_SIZE);
    assert!(g.reader_table_offset.is_multiple_of(8));
    let table_end = g
        .reader_table_offset
        .checked_add(
            (g.max_readers as usize)
                .checked_mul(READER_SLOT_SIZE)
                .unwrap(),
        )
        .unwrap();
    assert!(table_end <= g.data_offset);

    // The data region is 16-aligned and ends inside the backing.
    assert!(g.data_offset.is_multiple_of(16));
    let data_end = g.data_offset.checked_add(g.capacity as usize).unwrap();
    assert!(data_end <= region_len);

    // The control block also fits.
    assert!(region_len >= HEADER_SIZE);
    assert!(OFF_CONTROL + size_of::<Control>() <= HEADER_SIZE);
}

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

#[kani::proof]
fn data_ptr_in_bounds() {
    let hdr = any_header();
    let region_len: usize = kani::any();
    let Ok(g) = validate_header(&hdr, region_len) else {
        return;
    };

    let phys: u64 = kani::any();
    kani::assume(phys <= g.capacity);

    assert!(g.data_offset + (phys as usize) <= region_len);
    if phys < g.capacity && phys.is_multiple_of(16) {
        assert!(phys + 16 <= g.capacity);
    }
}

#[kani::proof]
fn layout_roundtrip() {
    let capacity: usize = kani::any();
    kani::assume(capacity.is_power_of_two() && capacity >= 16 && capacity <= 1 << 20);
    let max_readers: usize = kani::any();
    kani::assume(max_readers >= 1 && max_readers <= 64);

    let cfg = Config {
        capacity,
        max_readers,
    };
    let geometry = layout(&cfg);

    let hdr = RegionHeader {
        magic: MAGIC,
        version: VERSION,
        flags: 0,
        capacity: capacity as u64,
        data_offset: geometry.data_offset as u64,
        max_readers: max_readers as u32,
        reader_table_offset: geometry.reader_table_offset as u32,
        total_size: geometry.total_size as u64,
        arch_tag: arch_tag(),
    };

    let g = validate_header(&hdr, geometry.total_size).expect("a header we just laid out");
    assert_eq!(g.capacity, capacity as u64);
    assert_eq!(g.data_offset, geometry.data_offset);
    assert_eq!(g.reader_table_offset, geometry.reader_table_offset);
    assert_eq!(g.total_size, geometry.total_size);
    assert_eq!(g.max_readers, max_readers as u32);
}

// ---------------------------------------------------------------------------
// Tier C: bounded operational proofs on a real ring
// ---------------------------------------------------------------------------

#[repr(C, align(16))]
struct Region([u8; REGION]);

impl Region {
    fn new() -> Self {
        Region([0u8; REGION])
    }

    fn attach(&mut self) -> RingBuffer {
        let cfg = Config {
            capacity: CAP,
            max_readers: 1,
        };
        let geometry = layout(&cfg);
        assert_eq!(geometry.total_size, REGION);
        let base = self.0.as_mut_ptr();
        // SAFETY: the aligned region is exclusively held and outlives its handles.
        unsafe {
            let backing = Backing::raw(base, REGION);
            init_region(&backing, &geometry);
            RingBuffer::attach_raw(base, REGION).expect("a region we just laid out")
        }
    }
}

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

    w.try_write(&bytes[..len])
        .expect("empty ring fits a record");
    let grant = v.try_read().unwrap().expect("a record was committed");
    assert_eq!(grant.len(), len);
    assert_eq!(&grant[..], &bytes[..len]);
    drop(grant);

    assert_eq!(v.cursor(), frame_len(len) as u64);
    assert!(v.try_read().unwrap().is_none());
}

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
        let published = if gap > 0 && used as u64 + gap <= CAP as u64 {
            start
        } else {
            used as u64
        };
        assert_eq!(v.committed(), published);
    }
}

#[kani::proof]
#[kani::unwind(12)]
fn wrap_gap_skip_reads_through() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut w = ring.writer(NoWake).unwrap();
    let mut v = ring.view(NoWake).unwrap();

    // A first record of symbolic size, consumed, so the cursor sits mid-lap.
    let a: usize = kani::any();
    kani::assume(a <= 17);
    w.try_write(&[1u8; 17][..a]).unwrap();
    drop(v.try_read().unwrap().expect("first record"));
    assert_eq!(v.cursor(), frame_len(a) as u64);

    // A second record sized so it cannot fit contiguously in what is left of
    // the lap, which is exactly the condition that publishes a gap.
    let b: usize = kani::any();
    kani::assume(b <= 17);
    let used = frame_len(a);
    let rem = CAP - used;
    kani::assume(frame_len(b) > rem);

    let payload = [2u8; 17];
    if w.try_write(&payload[..b]) == Err(WriteError::WouldBlock) {
        assert!(v.try_read().unwrap().is_none());
        assert_eq!(v.cursor(), CAP as u64);
        w.try_write(&payload[..b]).expect("reader skipped padding");
    }

    // The reader is parked on the gap. It must skip it and serve the record
    // from the next lap, not misread the gap bytes as a header.
    let grant = v.try_read().unwrap().expect("wrapped record");
    assert_eq!(grant.len(), b);
    let index: usize = kani::any();
    kani::assume(index < b);
    assert_eq!(grant[index], payload[index]);
    drop(grant);
    assert_eq!(v.cursor(), (CAP + frame_len(b)) as u64);
    assert!(v.try_read().unwrap().is_none());
}

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
    let pinned = g.release_at;
    drop(g);
    assert_eq!(v.cursor(), pinned);

    let again = v.try_latest().unwrap().expect("still the newest");
    assert_eq!(&again[..], &[y]);
}

// ---------------------------------------------------------------------------
// Tier D: symbolic corruption
// ---------------------------------------------------------------------------

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
        // Symbolic words cover all data bytes with fewer loop iterations.
        for i in 0..CAP / 8 {
            base.add(REGION - CAP)
                .cast::<u64>()
                .add(i)
                .write(kani::any());
        }
        // Claim a committed position somewhere in the region, as a writer that
        // died mid-lap would leave behind.
        let committed: u64 = kani::any();
        kani::assume(committed <= 4 * CAP as u64 && committed.is_multiple_of(16));
        let hwm: u64 = kani::any();
        kani::assume(hwm <= 4 * CAP as u64 || hwm == HWM_NONE);
        base.add(OFF_CONTROL).cast::<u64>().write(committed);
        base.add(OFF_CONTROL + 8).cast::<u64>().write(hwm);
    }

    match v.try_read() {
        Err(ReadError::Corrupt) | Ok(None) => {}
        Ok(Some(g)) => {
            // A served record fits in the region with room for its header.
            assert!(g.len() <= CAP - 16);
        }
    }
}

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
    kani::assume(committed <= 2 * CAP as u64 && committed.is_multiple_of(16));
    kani::assume(hwm <= 2 * CAP as u64 || hwm == HWM_NONE);
    kani::assume(cursor <= 2 * CAP as u64 && cursor.is_multiple_of(16));

    // SAFETY: sole handle; offsets from this region's own geometry.
    unsafe {
        base.add(OFF_CONTROL).cast::<u64>().write(committed);
        base.add(OFF_CONTROL + 8).cast::<u64>().write(hwm);
        base.add(HEADER_SIZE).cast::<u64>().write(cursor);
    }

    match v.try_read() {
        Err(ReadError::Corrupt) | Ok(None) => {}
        Ok(Some(g)) => assert!(g.len() <= CAP - 16),
    }
}

#[kani::proof]
#[kani::unwind(12)]
fn corrupt_latest_never_ub() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut v = ring.view(NoWake).unwrap();
    let (base, _) = ring.region();

    // SAFETY: sole handle; offsets from this region's own geometry.
    unsafe {
        // Symbolic words cover all data bytes with fewer loop iterations.
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
        Ok(Some(g)) => assert!(g.len() <= CAP - 16),
    }
}
