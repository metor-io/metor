//! A single-writer, multi-reader ring buffer over heap or shared memory.
//!
//! Each [`View`] reads independently. Writes return [`WriteError::WouldBlock`]
//! before overwriting unread data. Borrowed records stay valid until released.
//! New views start at the current write position; they do not read earlier data.
//!
//! ```
//! use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
//!
//! let ring = RingBuffer::create_in_memory(Config { capacity: 1024, max_readers: 4 });
//! let mut writer = ring.writer(NoWake)?;
//! let mut view = ring.view(NoWake)?;
//! writer.try_write(b"hello")?;
//! if let Some(record) = view.try_read()? {
//!     assert_eq!(&*record, b"hello");
//! } // Dropping the grant consumes the record.
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! The `mmap` feature adds file-backed regions. The `notify` feature adds
//! in-process async notifications; [`NoWake`] supports synchronous use.
//! See `DESIGN.md` in the crate source for layout and synchronization rules.

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod wake;

mod backing;
mod region;
mod sync;

use backing::Backing;
#[cfg(all(test, not(ring_loom)))]
use backing::Word;
use region::*;

use crate::sync::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
use crate::sync::{Arc, AtomicU64, fence};
#[cfg(all(test, not(ring_loom)))]
use std::cell::UnsafeCell;

/// Alignment of every record payload, in bytes.
pub const PAYLOAD_ALIGNMENT: usize = 16;

#[inline]
#[cfg(all(not(kani), not(target_arch = "wasm32")))]
fn owner_tag() -> u64 {
    std::process::id() as u64
}

#[inline]
#[cfg(all(not(kani), target_arch = "wasm32"))]
fn owner_tag() -> u64 {
    1
}

#[inline]
#[cfg(kani)]
fn owner_tag() -> u64 {
    1
}

/// Round up to a multiple of 16. Requires `n <= usize::MAX - 15`.
#[inline]
const fn round_up_16(n: usize) -> usize {
    (n + (PAYLOAD_ALIGNMENT - 1)) & !(PAYLOAD_ALIGNMENT - 1)
}

/// Bytes occupied by a record: a 16-byte header and padded payload.
/// Requires `payload_len <= usize::MAX - 31`.
#[inline]
pub const fn frame_len(payload_len: usize) -> usize {
    RECORD_HEADER_SIZE + round_up_16(payload_len)
}

/// Return the record start and preceding wrap gap.
/// Requires a power-of-two capacity and `rec <= capacity`; positions must not overflow.
#[inline]
const fn reserve(committed: u64, rec: u64, capacity: u64) -> (u64, u64) {
    let phys = committed & (capacity - 1);
    if phys + rec > capacity {
        let gap = capacity - phys;
        (committed + gap, gap)
    } else {
        (committed, 0)
    }
}

/// Test whether a write preserves unread bytes.
/// Requires `slowest <= committed` and a representable used-byte count plus `need`.
#[inline]
const fn fits(committed: u64, slowest: u64, need: u64, capacity: u64) -> bool {
    committed.wrapping_sub(slowest) + need <= capacity
}

/// Check a raw payload length before converting it to `usize`.
/// `phys` must be a 16-aligned header position within the data region.
#[inline]
const fn record_fits(len: u64, phys: u64, capacity: u64) -> bool {
    len <= capacity - RECORD_HEADER_SIZE as u64 - phys
}

fn checked_frame_len(payload_len: usize, capacity: u64) -> Result<u64, WriteError> {
    if payload_len > u32::MAX as usize || !record_fits(payload_len as u64, 0, capacity) {
        return Err(WriteError::InsufficientCapacity);
    }
    Ok(frame_len(payload_len) as u64)
}

/// Fixed sizes of a ring region.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Data capacity in bytes: a power of two, at least 16.
    pub capacity: usize,
    /// Reader slots: nonzero and representable as `u32`.
    pub max_readers: usize,
}

/// Why a write was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The record exceeds the ring capacity or the format's payload limit.
    InsufficientCapacity,
    /// Writing would overwrite unread or borrowed data.
    WouldBlock,
}

impl core::fmt::Display for WriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WriteError::InsufficientCapacity => {
                write!(
                    f,
                    "record exceeds ring capacity or the payload length limit"
                )
            }
            WriteError::WouldBlock => write!(
                f,
                "ring is full: writing now would overwrite the slowest active reader"
            ),
        }
    }
}

impl std::error::Error for WriteError {}

/// Why a read failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// A record length extends beyond the data region.
    Corrupt,
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ring region violates a structural invariant (possible external corruption)")
    }
}

impl std::error::Error for ReadError {}

/// All reader slots are claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullReaderTable;

impl core::fmt::Display for FullReaderTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the ring's reader table is full; no free slot for another view")
    }
}

impl std::error::Error for FullReaderTable {}

/// The writer role is already claimed. Dead owners require explicit reclamation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterClaimed;

impl core::fmt::Display for WriterClaimed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("a writer already exists for this ring (or a crashed process leaked its claim)")
    }
}

impl std::error::Error for WriterClaimed {}

/// Why a region could not be attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    BadMagic,
    BadVersion,
    /// The region has an incompatible endianness tag.
    ArchMismatch,
    /// The region is too short for its header or requested configuration.
    TooSmall,
    /// The base address is not 16-byte aligned.
    Misaligned,
    /// Sizes or offsets are invalid, overlapping, misaligned, or unrepresentable.
    BadGeometry,
    /// The declared region size exceeds the backing length.
    RegionTruncated,
}

impl core::fmt::Display for AttachError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AttachError::BadMagic => f.write_str("region header has the wrong magic"),
            AttachError::BadVersion => {
                f.write_str("region was written by an incompatible ring version")
            }
            AttachError::ArchMismatch => {
                f.write_str("region was written with an incompatible endianness tag")
            }
            AttachError::TooSmall => f.write_str("region is shorter than the fixed header"),
            AttachError::Misaligned => f.write_str("region base pointer is not 16-byte aligned"),
            AttachError::BadGeometry => {
                f.write_str("region header fields are internally inconsistent")
            }
            AttachError::RegionTruncated => f.write_str(
                "region header's total_size exceeds the backing region (truncated file?)",
            ),
        }
    }
}

impl std::error::Error for AttachError {}

/// Notifies readers after data or wrap padding is published.
pub trait WakeSource {
    fn notify(&self);
}

/// Waits for a readiness change.
///
/// Implementations must check `ready` after arming to avoid missed wakeups.
/// They may return before readiness; the caller checks again after each return.
#[allow(async_fn_in_trait)]
pub trait WakeSink {
    async fn wait_until<F: FnMut() -> bool>(&self, ready: F);
}

/// Disables notifications. Async reads on an empty ring spin without yielding.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoWake;

impl WakeSource for NoWake {
    fn notify(&self) {}
}

impl WakeSink for NoWake {
    async fn wait_until<F: FnMut() -> bool>(&self, mut ready: F) {
        // Resolve immediately; the caller's loop re-polls.
        let _ = ready();
    }
}

/// Wakes all tasks waiting on clones of this notifier within one process.
#[cfg(feature = "notify")]
#[derive(Clone)]
pub struct Notifier(Arc<stellarator::sync::WaitQueue>);

#[cfg(feature = "notify")]
impl Default for Notifier {
    fn default() -> Self {
        Self(Arc::new(stellarator::sync::WaitQueue::new()))
    }
}

#[cfg(feature = "notify")]
impl WakeSource for Notifier {
    fn notify(&self) {
        self.0.wake_all();
    }
}

#[cfg(feature = "notify")]
impl WakeSink for Notifier {
    async fn wait_until<F: FnMut() -> bool>(&self, ready: F) {
        let _ = self.0.wait_for(ready).await;
    }
}

struct Inner {
    backing: Backing,
    geometry: Geometry,
}

impl Inner {
    #[inline]
    fn base(&self) -> *mut u8 {
        self.backing.base()
    }

    #[inline]
    fn control(&self) -> &Control {
        // SAFETY: validated geometry places this aligned, all-atomic block within the backing.
        unsafe { &*(self.base().add(OFF_CONTROL) as *const Control) }
    }

    #[inline]
    fn slot(&self, slot: u32) -> &ReaderSlot {
        let off = self.geometry.reader_table_offset + slot as usize * READER_SLOT_SIZE;
        // SAFETY: callers use slot < max_readers. Geometry bounds and aligns the slot;
        // its fields are atomic and its padding is immutable after initialization.
        unsafe { &*(self.base().add(off) as *const ReaderSlot) }
    }

    /// # Safety
    /// `phys <= capacity`. The one-past pointer may only form an empty slice.
    #[inline]
    unsafe fn data_ptr(&self, phys: usize) -> *mut u8 {
        // SAFETY: validated geometry and phys <= capacity keep this within the allocation
        // or one byte position past its end.
        unsafe { self.base().add(self.geometry.data_offset + phys) }
    }

    fn slowest_active_cursor(&self) -> Option<u64> {
        // Pair with the registration fence: a scan sees the claim or the reader sees publication.
        fence(SeqCst);
        (0..self.geometry.max_readers)
            .map(|slot| self.slot(slot).cursor.load(Acquire))
            .filter(|&cursor| cursor != FREE_SLOT)
            .min()
    }

    /// # Safety
    /// The caller holds the writer claim. The aligned, contiguous record fits in
    /// the data region, its length fits `u32`, and no reader pins the destination.
    unsafe fn write_record(&self, phys: usize, payload: &[u8]) {
        // SAFETY: phys is 8-aligned and in-bounds (caller contract).
        let p = unsafe { self.data_ptr(phys) };
        let len = payload.len() as u64; // low 32 bits = length, high = pad.
        // SAFETY: p is 8-aligned; [phys, phys+frame_len) is in-bounds.
        unsafe {
            (p as *mut u64).write(len);
            std::ptr::copy_nonoverlapping(
                payload.as_ptr(),
                p.add(RECORD_HEADER_SIZE),
                payload.len(),
            );
        }
    }

    /// # Safety
    /// `phys` is an aligned record header with eight readable bytes. Publication
    /// happens before this read, and a reader cursor keeps the record pinned.
    unsafe fn read_len(&self, phys: usize) -> u64 {
        // SAFETY: phys is 8-aligned and in-bounds (caller contract); the read
        // is ordered after the write via `committed`.
        let hdr = unsafe { (self.data_ptr(phys) as *const u64).read() };
        hdr & 0xFFFF_FFFF
    }

    /// Locate a record, skipping any wrap gap. Also return the position after the skip.
    fn locate_from(&self, mut r: u64) -> Result<(u64, Option<Located>), ReadError> {
        let cap = self.geometry.capacity;
        loop {
            // Acquire publication before the wrap marker, matching the writer's store order.
            let c = self.control().committed.load(Acquire);
            let hwm = self.control().hwm.load(Acquire);
            if r == hwm {
                // Skip this gap once; absolute gap positions increase.
                r = (r & !self.geometry.mask()) + cap;
                continue;
            }
            // The marker can become visible before the new committed position.
            if r >= c {
                return Ok((r, None));
            }
            let phys = (r & self.geometry.mask()) as usize;
            // SAFETY: r is a pinned record start and r < c observes publication.
            // Alignment and capacity >= 16 leave room for the header.
            let len = unsafe { self.read_len(phys) };
            if !record_fits(len, phys as u64, cap) {
                return Err(ReadError::Corrupt);
            }
            let len = len as usize;
            let rec = frame_len(len);
            return Ok((
                r,
                Some(Located {
                    start: r,
                    phys,
                    payload_len: len,
                    frame_len: rec,
                }),
            ));
        }
    }

    /// # Safety
    /// `loc` came from lookup. A reader cursor at or before its start must keep
    /// the record pinned for the returned slice's lifetime.
    unsafe fn payload(&self, loc: &Located) -> &[u8] {
        // SAFETY: the record is contiguous and in-bounds (caller contract).
        unsafe {
            let p = self.data_ptr(loc.phys + RECORD_HEADER_SIZE);
            std::slice::from_raw_parts(p as *const u8, loc.payload_len)
        }
    }
}

/// A cloneable region handle. Its writers and views keep the backing alive.
#[derive(Clone)]
pub struct RingBuffer {
    inner: Arc<Inner>,
}

impl RingBuffer {
    /// Create a heap-backed ring.
    ///
    /// # Panics
    /// Panics if capacity is not a power of two of at least 16, the reader count
    /// is zero or exceeds `u32::MAX`, or the total size exceeds `isize::MAX`.
    pub fn create_in_memory(cfg: Config) -> Self {
        let geometry = layout(&cfg);
        let backing = Backing::heap(geometry.total_size);
        // SAFETY: fresh, aligned storage large enough for the checked geometry.
        unsafe { Self::initialize(backing, geometry) }
    }

    /// Initialize caller-owned storage. Checks base alignment and available size.
    ///
    /// # Safety
    /// The region is one writable, interior-mutable allocation, exclusively
    /// accessible during initialization. It must outlive all resulting handles,
    /// clones, and borrows. Afterwards, access must follow the ring protocol.
    ///
    /// # Panics
    /// Panics on invalid configurations, as [`Self::create_in_memory`] does.
    pub unsafe fn create_raw(base: *mut u8, len: usize, cfg: Config) -> Result<Self, AttachError> {
        if !(base as usize).is_multiple_of(PAYLOAD_ALIGNMENT) {
            return Err(AttachError::Misaligned);
        }
        let geometry = layout(&cfg);
        if len < geometry.total_size {
            return Err(AttachError::TooSmall);
        }
        // SAFETY: the caller supplies exclusive storage; size and alignment were checked.
        unsafe {
            let backing = Backing::raw(base, geometry.total_size);
            Ok(Self::initialize(backing, geometry))
        }
    }

    /// Create a file-backed ring, truncating any existing file.
    ///
    /// # Safety
    /// The caller has exclusive access during creation, with no previous mappings
    /// or handles in use. Until all resulting handles and borrows are dropped,
    /// the file must not be truncated, reformatted, or modified outside the protocol.
    ///
    /// # Panics
    /// Panics on invalid configurations, as [`Self::create_in_memory`] does.
    #[cfg(feature = "mmap")]
    pub unsafe fn create_mmap(path: &std::path::Path, cfg: Config) -> std::io::Result<Self> {
        let geometry = layout(&cfg);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(geometry.total_size as u64)?;
        // SAFETY: the file has the required size and the caller guarantees exclusive access.
        let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        if !(map.as_ptr() as usize).is_multiple_of(PAYLOAD_ALIGNMENT) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                AttachError::Misaligned,
            ));
        }
        // SAFETY: the caller guarantees exclusive access; size and alignment were checked.
        Ok(unsafe { Self::initialize(Backing::mmap(map), geometry) })
    }

    /// Map an existing ring and validate its header.
    ///
    /// # Safety
    /// The file contains an initialized ring. Until all resulting handles and
    /// borrows are dropped, it must not be truncated, reformatted, or modified
    /// outside the ring protocol. The header must remain immutable.
    #[cfg(feature = "mmap")]
    pub unsafe fn attach_mmap(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        // SAFETY: caller asserts a valid region; mapping read+write shared.
        let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        // SAFETY: the caller guarantees a live ring with an immutable header.
        unsafe { Self::attach(Backing::mmap(map)) }
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Attach to caller-owned storage after validating its header.
    ///
    /// # Safety
    /// The region is one live, interior-mutable allocation containing an initialized
    /// ring. It must outlive all resulting handles, clones, and borrows. The header
    /// remains immutable; all other access follows the ring protocol.
    pub unsafe fn attach_raw(base: *mut u8, len: usize) -> Result<Self, AttachError> {
        // SAFETY: the caller guarantees the region's lifetime and shared-memory access.
        unsafe { Self::attach(Backing::raw(base, len)) }
    }

    /// # Safety
    /// The backing contains an initialized ring with an immutable header. Its
    /// lifetime and all concurrent access must satisfy the ring protocol.
    unsafe fn attach(backing: Backing) -> Result<Self, AttachError> {
        // SAFETY: the caller guarantees readable, immutable header bytes.
        let geometry = unsafe { read_header(backing.base(), backing.len()) }?;
        Ok(Self::from_parts(backing, geometry))
    }

    fn from_parts(backing: Backing, geometry: Geometry) -> Self {
        Self {
            inner: Arc::new(Inner { backing, geometry }),
        }
    }

    /// # Safety
    /// The backing is aligned, exclusively writable, and holds the checked geometry.
    unsafe fn initialize(backing: Backing, geometry: Geometry) -> Self {
        // SAFETY: the caller provides exclusive storage for the checked geometry.
        unsafe { init_region(&backing, &geometry) };
        Self::from_parts(backing, geometry)
    }

    /// Base address and backing length. Raw attachments do not keep the owner alive.
    pub fn region(&self) -> (*mut u8, usize) {
        (self.inner.backing.base(), self.inner.backing.len())
    }

    /// Absolute published position in bytes, including wrap padding.
    pub fn committed(&self) -> u64 {
        self.inner.control().committed.load(Acquire)
    }

    /// Claim the region's only writer. Dropping it releases the claim.
    /// `data` is notified after publication; use [`NoWake`] for synchronous readers.
    pub fn writer<WD: WakeSource>(&self, data: WD) -> Result<Writer<WD>, WriterClaimed> {
        // Acquire the state released by the previous writer or reclaimer.
        self.inner
            .control()
            .writer
            .compare_exchange(0, owner_tag(), Acquire, Relaxed)
            .map_err(|_| WriterClaimed)?;
        Ok(Writer {
            inner: self.inner.clone(),
            data,
        })
    }

    /// Release writer and reader claims held by a dead process.
    ///
    /// # Safety
    /// `pid` is a nonzero ID of an exited, reaped process whose stores have finished
    /// and whose handles will never be used again. It must not identify a live
    /// claimant through PID reuse.
    pub unsafe fn reclaim_owner(&self, pid: u64) {
        for index in 0..self.inner.geometry.max_readers {
            let slot = self.inner.slot(index);
            if slot.cursor.load(Acquire) != FREE_SLOT
                && slot.owner.compare_exchange(pid, 0, AcqRel, Acquire).is_ok()
            {
                // Free the cursor. Release pairs with the claim CAS in `view`.
                slot.cursor.store(FREE_SLOT, Release);
            }
        }
        // Release only this owner's claim; the next writer acquires its state.
        let _ = self
            .inner
            .control()
            .writer
            .compare_exchange(pid, 0, Release, Relaxed);
    }

    /// Register a reader at the current published position.
    /// Earlier records are skipped. Async reads wait on `data`.
    pub fn view<RD: WakeSink>(&self, data: RD) -> Result<View<RD>, FullReaderTable> {
        let mut start = self.inner.control().committed.load(Acquire);
        for slot in 0..self.inner.geometry.max_readers {
            // Acquire the slot released by its previous owner.
            if self
                .inner
                .slot(slot)
                .cursor
                .compare_exchange(FREE_SLOT, start, AcqRel, Relaxed)
                .is_ok()
            {
                // Record ownership promptly; a crash before this store leaves an unattributed slot.
                self.inner.slot(slot).owner.store(owner_tag(), Release);
                // Retry until publication stays stable across the fence paired with the writer scan.
                loop {
                    fence(SeqCst);
                    let c2 = self.inner.control().committed.load(Acquire);
                    if c2 == start {
                        break;
                    }
                    // Publication raced registration; start at the new edge and recheck.
                    start = c2;
                    self.inner.slot(slot).cursor.store(start, Release);
                }
                return Ok(View {
                    inner: self.inner.clone(),
                    slot,
                    data,
                    pending: None,
                });
            }
        }
        Err(FullReaderTable)
    }

    #[cfg(all(test, not(ring_loom)))]
    pub(crate) fn reader_count(&self) -> usize {
        (0..self.inner.geometry.max_readers)
            .filter(|&s| self.inner.slot(s).cursor.load(Relaxed) != FREE_SLOT)
            .count()
    }
}

/// Read the configuration from a region without allocating a handle.
///
/// # Safety
/// `base..base + len` is one readable allocation. The header bytes are initialized
/// and must not change during this call.
pub unsafe fn config_of(base: *mut u8, len: usize) -> Result<Config, AttachError> {
    // SAFETY: the caller guarantees readable, immutable header bytes.
    let geometry = unsafe { read_header(base, len) }?;
    Ok(Config {
        capacity: geometry.capacity as usize,
        max_readers: geometry.max_readers as usize,
    })
}

/// Required backing size for this configuration.
///
/// # Panics
/// Panics on invalid configurations, as [`RingBuffer::create_in_memory`] does.
pub fn region_len(cfg: &Config) -> usize {
    layout(cfg).total_size
}

/// Required backing size, or `None` if the configuration cannot form a valid region.
pub fn checked_region_len(cfg: &Config) -> Option<usize> {
    checked_layout(cfg).map(|geometry| geometry.total_size)
}

/// The region's sole writer. Dropping it releases the shared writer claim.
pub struct Writer<WD: WakeSource> {
    inner: Arc<Inner>,
    data: WD,
}

impl<WD: WakeSource> Writer<WD> {
    /// Publish one record without blocking.
    /// Returns [`WriteError::WouldBlock`] if readers pin the required space.
    /// A blocked write may publish wrap padding, but never a payload.
    pub fn try_write(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        let rec = checked_frame_len(bytes.len(), self.inner.geometry.capacity)?;
        let mut c = self.inner.control().committed.load(Relaxed); // sole writer
        let (start_abs, mut gap) = reserve(c, rec, self.inner.geometry.capacity);
        if !self.fits(c, gap + rec) {
            if gap == 0 || !self.fits(c, gap) {
                return Err(WriteError::WouldBlock);
            }
            self.inner.control().hwm.store(c, Release);
            self.inner.control().committed.store(start_abs, Release);
            self.data.notify();
            c = start_abs;
            gap = 0;
            if !self.fits(c, rec) {
                return Err(WriteError::WouldBlock);
            }
        }
        // SAFETY: reservation is contiguous, the length is valid, and the reader scan
        // allows this writer to reuse the destination.
        unsafe { self.commit(c, start_abs, start_abs + rec, gap, bytes) };
        Ok(())
    }

    #[inline]
    fn fits(&self, committed: u64, need: u64) -> bool {
        let slowest = self.inner.slowest_active_cursor().unwrap_or(committed);
        // PANIC Safety: the protocol must keep the scanned cursor <= committed.
        // Checked by tests and Loom; unchecked overflow is not a safe fallback.
        #[cfg(any(test, ring_loom))]
        debug_assert!(
            slowest <= committed,
            "cursor {slowest} is ahead of committed {committed}"
        );
        fits(committed, slowest, need, self.inner.geometry.capacity)
    }

    /// # Safety
    /// The writer claim is held. The record fits contiguously in unpinned space,
    /// and `end_abs` equals its start plus its frame length.
    unsafe fn commit(&self, committed: u64, start_abs: u64, end_abs: u64, gap: u64, bytes: &[u8]) {
        let phys = (start_abs & self.inner.geometry.mask()) as usize;
        // SAFETY: record is contiguous and in-bounds (caller contract).
        unsafe { self.inner.write_record(phys, bytes) };
        if gap > 0 {
            // Publish the wrap marker before the position that crosses it.
            self.inner.control().hwm.store(committed, Release);
        }
        // Release publishes record bytes to acquiring readers.
        self.inner.control().committed.store(end_abs, Release);
        self.data.notify();
    }
}

impl<WD: WakeSource> Drop for Writer<WD> {
    fn drop(&mut self) {
        // Release the region state to the next writer.
        self.inner.control().writer.store(0, Release);
    }
}

/// A published record whose payload bounds have been checked.
struct Located {
    /// Absolute record start.
    start: u64,
    /// Record offset within the data region.
    phys: usize,
    payload_len: usize,
    frame_len: usize,
}

impl Located {
    fn end(&self) -> u64 {
        self.start + self.frame_len as u64
    }
}

/// An independent reader. Dropping it releases its shared reader slot.
pub struct View<RD: WakeSink> {
    inner: Arc<Inner>,
    slot: u32,
    data: RD,
    /// Drain progress applied once its borrowed slices can no longer be used.
    pending: Option<u64>,
}

impl<RD: WakeSink> View<RD> {
    /// Absolute read position. Pending drain consumption is not yet included.
    pub fn cursor(&self) -> u64 {
        self.inner.slot(self.slot).cursor.load(Acquire)
    }

    /// Absolute published position in bytes, including wrap padding.
    pub fn committed(&self) -> u64 {
        self.inner.control().committed.load(Acquire)
    }

    /// Copy and consume the next record; return `false` when caught up.
    /// Leaves `buf` unchanged on an empty read or error. Reserve enough buffer
    /// capacity during initialization to avoid allocation while reading.
    pub fn try_read_into(&mut self, buf: &mut Vec<u8>) -> Result<bool, ReadError> {
        let Some(grant) = self.try_read()? else {
            return Ok(false);
        };
        buf.clear();
        buf.extend_from_slice(&grant);
        Ok(true)
    }

    /// Borrow the next unread record, or return `None` if caught up.
    /// Dropping the grant consumes the record and lets the writer reuse its space.
    pub fn try_read(&mut self) -> Result<Option<ReadGrant<'_>>, ReadError> {
        self.settle();
        let Some(loc) = self.locate()? else {
            return Ok(None);
        };
        let end = loc.end();
        Ok(Some(self.grant(loc, end)))
    }

    /// Wait for the next record and borrow it. Dropping the grant consumes it.
    pub async fn read(&mut self) -> Result<ReadGrant<'_>, ReadError> {
        self.settle();
        let loc = loop {
            if let Some(loc) = self.locate()? {
                break loc;
            }
            let inner = &self.inner;
            let slot = self.slot;
            self.data
                .wait_until(|| {
                    inner.control().committed.load(Acquire) > inner.slot(slot).cursor.load(Acquire)
                })
                .await;
        };
        let end = loc.end();
        Ok(self.grant(loc, end))
    }

    /// Skip older unread records and borrow the newest.
    /// The newest stays pinned after grant drop, so repeated calls can return it
    /// again. Returns `None` when this view has no unread record.
    pub fn try_latest(&mut self) -> Result<Option<ReadGrant<'_>>, ReadError> {
        self.settle();
        loop {
            let Some(loc) = self.locate()? else {
                return Ok(None);
            };
            let end = loc.end();
            if end >= self.data_end() {
                let start = loc.start;
                return Ok(Some(self.grant(loc, start)));
            }
            self.advance(end);
        }
    }

    /// Borrow the newest record until the next mutable access to this view.
    /// The record stays pinned, as with [`Self::try_latest`].
    pub fn try_latest_bytes(&mut self) -> Result<Option<&[u8]>, ReadError> {
        // Dropping a latest grant leaves its record pinned by the view.
        Ok(self.try_latest()?.map(|grant| grant.slice))
    }

    fn locate(&self) -> Result<Option<Located>, ReadError> {
        let r = self.inner.slot(self.slot).cursor.load(Acquire);
        let (skipped, loc) = self.inner.locate_from(r)?;
        if skipped != r {
            self.advance(skipped);
        }
        Ok(loc)
    }

    fn grant(&mut self, loc: Located, release_at: u64) -> ReadGrant<'_> {
        // SAFETY: lookup validated the record; this exclusive view borrow keeps it pinned.
        let slice = unsafe { self.inner.payload(&loc) };
        ReadGrant {
            cursor: &self.inner.slot(self.slot).cursor,
            release_at,
            slice,
        }
    }

    fn settle(&mut self) {
        if let Some(pos) = self.pending.take() {
            self.advance(pos);
        }
    }

    /// Iterate unread records as borrowed slices, which may coexist.
    /// Consumption is deferred until the next mutable view operation, after all
    /// slices are no longer used. Stopping early consumes only yielded records.
    pub fn drain(&mut self) -> Drain<'_> {
        self.settle();
        let pos = self.cursor();
        let View { inner, pending, .. } = self;
        Drain {
            inner,
            pos,
            pending,
            done: false,
        }
    }

    /// Published position excluding a trailing wrap gap.
    fn data_end(&self) -> u64 {
        let committed = self.inner.control().committed.load(Acquire);
        let hwm = self.inner.control().hwm.load(Acquire);
        if hwm < committed
            && hwm + self.inner.geometry.capacity - (hwm & self.inner.geometry.mask()) == committed
        {
            hwm
        } else {
            committed
        }
    }

    #[inline]
    fn advance(&self, end_abs: u64) {
        self.inner.slot(self.slot).cursor.store(end_abs, Release);
    }
}

impl<RD: WakeSink> Drop for View<RD> {
    fn drop(&mut self) {
        self.inner.slot(self.slot).owner.store(0, Relaxed);
        self.inner.slot(self.slot).cursor.store(FREE_SLOT, Release);
    }
}

/// A borrowed record, dereferencing to its payload.
/// Dropping it consumes an ordinary read or keeps a latest read pinned.
pub struct ReadGrant<'a> {
    cursor: &'a AtomicU64,
    release_at: u64,
    slice: &'a [u8],
}

impl std::ops::Deref for ReadGrant<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.slice
    }
}

impl Drop for ReadGrant<'_> {
    fn drop(&mut self) {
        self.cursor.store(self.release_at, Release);
    }
}

/// Borrows unread records while deferring consumption. See [`View::drain`].
pub struct Drain<'a> {
    inner: &'a Inner,
    pos: u64,
    pending: &'a mut Option<u64>,
    done: bool,
}

impl<'a> Iterator for Drain<'a> {
    type Item = Result<&'a [u8], ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.inner.locate_from(self.pos) {
            Ok((_, Some(loc))) => {
                self.pos = loc.end();
                *self.pending = Some(self.pos);
                // SAFETY: the view's cursor is at or before `loc.start` until the
                // drain's position is applied, which waits for `'a` to end.
                Some(Ok(unsafe { self.inner.payload(&loc) }))
            }
            Ok((pos, None)) => {
                self.pos = pos;
                *self.pending = Some(pos);
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

#[cfg(all(test, not(ring_loom)))]
mod tests;

#[cfg(all(test, ring_loom))]
mod loom_tests;

#[cfg(kani)]
mod verify;
