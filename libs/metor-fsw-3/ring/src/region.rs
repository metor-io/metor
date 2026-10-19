//! The region's memory layout and header validation.
//!
//! Field order is part of the on-disk and shared-memory format.

use core::ops::Deref;
use core::ptr;

use crate::backing::Backing;
use crate::sync::AtomicU64;
use crate::{AttachError, Config, InvalidConfig, PAYLOAD_ALIGNMENT, WriteError};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub(super) const MAGIC: u32 = u32::from_ne_bytes(*b"MFR1");

/// The region format version. Bump it whenever the layout changes incompatibly.
pub(super) const VERSION: u16 = 6;

pub(super) const RECORD_HEADER_SIZE: usize = PAYLOAD_ALIGNMENT;

/// Region metadata that never changes after initialization.
///
/// Deriving `IntoBytes` checks that the struct has no padding.
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

/// The writer's shared state. Every field is atomic.
#[repr(C)]
pub(super) struct Control {
    pub(super) committed: AtomicU64,
    pub(super) hwm: AtomicU64,
    /// Owner process ID, or zero when unclaimed.
    pub(super) writer: AtomicU64,
}

/// One reader's entry in the reader table.
#[repr(C)]
pub(super) struct ReaderSlot {
    pub(super) cursor: AtomicU64,
    pub(super) owner: AtomicU64,
}

/// A value padded out to whole 64-byte cache lines.
///
/// It pads instead of aligning, so a region base only needs 16-byte alignment.
#[repr(C)]
pub(super) struct CacheLine<T, const PAD: usize> {
    value: T,
    _pad: [u8; PAD],
}

impl<T, const PAD: usize> CacheLine<T, PAD> {
    const fn new(value: T) -> Self {
        Self {
            value,
            _pad: [0; PAD],
        }
    }
}

impl<T, const PAD: usize> Deref for CacheLine<T, PAD> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

/// Returns the padding that rounds `size` up to whole cache lines.
const fn line_pad(size: usize) -> usize {
    size.next_multiple_of(64) - size
}

type HeaderLine = CacheLine<RegionHeader, { line_pad(size_of::<RegionHeader>()) }>;
type ControlLine = CacheLine<Control, { line_pad(size_of::<Control>()) }>;
pub(super) type SlotLine = CacheLine<ReaderSlot, { line_pad(size_of::<ReaderSlot>()) }>;

/// The start of a region: its header, control words, and reader table.
///
/// The data region follows at the header's `data_offset`. Sharing `&Region`
/// between handles and processes is sound because the header and all padding
/// are written only during initialization, before any handle exists. Every
/// field written after that is atomic.
#[repr(C)]
pub(super) struct Region {
    pub(super) header: HeaderLine,
    pub(super) control: ControlLine,
    pub(super) slots: [SlotLine],
}

/// The reader table's offset from the region base.
pub(super) const SLOTS_OFFSET: usize = size_of::<HeaderLine>() + size_of::<ControlLine>();

// PANIC Safety: compile-time assertions pin the shipped layout.
#[cfg(not(ring_loom))]
const _: () = {
    assert!(core::mem::offset_of!(Region, control) == 0x40);
    assert!(SLOTS_OFFSET == 0x80);
    assert!(size_of::<SlotLine>() == 64);
};
// PANIC Safety: a compile-time assertion that the table follows the control
// words without padding.
const _: () = assert!(SLOTS_OFFSET.is_multiple_of(align_of::<SlotLine>()));

impl Region {
    /// Returns a pointer to the region at `base` with `max_readers` reader slots.
    pub(super) fn ptr_at(base: *mut u8, max_readers: u32) -> *mut Self {
        ptr::slice_from_raw_parts_mut(base, max_readers as usize) as *mut Self
    }
}

/// The cursor value of a free slot.
///
/// No aligned record position equals it.
pub(super) const FREE_SLOT: u64 = u64::MAX;
/// The `hwm` value before any wrap gap has been published.
pub(super) const HWM_NONE: u64 = u64::MAX;

/// Returns a tag that identifies native endianness.
///
/// The format uses fixed-width fields, so 32-bit and 64-bit processes can share
/// a region.
pub(super) const fn arch_tag() -> u64 {
    (0x0102_0304u32 as u64) << 32
}

/// Writes a fresh header, control words, and free reader table into `backing`.
///
/// # Safety
/// The backing is aligned, writable only through this call, and large enough for
/// a valid `geometry`. No other handle may observe initialization.
pub(super) unsafe fn init_region(backing: &Backing, geometry: &Geometry) {
    let region = Region::ptr_at(backing.base, geometry.max_readers);
    // SAFETY: checked geometry fits the exclusive, aligned backing.
    unsafe {
        (&raw mut (*region).header).write(CacheLine::new(RegionHeader {
            magic: MAGIC,
            version: VERSION,
            flags: 0,
            capacity: geometry.capacity,
            data_offset: geometry.data_offset as u64,
            max_readers: geometry.max_readers,
            reader_table_offset: SLOTS_OFFSET as u32,
            total_size: geometry.total_size as u64,
            arch_tag: arch_tag(),
        }));
        (&raw mut (*region).control).write(CacheLine::new(Control {
            committed: AtomicU64::new(0),
            hwm: AtomicU64::new(HWM_NONE),
            writer: AtomicU64::new(0),
        }));
        let slots = (&raw mut (*region).slots).cast::<SlotLine>();
        for slot in 0..geometry.max_readers as usize {
            slots.add(slot).write(CacheLine::new(ReaderSlot {
                cursor: AtomicU64::new(FREE_SLOT),
                owner: AtomicU64::new(0),
            }));
        }
    }
}

/// The validated sizes and offsets of a region.
pub(super) struct Geometry {
    /// Bytes in the whole region, including the header, reader table, and data.
    pub(super) total_size: usize,
    /// Bytes in the data region. Kept as `u64` because positions are `u64`.
    pub(super) capacity: u64,
    /// Offset of the data region from the region base.
    pub(super) data_offset: usize,
    pub(super) max_readers: u32,
}

impl TryFrom<Config> for Geometry {
    type Error = InvalidConfig;

    /// Returns the compact layout for `cfg`.
    fn try_from(cfg: Config) -> Result<Self, InvalidConfig> {
        let max_readers = u32::try_from(cfg.max_readers).map_err(|_| InvalidConfig)?;
        if !cfg.capacity.is_power_of_two() || cfg.capacity < RECORD_HEADER_SIZE || max_readers == 0
        {
            return Err(InvalidConfig);
        }
        let data_offset = cfg
            .max_readers
            .checked_mul(size_of::<SlotLine>())
            .and_then(|len| len.checked_add(SLOTS_OFFSET))
            .ok_or(InvalidConfig)?;
        let total_size = data_offset
            .checked_add(cfg.capacity)
            .filter(|&total| total <= isize::MAX as usize)
            .ok_or(InvalidConfig)?;
        Ok(Self {
            total_size,
            capacity: cfg.capacity as u64,
            data_offset,
            max_readers,
        })
    }
}

impl Geometry {
    pub(super) fn mask(&self) -> u64 {
        self.capacity - 1
    }

    /// Returns the bytes a `payload_len`-byte record takes up in this ring.
    ///
    /// Returns [`WriteError::InsufficientCapacity`] if the record can never fit
    /// or its length does not fit the header's 32 bits.
    pub(super) fn frame_len(&self, payload_len: usize) -> Result<u64, WriteError> {
        if payload_len > u32::MAX as usize
            || !crate::payload_in_bounds(payload_len as u64, 0, self.capacity)
        {
            return Err(WriteError::InsufficientCapacity);
        }
        Ok(crate::frame_len(payload_len) as u64)
    }
}

/// Reads and validates the header at `base`, returning the region's geometry.
///
/// # Safety
/// `base` points to one readable allocation of `region_len` bytes. The header
/// bytes are initialized and do not change during the call.
pub(super) unsafe fn read_header(
    base: *mut u8,
    region_len: usize,
) -> Result<Geometry, AttachError> {
    if !(base as usize).is_multiple_of(PAYLOAD_ALIGNMENT) {
        return Err(AttachError::Misaligned);
    }
    if region_len < SLOTS_OFFSET {
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

/// Checks the header and returns the region's geometry.
///
/// Call it before forming any reference into the region.
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
    // The table is the tail of `Region`, so it must sit exactly where the struct puts it.
    let table_end = (max_readers as u64)
        .checked_mul(size_of::<SlotLine>() as u64)
        .and_then(|sz| reader_table_offset.checked_add(sz))
        .ok_or(AttachError::BadGeometry)?;
    if reader_table_offset != SLOTS_OFFSET as u64 || table_end > data_offset {
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
        max_readers,
    })
}
