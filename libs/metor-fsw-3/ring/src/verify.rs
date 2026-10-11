//! Sequential Kani harnesses; see `VERIFY.md` for assumptions and bounds.
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

#[kani::proof]
fn frame_alignment() {
    let n: usize = kani::any();
    kani::assume(n <= usize::MAX - 15);
    let r = round_up_16(n);
    assert!(r.is_multiple_of(16));
    // Subtraction avoids overflow in the assertion near `usize::MAX`.
    assert!(r >= n);
    assert!(r - n < 16);

    if n <= usize::MAX - 31 {
        let f = frame_len(n);
        assert_eq!(f, 16 + r);
        assert!(f.is_multiple_of(16));
        assert!(f >= 16);
        assert!(f - 16 >= n);
        assert!(f - n < 32);
    }
}

#[kani::proof]
fn payload_in_bounds_holds() {
    let cap = any_capacity();
    let offset: u64 = kani::any();
    kani::assume(offset < cap && offset.is_multiple_of(16));
    // The reader takes the low 32 bits of the length word.
    let len: u64 = kani::any();
    kani::assume(len <= u32::MAX as u64);

    if payload_in_bounds(len, offset, cap) {
        assert!(offset + 16 <= cap);
        assert!(offset + 16 + len <= cap);
        assert!(offset + 16 + round_up16_u64(len) <= cap);
        // The frame length is also representable in 32-bit arithmetic.
        let padded = (len as u32).checked_add(15).expect("round_up16 would wrap") & !15u32;
        let frame = padded.checked_add(16).expect("frame_len would wrap");
        assert_eq!(frame as u64, 16 + round_up16_u64(len));
    }
}

#[kani::proof]
fn frame_start_never_straddles() {
    let cap = any_capacity();
    let rec = any_record(cap);
    let committed: u64 = kani::any();
    kani::assume(committed.is_multiple_of(16) && committed <= u64::MAX - 2 * cap);

    let (start, gap) = frame_start(committed, rec, cap);

    assert_eq!(start - committed, gap);
    assert!(gap < cap);
    assert!(start.is_multiple_of(16));
    assert!((start & (cap - 1)) + rec <= cap);

    // On an empty ring, padding alone always makes room for the record.
    if fits(committed, committed, gap + rec, cap) == Ok(false) {
        assert!(gap > 0);
        assert_eq!(fits(committed, committed, gap, cap), Ok(true));
        assert_eq!(start & (cap - 1), 0);
        assert_eq!(fits(start, start, rec, cap), Ok(true));
    }
}

#[kani::proof]
fn fits_implies_no_lap() {
    let cap = any_capacity();
    let rec = any_record(cap);
    let committed: u64 = kani::any();
    kani::assume(committed.is_multiple_of(16) && committed <= u64::MAX - 4 * cap);
    let slowest_cursor: u64 = kani::any();
    kani::assume(slowest_cursor <= committed);

    let (start, gap) = frame_start(committed, rec, cap);
    let write_len = gap + rec;

    if fits(committed, slowest_cursor, write_len, cap) == Ok(true) {
        assert!(start + rec - slowest_cursor <= cap);
    } else {
        assert!(committed - slowest_cursor + write_len > cap);
    }

    // With no readers registered the writer passes its own position as
    // `slowest_cursor`, leaving capacity as the only bound.
    assert_eq!(
        fits(committed, committed, write_len, cap),
        Ok(write_len <= cap)
    );
}

#[kani::proof]
fn fits_hostile_cursor() {
    let cap: u64 = kani::any();
    let committed: u64 = kani::any();
    let slowest_cursor: u64 = kani::any();
    let write_len: u64 = kani::any();

    match fits(committed, slowest_cursor, write_len, cap) {
        Ok(fit) => {
            assert!(slowest_cursor <= committed);
            assert_eq!(
                fit,
                (committed - slowest_cursor) as u128 + write_len as u128 <= cap as u128
            );
        }
        Err(e) => {
            assert_eq!(e, WriteError::Corrupt);
            assert!(
                slowest_cursor > committed
                    || (committed - slowest_cursor)
                        .checked_add(write_len)
                        .is_none()
            );
        }
    }
}

fn round_up16_u64(n: u64) -> u64 {
    (n + 15) & !15
}

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

    assert!(g.capacity.is_power_of_two());
    assert!(g.capacity >= 16);
    assert!(g.max_readers > 0);
    assert!(region_len >= HEADER_SIZE);

    // The reader table sits behind the header, 8-aligned, before the data region.
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

    let slot: u32 = kani::any();
    kani::assume(slot < g.max_readers);
    let off = g.reader_table_offset + slot as usize * READER_SLOT_SIZE;
    assert!(off.is_multiple_of(8));
    assert!(off + READER_SLOT_SIZE <= g.data_offset);

    // The data region is 16-aligned and ends inside the backing.
    assert!(g.data_offset.is_multiple_of(16));
    let data_end = g.data_offset.checked_add(g.capacity as usize).unwrap();
    assert!(data_end <= region_len);

    let offset: u64 = kani::any();
    kani::assume(offset <= g.capacity);
    assert!(g.data_offset + (offset as usize) <= region_len);
    if offset < g.capacity && offset.is_multiple_of(16) {
        assert!(offset + 16 <= g.capacity);
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
    let geometry = Geometry::from_config(&cfg);

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
        let geometry = Geometry::from_config(&cfg);
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
    let w = ring.writer(NoWake).unwrap();
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
fn backpressure_at_capacity() {
    let mut region = Region::new();
    let ring = region.attach();
    let w = ring.writer(NoWake).unwrap();
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
    let (start, gap) = frame_start(used as u64, frame_len(b) as u64, CAP as u64);
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
fn read_skips_wrap_gap() {
    let mut region = Region::new();
    let ring = region.attach();
    let w = ring.writer(NoWake).unwrap();
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
    let w = ring.writer(NoWake).unwrap();
    let mut v = ring.view(NoWake).unwrap();

    let x: u8 = kani::any();
    let y: u8 = kani::any();
    w.try_write(&[x]).unwrap();
    w.try_write(&[y]).unwrap();

    assert_eq!(v.try_latest(), Ok(Some(&[y][..])));
    assert_eq!(v.cursor(), frame_len(1) as u64);
    assert_eq!(v.try_latest(), Ok(Some(&[y][..])), "still the newest");
}

/// Fills the data region with symbolic bytes.
///
/// It writes whole words to keep the loop short.
///
/// # Safety
/// `base` points to a region of `REGION` bytes that no other handle uses.
unsafe fn fill_data(base: *mut u8) {
    for i in 0..CAP / 8 {
        // SAFETY: word `i` lies within the data region (caller contract).
        unsafe {
            base.add(REGION - CAP)
                .cast::<u64>()
                .add(i)
                .write(kani::any())
        };
    }
}

/// # Safety
/// `base` points to a region of `REGION` bytes that no other handle uses.
unsafe fn write_control(base: *mut u8, committed: u64, hwm: u64) {
    // SAFETY: the control words lie within the region (caller contract).
    unsafe {
        base.add(CONTROL_OFFSET).cast::<u64>().write(committed);
        base.add(CONTROL_OFFSET + 8).cast::<u64>().write(hwm);
    }
}

#[kani::proof]
#[kani::unwind(12)]
fn corrupt_data_never_ub() {
    let mut region = Region::new();
    let ring = region.attach();
    let mut v = ring.view(NoWake).unwrap();
    let (base, region_len) = ring.region();
    assert_eq!(region_len, REGION);

    // A committed position a writer that died mid-lap could leave behind.
    let committed: u64 = kani::any();
    kani::assume(committed <= 4 * CAP as u64 && committed.is_multiple_of(16));
    let hwm: u64 = kani::any();
    kani::assume(hwm <= 4 * CAP as u64 || hwm == HWM_NONE);
    // SAFETY: we hold the only handle to this region.
    unsafe {
        fill_data(base);
        write_control(base, committed, hwm);
    }

    match v.try_read() {
        Err(ReadError::Corrupt) | Ok(None) => {}
        Ok(Some(g)) => assert!(g.len() <= CAP - 16),
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
    // SAFETY: we hold the only handle; the cursor is the first reader slot's.
    unsafe {
        write_control(base, committed, hwm);
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
    // SAFETY: we hold the only handle to this region.
    unsafe {
        fill_data(base);
        write_control(base, CAP as u64, HWM_NONE);
    }

    match v.try_latest() {
        Err(ReadError::Corrupt) | Ok(None) => {}
        Ok(Some(g)) => assert!(g.len() <= CAP - 16),
    }
}

/// # Safety
/// `base` points to a region of `REGION` bytes that no other handle uses.
unsafe fn write_peer_words(base: *mut u8) {
    // SAFETY: the control words and the first reader slot lie within the region.
    unsafe {
        write_control(base, kani::any(), kani::any());
        base.add(HEADER_SIZE).cast::<u64>().write(kani::any());
    }
}

#[kani::proof]
#[kani::unwind(12)]
fn corrupt_peer_writer_never_ub() {
    let mut region = Region::new();
    let ring = region.attach();
    let (base, _) = ring.region();
    // SAFETY: we hold the only handle to this region.
    unsafe { write_peer_words(base) };
    let w = ring.writer(NoWake).unwrap();
    // SAFETY: the peer writes control words the writer only publishes.
    unsafe { write_peer_words(base) };

    for _ in 0..2 {
        let len: usize = kani::any();
        kani::assume(len <= CAP);
        if w.try_write(&[7u8; CAP][..len]).is_ok() {
            assert!(w.committed.get().is_multiple_of(16));
        }
    }
}

#[kani::proof]
#[kani::unwind(12)]
fn corrupt_peer_view_never_ub() {
    let mut region = Region::new();
    let ring = region.attach();
    let (base, _) = ring.region();
    // SAFETY: we hold the only handle to this region.
    unsafe { write_peer_words(base) };
    let mut v = ring.view(NoWake).unwrap();
    let start = v.cursor();
    // SAFETY: the peer writes control words and the view's published cursor.
    unsafe {
        fill_data(base);
        write_peer_words(base);
    }
    assert_eq!(v.cursor(), start);

    match kani::any::<u8>() % 3 {
        0 => match v.try_read() {
            Err(ReadError::Corrupt) | Ok(None) => {}
            Ok(Some(g)) => assert!(g.len() <= CAP - 16),
        },
        1 => match v.try_latest() {
            Err(ReadError::Corrupt) | Ok(None) => {}
            Ok(Some(g)) => assert!(g.len() <= CAP - 16),
        },
        _ => {
            for record in v.drain() {
                if let Ok(g) = record {
                    assert!(g.len() <= CAP - 16);
                }
            }
            v.settle();
        }
    }
}
