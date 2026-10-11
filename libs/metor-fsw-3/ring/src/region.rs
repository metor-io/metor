//! Shared layout and validation. Field order is part of the region format.

use crate::backing::Backing;
use crate::sync::AtomicU64;
use crate::{AttachError, Config, PAYLOAD_ALIGNMENT};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub(super) const MAGIC: u32 = u32::from_ne_bytes(*b"MFR1");

/// Shared-memory format version. Change it when the layout becomes incompatible.
pub(super) const VERSION: u16 = 6;

pub(super) const RECORD_HEADER_SIZE: usize = PAYLOAD_ALIGNMENT;

/// Immutable region metadata. `IntoBytes` checks that the layout has no padding.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, align(8))]
pub(super) struct RegionHeader {
    pub(super) magic: u32,
    pub(super) version: u16,
    pub(super) flags: u16,
    pub(super) capacity: u64,
    pub(super) data_offset: u64,
    pub(super) max_readers: u32,
    pub(super) reader_table_offset: u32,
    pub(super) total_size: u64,
    pub(super) arch_tag: u64,
}

/// Shared writer state. All fields remain atomic after initialization.
#[repr(C)]
pub(super) struct Control {
    pub(super) committed: AtomicU64,
    pub(super) hwm: AtomicU64,
    /// Owner process ID, or zero when unclaimed.
    pub(super) writer: AtomicU64,
}

/// One reader cursor and owner, with immutable padding to avoid false sharing.
#[repr(C)]
pub(super) struct ReaderSlot {
    pub(super) cursor: AtomicU64,
    pub(super) owner: AtomicU64,
    pub(super) _pad: [u8; READER_SLOT_PAD],
}

/// Loom atomics have a larger layout and need no cache-line padding.
#[cfg(not(ring_loom))]
pub(super) const READER_SLOT_PAD: usize = 48;
#[cfg(ring_loom)]
pub(super) const READER_SLOT_PAD: usize = 0;

pub(super) const OFF_CONTROL: usize = 0x40;

/// Start of the reader table. Loom requires extra space for its atomics.
#[cfg(not(ring_loom))]
pub(super) const HEADER_SIZE: usize = 0x80;
#[cfg(ring_loom)]
pub(super) const HEADER_SIZE: usize = (OFF_CONTROL + size_of::<Control>()).next_multiple_of(64);

// PANIC Safety: compile-time assertions pin the shipped layout.
#[cfg(not(ring_loom))]
const _: () = {
    assert!(size_of::<RegionHeader>() <= OFF_CONTROL);
    assert!(OFF_CONTROL + size_of::<Control>() <= HEADER_SIZE);
    assert!(size_of::<ReaderSlot>() == 64);
    assert!(align_of::<ReaderSlot>() == 8);
};
pub(super) const READER_SLOT_SIZE: usize = size_of::<ReaderSlot>();

/// A free slot. Aligned record positions never equal this value.
pub(super) const FREE_SLOT: u64 = u64::MAX;
/// No wrap gap has been published.
pub(super) const HWM_NONE: u64 = u64::MAX;

/// Identifies native endianness. The fixed-width format is shared across pointer widths.
pub(super) const fn arch_tag() -> u64 {
    (0x0102_0304u32 as u64) << 32
}

pub(super) fn layout(cfg: &Config) -> Geometry {
    // PANIC Safety: callers of this infallible API must provide a valid config.
    checked_layout(cfg)
        .expect("ring capacity and reader table must form a valid, representable region")
}

pub(super) fn checked_layout(cfg: &Config) -> Option<Geometry> {
    if !cfg.capacity.is_power_of_two()
        || cfg.capacity < RECORD_HEADER_SIZE
        || cfg.max_readers == 0
        || u32::try_from(cfg.max_readers).is_err()
    {
        return None;
    }
    let data_offset = cfg
        .max_readers
        .checked_mul(READER_SLOT_SIZE)?
        .checked_add(HEADER_SIZE)?;
    let total = data_offset.checked_add(cfg.capacity)?;
    if total > isize::MAX as usize {
        return None;
    }
    Some(Geometry {
        capacity: cfg.capacity as u64,
        reader_table_offset: HEADER_SIZE,
        data_offset,
        max_readers: cfg.max_readers as u32,
        total_size: total,
    })
}

/// # Safety
/// The backing is aligned, exclusively writable, and large enough for geometry
/// whose sizes and offsets are valid. No other handle may observe initialization.
pub(super) unsafe fn init_region(backing: &Backing, geometry: &Geometry) {
    let base = backing.base();
    // SAFETY: checked geometry fits the exclusive, aligned backing.
    unsafe {
        base.cast::<RegionHeader>().write(RegionHeader {
            magic: MAGIC,
            version: VERSION,
            flags: 0,
            capacity: geometry.capacity,
            data_offset: geometry.data_offset as u64,
            max_readers: geometry.max_readers,
            reader_table_offset: geometry.reader_table_offset as u32,
            total_size: geometry.total_size as u64,
            arch_tag: arch_tag(),
        });
        base.add(OFF_CONTROL).cast::<Control>().write(Control {
            committed: AtomicU64::new(0),
            hwm: AtomicU64::new(HWM_NONE),
            writer: AtomicU64::new(0),
        });
        for slot in 0..geometry.max_readers as usize {
            base.add(geometry.reader_table_offset + slot * READER_SLOT_SIZE)
                .cast::<ReaderSlot>()
                .write(ReaderSlot {
                    cursor: AtomicU64::new(FREE_SLOT),
                    owner: AtomicU64::new(0),
                    _pad: [0; READER_SLOT_PAD],
                });
        }
    }
}

/// Validated process-local sizes and offsets.
pub(super) struct Geometry {
    pub(super) total_size: usize,
    pub(super) capacity: u64,
    pub(super) data_offset: usize,
    pub(super) reader_table_offset: usize,
    pub(super) max_readers: u32,
}

impl Geometry {
    pub(super) fn mask(&self) -> u64 {
        self.capacity - 1
    }
}

/// # Safety
/// `base` covers one readable allocation of `region_len` bytes. Header bytes
/// are initialized and remain immutable during the call.
pub(super) unsafe fn read_header(
    base: *mut u8,
    region_len: usize,
) -> Result<Geometry, AttachError> {
    if !(base as usize).is_multiple_of(PAYLOAD_ALIGNMENT) {
        return Err(AttachError::Misaligned);
    }
    if region_len < HEADER_SIZE {
        return Err(AttachError::TooSmall);
    }
    // SAFETY: the checked length covers the immutable header. Exclude the
    // following control words, which may be changing concurrently.
    let hdr_bytes =
        unsafe { core::slice::from_raw_parts(base as *const u8, size_of::<RegionHeader>()) };
    // PANIC Safety: the slice has exactly the size of this FromBytes header.
    let hdr = RegionHeader::read_from_bytes(hdr_bytes).expect("exact-size slice");
    validate_header(&hdr, region_len)
}

/// Validate metadata before forming references into the region.
pub(super) fn validate_header(
    hdr: &RegionHeader,
    region_len: usize,
) -> Result<Geometry, AttachError> {
    if hdr.magic != MAGIC {
        return Err(AttachError::BadMagic);
    }
    if hdr.version != VERSION {
        return Err(AttachError::BadVersion);
    }
    if hdr.arch_tag != arch_tag() {
        return Err(AttachError::ArchMismatch);
    }
    let capacity = hdr.capacity;
    let data_offset = hdr.data_offset;
    let max_readers = hdr.max_readers;
    let reader_table_offset = hdr.reader_table_offset as u64;
    let total_size = hdr.total_size;

    if !capacity.is_power_of_two()
        || capacity < RECORD_HEADER_SIZE as u64
        || capacity > usize::MAX as u64
    {
        return Err(AttachError::BadGeometry);
    }
    if max_readers == 0 {
        return Err(AttachError::BadGeometry);
    }
    let table_end = (max_readers as u64)
        .checked_mul(READER_SLOT_SIZE as u64)
        .and_then(|sz| reader_table_offset.checked_add(sz))
        .ok_or(AttachError::BadGeometry)?;
    if reader_table_offset < HEADER_SIZE as u64
        || !reader_table_offset.is_multiple_of(8)
        || table_end > data_offset
    {
        return Err(AttachError::BadGeometry);
    }
    let data_end = data_offset
        .checked_add(capacity)
        .ok_or(AttachError::BadGeometry)?;
    if !data_offset.is_multiple_of(PAYLOAD_ALIGNMENT as u64) || data_end > total_size {
        return Err(AttachError::BadGeometry);
    }
    if total_size > region_len as u64 {
        return Err(AttachError::RegionTruncated);
    }

    Ok(Geometry {
        total_size: total_size as usize,
        capacity,
        // Both fit usize: they are <= total_size <= region_len: usize.
        data_offset: data_offset as usize,
        reader_table_offset: reader_table_offset as usize,
        max_readers,
    })
}
