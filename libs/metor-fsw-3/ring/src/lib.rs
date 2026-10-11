//! A single-writer, multi-reader ring buffer over heap or shared memory.
//!
//! One [`Writer`] appends variable-length records. Up to
//! [`Config::max_readers`] [`View`]s read them, each with its own cursor, so one
//! reader never consumes records for another. The writer never overwrites a
//! record that some view has not read yet. Instead, the write fails with
//! [`WriteError::WouldBlock`]. A record borrowed from a view stays valid until
//! the borrow is released. A new view starts at the current write position and
//! does not see records written before it was created.
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
//! in-process async notifications. Pass [`NoWake`] when readers poll instead.
//! See `DESIGN.md` in the crate source for layout and synchronization rules.

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod wake;

mod backing;
mod owner;
mod region;
mod sync;

use backing::Backing;
pub use owner::{RawOwner, RingExport};
use region::*;

use crate::sync::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
use crate::sync::{Arc, AtomicU64, fence};
use core::cell::Cell;

/// Alignment of every record payload, in bytes.
pub const PAYLOAD_ALIGNMENT: usize = 16;

#[inline]
#[cfg(not(any(kani, target_arch = "wasm32")))]
fn owner_tag() -> u64 {
    std::process::id() as u64
}

#[inline]
#[cfg(any(kani, target_arch = "wasm32"))]
fn owner_tag() -> u64 {
    1
}

/// Rounds `n` up to a multiple of 16.
///
/// `n` must be at most `usize::MAX - 15`.
#[inline]
const fn round_up_16(n: usize) -> usize {
    (n + (PAYLOAD_ALIGNMENT - 1)) & !(PAYLOAD_ALIGNMENT - 1)
}

/// Returns the bytes a record takes up in the ring.
///
/// That is a 16-byte header plus the payload padded to 16 bytes. `payload_len`
/// must be at most `usize::MAX - 31`.
#[inline]
pub const fn frame_len(payload_len: usize) -> usize {
    RECORD_HEADER_SIZE + round_up_16(payload_len)
}

/// Returns where a record starts and the wrap gap before it.
///
/// A record that would run past the end of the data region starts at the next
/// lap instead. The skipped bytes form the gap. `capacity` must be a power of
/// two, `rec` must be at most `capacity`, and positions must not overflow.
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

/// Returns whether `need` more bytes fit without overwriting unread data.
///
/// Returns `Corrupt` if `slowest` is ahead of `committed` or the sum overflows.
#[inline]
const fn fits(committed: u64, slowest: u64, need: u64, capacity: u64) -> Result<bool, WriteError> {
    if slowest > committed {
        return Err(WriteError::Corrupt);
    }
    match (committed - slowest).checked_add(need) {
        Some(used) => Ok(used <= capacity),
        None => Err(WriteError::Corrupt),
    }
}

/// Returns whether the writer can safely reserve space at `committed`.
///
/// Reserving must neither overflow nor break alignment.
#[inline]
const fn writable_from(committed: u64, capacity: u64) -> bool {
    committed.is_multiple_of(PAYLOAD_ALIGNMENT as u64) && committed <= u64::MAX - 2 * capacity
}

/// Returns whether a payload of `len` bytes with its header at `phys` fits.
///
/// Use it on a raw length read from the region before converting it to
/// `usize`. `phys` must be a 16-aligned position in the data region.
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

/// The sizes a ring region is created with.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Data capacity in bytes: a power of two, at least 16.
    pub capacity: usize,
    /// Reader slots: nonzero and representable as `u32`.
    pub max_readers: usize,
}

/// Why a write was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WriteError {
    #[error("record exceeds the ring capacity or the payload length limit")]
    InsufficientCapacity,
    #[error("ring is full: writing would overwrite unread or borrowed data")]
    WouldBlock,
    #[error("ring region violates a structural invariant")]
    Corrupt,
}

/// Why a read was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    #[error("ring region violates a structural invariant")]
    Corrupt,
}

/// Returned when every reader slot is already claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the ring's reader table is full")]
pub struct FullReaderTable;

/// Returned when the ring already has a writer.
///
/// A claim held by a process that died stays held until
/// [`RingBuffer::reclaim_owner`] releases it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a writer already holds this ring's claim")]
pub struct WriterClaimed;

/// Why a region could not be attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AttachError {
    #[error("region header has the wrong magic")]
    BadMagic,
    #[error("region was written by an incompatible ring version")]
    BadVersion,
    #[error("region has an incompatible endianness tag")]
    ArchMismatch,
    #[error("region is shorter than its header or configuration")]
    TooSmall,
    #[error("region base is not 16-byte aligned")]
    Misaligned,
    #[error("region sizes or offsets are invalid, overlapping, or unrepresentable")]
    BadGeometry,
    #[error("region header's total size exceeds the backing length")]
    RegionTruncated,
}

/// Tells readers that the writer published new data.
///
/// New data is a record or wrap padding. The writer calls `notify` last, once the write is fully published, so
/// `notify` may itself write to the ring again.
pub trait WakeSource {
    fn notify(&self);
}

/// Lets a reader wait until new data may be ready.
///
/// `wait_until` must check `ready` after it registers for wakeups. Otherwise a
/// wakeup that arrives in between is lost. It may return while `ready` is still
/// false, so callers check again after every return.
#[allow(async_fn_in_trait)]
pub trait WakeSink {
    async fn wait_until<F: FnMut() -> bool>(&self, ready: F);

    /// Returns a wait future that does not borrow this sink.
    ///
    /// Returns `None` if the sink does not support one.
    #[cfg(feature = "notify")]
    fn wait_owned(&self) -> Option<maitake_sync::wait_queue::WaitOwned> {
        None
    }
}

/// A wake source and sink that does nothing.
///
/// Waiting on it returns immediately, so use it when readers poll.
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

/// Wakes async readers in the same process.
///
/// Notifying it wakes every task waiting on any clone of it.
#[cfg(feature = "notify")]
#[derive(Clone)]
pub struct Notifier(Arc<maitake_sync::WaitQueue>);

#[cfg(feature = "notify")]
impl Default for Notifier {
    fn default() -> Self {
        Self(Arc::new(maitake_sync::WaitQueue::new()))
    }
}

#[cfg(feature = "notify")]
impl Notifier {
    /// Returns a future that completes on the next notification.
    ///
    /// The future does not borrow this handle and does not allocate. It
    /// registers when first polled. Check readiness after that first poll, or a
    /// notification sent before registration is missed.
    pub fn wait_owned(&self) -> maitake_sync::wait_queue::WaitOwned {
        self.0.wait_owned()
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

    fn wait_owned(&self) -> Option<maitake_sync::wait_queue::WaitOwned> {
        Some(Notifier::wait_owned(self))
    }
}

struct Inner {
    backing: Backing,
    geometry: Geometry,
}

impl Inner {
    #[inline]
    fn base(&self) -> *mut u8 {
        self.backing.base
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
    /// The caller holds the writer claim. The record is aligned, contiguous, and
    /// fits in the data region. Its length fits in a `u32`, and no reader cursor
    /// pins the destination.
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
    /// `phys` is the aligned position of a record header with eight readable
    /// bytes. The record was published before this read, and a reader cursor
    /// keeps it pinned.
    unsafe fn read_len(&self, phys: usize) -> u64 {
        // SAFETY: phys is 8-aligned and in-bounds (caller contract); the read
        // is ordered after the write via `committed`.
        let hdr = unsafe { (self.data_ptr(phys) as *const u64).read() };
        hdr & 0xFFFF_FFFF
    }

    /// Finds the record at `r`, skipping a wrap gap if `r` sits on one.
    ///
    /// Returns the position after any skip, and the record if one was published
    /// before `committed`. Returns `Corrupt` if `r` is more than a lap behind
    /// `committed` or is not on a 16-byte boundary.
    fn locate_before(
        &self,
        mut r: u64,
        committed: u64,
    ) -> Result<(u64, Option<Located>), ReadError> {
        let cap = self.geometry.capacity;
        loop {
            if r >= committed {
                return Ok((r, None));
            }
            if committed - r > cap || !r.is_multiple_of(PAYLOAD_ALIGNMENT as u64) {
                return Err(ReadError::Corrupt);
            }
            let hwm = self.control().hwm.load(Acquire);
            if r == hwm {
                // Skip this gap once; absolute gap positions increase.
                r = self.next_lap(r).ok_or(ReadError::Corrupt)?;
                continue;
            }
            let phys = (r & self.geometry.mask()) as usize;
            // SAFETY: r is pinned and below an acquired committed position.
            // Alignment and capacity >= 16 leave room for the header.
            let len = unsafe { self.read_len(phys) };
            if !record_fits(len, phys as u64, cap) {
                return Err(ReadError::Corrupt);
            }
            let len = len as usize;
            let rec = frame_len(len);
            if rec as u64 > committed - r {
                return Err(ReadError::Corrupt);
            }
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

    /// Returns the start of the lap after `pos`, or `None` on overflow.
    #[inline]
    fn next_lap(&self, pos: u64) -> Option<u64> {
        (pos & !self.geometry.mask()).checked_add(self.geometry.capacity)
    }

    /// # Safety
    /// `loc` came from `locate_before`. A reader cursor at or before the
    /// record's start must pin the record for as long as the returned slice lives.
    unsafe fn payload(&self, loc: &Located) -> &[u8] {
        // SAFETY: the record is contiguous and in-bounds (caller contract).
        unsafe {
            let p = self.data_ptr(loc.phys + RECORD_HEADER_SIZE);
            std::slice::from_raw_parts(p as *const u8, loc.payload_len)
        }
    }
}

/// A cloneable handle to a ring.
///
/// Use it to create the ring's [`Writer`] and its [`View`]s. The ring's storage
/// stays alive while any handle, writer, or view exists.
#[derive(Clone)]
pub struct RingBuffer {
    inner: Arc<Inner>,
}

impl RingBuffer {
    /// Creates a ring on the heap.
    ///
    /// # Panics
    /// Panics if the capacity is not a power of two of at least 16, if the reader
    /// count is zero or above `u32::MAX`, or if the total size is above `isize::MAX`.
    pub fn create_in_memory(cfg: Config) -> Self {
        let geometry = layout(&cfg);
        let backing = Backing::heap(geometry.total_size);
        // SAFETY: fresh, aligned storage large enough for the checked geometry.
        unsafe { Self::initialize(backing, geometry) }
    }

    /// Creates a ring in caller-owned storage.
    ///
    /// Returns an error if `base` is not 16-byte aligned or `len` is too small
    /// for `cfg`.
    ///
    /// # Safety
    /// The region must be one writable allocation that allows interior mutation.
    /// Nothing else may access it during initialization. It must outlive every
    /// handle, clone, export, and borrow created from the ring. After
    /// initialization, all access must follow the ring protocol.
    ///
    /// # Panics
    /// Panics on an invalid configuration, like [`Self::create_in_memory`].
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

    /// Creates a ring backed by the file at `path`.
    ///
    /// Any existing file is truncated.
    ///
    /// # Safety
    /// The caller has exclusive access to the file during creation, and no
    /// earlier mappings or handles to it are in use. Until every handle and
    /// borrow from the ring is dropped, the file must not be truncated,
    /// reformatted, or modified outside the ring protocol.
    ///
    /// # Panics
    /// Panics on an invalid configuration, like [`Self::create_in_memory`].
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

    /// Maps an existing ring file and validates its header.
    ///
    /// # Safety
    /// The file contains an initialized ring. Until every handle and borrow from
    /// the ring is dropped, the file must not be truncated, reformatted, or
    /// modified outside the ring protocol. The header must never change.
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

    /// Attaches to an existing ring in caller-owned storage.
    ///
    /// Returns an error if the ring's header is invalid.
    ///
    /// # Safety
    /// The region is one live allocation that allows interior mutation and holds
    /// an initialized ring. It must outlive every handle, clone, export, and
    /// borrow created from the ring. The header must never change, and all other
    /// access must follow the ring protocol.
    pub unsafe fn attach_raw(base: *mut u8, len: usize) -> Result<Self, AttachError> {
        // SAFETY: the caller guarantees the region's lifetime and shared-memory access.
        unsafe { Self::attach(Backing::raw(base, len)) }
    }

    /// Attaches to an existing ring whose storage is kept alive by `owner`.
    ///
    /// # Safety
    /// `owner` must meet [`RawOwner`]'s contract and keep the entire region
    /// alive. The region holds an initialized ring whose header never changes,
    /// and all other access follows the ring protocol.
    pub unsafe fn attach_owned(
        base: *mut u8,
        len: usize,
        owner: RawOwner,
    ) -> Result<Self, AttachError> {
        // SAFETY: the caller supplies a live owner and valid shared ring storage.
        unsafe { Self::attach(Backing::owned(base, len, owner)) }
    }

    /// Exports this ring to pass across an ABI boundary.
    ///
    /// The returned [`RingExport`] keeps the region alive until the receiver takes
    /// its own reference. Storage from [`Self::create_raw`] or
    /// [`Self::attach_raw`] still relies on the caller to keep it alive.
    pub fn export(&self) -> RingExport {
        RingExport::new(self.inner.clone())
    }

    /// # Safety
    /// The backing holds an initialized ring whose header never changes. Its
    /// lifetime and all concurrent access must follow the ring protocol.
    unsafe fn attach(backing: Backing) -> Result<Self, AttachError> {
        // SAFETY: the caller guarantees readable, immutable header bytes.
        let geometry = unsafe { read_header(backing.base, backing.len) }?;
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

    /// Returns the region's base address and length.
    ///
    /// For raw storage, the pointer stays valid only as long as the caller keeps
    /// the storage alive.
    pub fn region(&self) -> (*mut u8, usize) {
        (self.inner.backing.base, self.inner.backing.len)
    }

    /// Returns the absolute published position in bytes.
    ///
    /// Wrap padding counts toward it.
    pub fn committed(&self) -> u64 {
        self.inner.control().committed.load(Acquire)
    }

    /// Returns the configuration this region was created with.
    pub fn config(&self) -> Config {
        Config {
            capacity: self.inner.geometry.capacity as usize,
            max_readers: self.inner.geometry.max_readers as usize,
        }
    }

    /// Claims the ring's single writer.
    ///
    /// Returns [`WriterClaimed`] if another writer holds the claim. Dropping the
    /// writer releases it. The writer notifies `data` after each publish. Pass
    /// [`NoWake`] if readers poll instead of waiting.
    pub fn writer<WD: WakeSource>(&self, data: WD) -> Result<Writer<WD>, WriterClaimed> {
        // Acquire the state released by the previous writer or reclaimer.
        self.inner
            .control()
            .writer
            .compare_exchange(0, owner_tag(), Acquire, Relaxed)
            .map_err(|_| WriterClaimed)?;
        Ok(Writer {
            committed: Cell::new(self.inner.control().committed.load(Relaxed)),
            inner: self.inner.clone(),
            data,
        })
    }

    /// Releases the writer claim and reader slots held by a dead process.
    ///
    /// # Safety
    /// `pid` is the nonzero ID of a process that has exited and been reaped. Its
    /// stores have finished, and its handles will never be used again. PID reuse
    /// must not have given that ID to a live claimant.
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

    /// Registers a new reader at the current published position.
    ///
    /// The view does not see records published before this call. Returns
    /// [`FullReaderTable`] if every reader slot is taken. The view waits for new
    /// data through `data`.
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
                    cursor: Cell::new(start),
                    inner: self.inner.clone(),
                    slot,
                    data,
                    pending: None,
                });
            }
        }
        Err(FullReaderTable)
    }
}

/// Returns the backing size `cfg` needs.
///
/// Returns `None` if `cfg` cannot form a valid region.
pub fn checked_region_len(cfg: &Config) -> Option<usize> {
    checked_layout(cfg).map(|geometry| geometry.total_size)
}

/// The ring's single writer.
///
/// Dropping it releases the writer claim, so another writer can take over.
/// `Writer` is `Send` but not `Sync`, so writes through `&Writer` never overlap:
///
/// ```compile_fail
/// use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
///
/// let ring = RingBuffer::create_in_memory(Config { capacity: 1024, max_readers: 1 });
/// let writer = ring.writer(NoWake).unwrap();
/// std::thread::scope(|s| {
///     s.spawn(|| writer.try_write(b"a"));
///     writer.try_write(b"b")
/// });
/// ```
pub struct Writer<WD: WakeSource> {
    inner: Arc<Inner>,
    data: WD,
    /// The writer's position.
    ///
    /// The copy in the region exists only for readers.
    committed: Cell<u64>,
}

impl<WD: WakeSource> Writer<WD> {
    /// Publishes one record without blocking.
    ///
    /// # Errors
    /// - [`WriteError::InsufficientCapacity`] if the record can never fit.
    /// - [`WriteError::WouldBlock`] if readers have not yet read the space the
    ///   record needs. The call may still publish wrap padding, but never part
    ///   of the record.
    /// - [`WriteError::Corrupt`] if a reader cursor is ahead of this writer.
    pub fn try_write(&self, bytes: &[u8]) -> Result<(), WriteError> {
        let cap = self.inner.geometry.capacity;
        let rec = checked_frame_len(bytes.len(), cap)?;
        let mut c = self.committed.get();
        if !writable_from(c, cap) {
            return Err(WriteError::Corrupt);
        }
        let (start_abs, mut gap) = reserve(c, rec, cap);
        if !self.fits(c, gap + rec)? {
            if gap == 0 || !self.fits(c, gap)? {
                return Err(WriteError::WouldBlock);
            }
            self.inner.control().hwm.store(c, Release);
            self.publish(start_abs);
            c = start_abs;
            gap = 0;
            if !self.fits(c, rec)? {
                self.data.notify();
                return Err(WriteError::WouldBlock);
            }
        }
        // SAFETY: reservation is contiguous, the length is valid, and the reader scan
        // allows this writer to reuse the destination.
        unsafe { self.commit(c, start_abs, start_abs + rec, gap, bytes) };
        Ok(())
    }

    #[inline]
    fn fits(&self, committed: u64, need: u64) -> Result<bool, WriteError> {
        let slowest = self.inner.slowest_active_cursor().unwrap_or(committed);
        fits(committed, slowest, need, self.inner.geometry.capacity)
    }

    /// Stores a new position locally, then in the region with Release ordering.
    #[inline]
    fn publish(&self, committed: u64) {
        self.committed.set(committed);
        self.inner.control().committed.store(committed, Release);
    }

    /// # Safety
    /// The writer claim is held. The record fits contiguously in space no reader
    /// pins, and `end_abs` is `start_abs` plus the record's frame length.
    unsafe fn commit(&self, committed: u64, start_abs: u64, end_abs: u64, gap: u64, bytes: &[u8]) {
        let phys = (start_abs & self.inner.geometry.mask()) as usize;
        // SAFETY: record is contiguous and in-bounds (caller contract).
        unsafe { self.inner.write_record(phys, bytes) };
        if gap > 0 {
            // Publish the wrap marker before the position that crosses it.
            self.inner.control().hwm.store(committed, Release);
        }
        // Release publishes record bytes to acquiring readers.
        self.publish(end_abs);
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

/// A reader with its own cursor.
///
/// Views never consume records for each other. Dropping a view frees its
/// reader slot, so the writer stops waiting on it.
pub struct View<RD: WakeSink> {
    inner: Arc<Inner>,
    slot: u32,
    /// The view's position.
    ///
    /// The copy in its slot exists for the writer.
    cursor: Cell<u64>,
    data: RD,
    /// The position a drain has read up to.
    ///
    /// It is applied to the cursor once the drain's slices can no longer be used.
    pending: Option<u64>,
}

impl<RD: WakeSink> View<RD> {
    /// Returns the absolute read position.
    ///
    /// Records a drain has yielded are not counted until the view settles.
    pub fn cursor(&self) -> u64 {
        self.cursor.get()
    }

    /// Returns the absolute published position in bytes.
    ///
    /// Wrap padding counts toward it.
    pub fn committed(&self) -> u64 {
        self.inner.control().committed.load(Acquire)
    }

    /// Returns the sink this view waits on.
    ///
    /// Use it when one caller waits on several views.
    pub fn wake(&self) -> &RD {
        &self.data
    }

    /// Returns whether an unread record is waiting.
    ///
    /// Wrap padding does not count.
    pub fn has_record(&self) -> bool {
        matches!(self.locate(), Ok(Some(_)))
    }

    /// Applies a drain's deferred consumption now.
    ///
    /// After this, [`Self::cursor`] is exact.
    pub fn settle(&mut self) {
        if let Some(pos) = self.pending.take() {
            self.advance(pos);
        }
    }

    /// Borrows the next unread record.
    ///
    /// Returns `None` if the view has caught up. Dropping the grant consumes the
    /// record and lets the writer reuse its space.
    pub fn try_read(&mut self) -> Result<Option<ReadGrant<'_>>, ReadError> {
        self.settle();
        let Some(loc) = self.locate()? else {
            return Ok(None);
        };
        Ok(Some(self.grant(loc)))
    }

    /// Skips older unread records and borrows the newest one.
    ///
    /// The newest record is not consumed. It stays pinned, so calling this again
    /// returns it again until a newer record arrives. Returns `None` when the
    /// view has no unread record.
    pub fn try_latest(&mut self) -> Result<Option<&[u8]>, ReadError> {
        self.settle();
        loop {
            let Some(loc) = self.locate()? else {
                return Ok(None);
            };
            let end = loc.end();
            if end >= self.data_end() {
                // SAFETY: lookup validated the record, and `locate` left the
                // cursor at its start, pinning it until the next mutable access.
                return Ok(Some(unsafe { self.inner.payload(&loc) }));
            }
            self.advance(end);
        }
    }

    fn locate(&self) -> Result<Option<Located>, ReadError> {
        let r = self.cursor.get();
        let (skipped, loc) = self.inner.locate_before(r, self.committed())?;
        if skipped != r {
            self.advance(skipped);
        }
        Ok(loc)
    }

    fn grant(&mut self, loc: Located) -> ReadGrant<'_> {
        // SAFETY: lookup validated the record; this exclusive view borrow keeps it pinned.
        let slice = unsafe { self.inner.payload(&loc) };
        ReadGrant {
            cursor: &self.cursor,
            slot: &self.inner.slot(self.slot).cursor,
            end: loc.end(),
            slice,
        }
    }

    /// Borrows every record published before this call.
    ///
    /// All the slices can be held at once, because consumption is deferred. The
    /// view's cursor moves at the next mutable operation on the view, once no
    /// slice is in use. If iteration stops early, only the records already
    /// yielded are consumed.
    pub fn drain(&mut self) -> Drain<'_> {
        self.settle();
        let pos = self.cursor();
        let committed = self.committed();
        let View { inner, pending, .. } = self;
        Drain {
            inner,
            pos,
            committed,
            pending,
            done: false,
        }
    }

    /// Returns the published position, minus a wrap gap at the very end.
    fn data_end(&self) -> u64 {
        let committed = self.inner.control().committed.load(Acquire);
        let hwm = self.inner.control().hwm.load(Acquire);
        if hwm < committed && self.inner.next_lap(hwm) == Some(committed) {
            hwm
        } else {
            committed
        }
    }

    #[inline]
    fn advance(&self, end_abs: u64) {
        store_cursor(&self.cursor, &self.inner.slot(self.slot).cursor, end_abs);
    }
}

impl<RD: WakeSink> Drop for View<RD> {
    fn drop(&mut self) {
        self.inner.slot(self.slot).owner.store(0, Relaxed);
        self.inner.slot(self.slot).cursor.store(FREE_SLOT, Release);
    }
}

/// Stores a view's position locally, then in its slot with Release ordering.
#[inline]
fn store_cursor(local: &Cell<u64>, slot: &AtomicU64, pos: u64) {
    local.set(pos);
    slot.store(pos, Release);
}

/// A borrowed record.
///
/// It derefs to the record's payload. Dropping it consumes the record.
pub struct ReadGrant<'a> {
    cursor: &'a Cell<u64>,
    slot: &'a AtomicU64,
    end: u64,
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
        store_cursor(self.cursor, self.slot, self.end);
    }
}

/// An iterator over unread records that defers consuming them.
///
/// See [`View::drain`].
pub struct Drain<'a> {
    inner: &'a Inner,
    pos: u64,
    committed: u64,
    pending: &'a mut Option<u64>,
    done: bool,
}

impl<'a> Iterator for Drain<'a> {
    type Item = Result<&'a [u8], ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.inner.locate_before(self.pos, self.committed) {
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

#[cfg(test)]
mod models;

#[cfg(all(test, not(ring_loom)))]
mod tests;

#[cfg(all(test, ring_loom))]
mod loom_tests;

#[cfg(kani)]
mod verify;
