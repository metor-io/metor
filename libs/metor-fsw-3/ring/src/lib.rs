//! A ring buffer of variable-length records, with one writer and many readers.
//!
//! The ring lives in one region of memory: on the heap, in a file that several
//! processes map, or in storage the caller provides. A [`Writer`] appends
//! records, and each [`View`] reads them at its own pace with its own cursor.
//! The writer never overwrites bytes that a view has not read. When a record
//! would need them, [`Writer::try_write`] returns [`WriteError::WouldBlock`]
//! until the slowest view catches up.
//!
//! Positions count bytes from the ring's creation and only grow. A position
//! modulo the capacity is an offset into the data region. Each record is a
//! 16-byte header followed by its payload, padded to 16 bytes. Records never
//! wrap around the end of the data region. When one would, the writer leaves the
//! last bytes of the lap empty and starts the record at the beginning of the
//! next lap, and readers skip the empty bytes.
//!
//! A new view starts where the writer is, so it never sees older records.
//! Reads borrow the payload in place, and the view's cursor stays put until the
//! borrow ends, which keeps the writer off those bytes.
//!
//! ```
//! use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
//!
//! let ring = RingBuffer::create_in_memory(Config { capacity: 1024, max_readers: 4 })?;
//! let mut writer = ring.writer(NoWake)?;
//! let mut view = ring.view(NoWake)?;
//! writer.try_write(b"hello")?;
//! if let Some(record) = view.try_read()? {
//!     assert_eq!(&*record, b"hello");
//! } // Dropping the grant consumes the record.
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! The `mmap` feature adds file-backed regions, and the `notify` feature adds
//! `Notifier` for async readers in the same process. Pass [`NoWake`] when
//! readers poll. `DESIGN.md` covers the region layout and memory ordering.

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod wake;

mod backing;
mod owner;
mod region;
mod sync;

use backing::Backing;
pub use owner::{Lease, RawOwner, RingExport};
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

/// Returns where the next frame starts, and the wrap gap before it.
///
/// Frames never cross the end of the data region. When the bytes left before
/// the end are too few for `frame_len`, the frame starts at the next lap and
/// those bytes become the gap. This only places the frame; [`fits`] checks
/// that readers have released the space. `capacity` must be a power of two,
/// `frame_len` at most `capacity`, and positions must not overflow.
#[inline]
const fn frame_start(committed: u64, frame_len: u64, capacity: u64) -> (u64, u64) {
    let offset = committed & (capacity - 1);
    let left_in_lap = capacity - offset;
    if frame_len > left_in_lap {
        (committed + left_in_lap, left_in_lap)
    } else {
        (committed, 0)
    }
}

/// Returns whether `write_len` more bytes fit without overwriting unread data.
///
/// Returns `Corrupt` if `slowest_cursor` is ahead of `committed` or the sum overflows.
#[inline]
const fn fits(
    committed: u64,
    slowest_cursor: u64,
    write_len: u64,
    capacity: u64,
) -> Result<bool, WriteError> {
    if slowest_cursor > committed {
        return Err(WriteError::Corrupt);
    }
    match (committed - slowest_cursor).checked_add(write_len) {
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

/// Returns whether a payload of `len` bytes, with its header at `offset`, ends
/// within the data region.
///
/// Unlike [`fits`], this ignores readers and only checks bounds. Use it on a raw
/// length read from the region before converting it to `usize`. `offset` must be
/// a 16-aligned offset in the data region.
#[inline]
const fn payload_in_bounds(len: u64, offset: u64, capacity: u64) -> bool {
    len <= capacity - RECORD_HEADER_SIZE as u64 - offset
}

fn checked_frame_len(payload_len: usize, capacity: u64) -> Result<u64, WriteError> {
    if payload_len > u32::MAX as usize || !payload_in_bounds(payload_len as u64, 0, capacity) {
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

/// Returned when a read finds the ring region corrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("ring region violates a structural invariant")]
pub struct ReadError;

/// Returned when every reader slot is already claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the ring's reader table is full")]
pub struct FullReaderTable;

/// Returned when a config cannot form a valid ring region.
///
/// The capacity must be a power of two of at least 16, the reader count must be
/// between 1 and `u32::MAX`, and the total size must fit `isize::MAX`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("ring capacity and reader count do not form a valid region")]
pub struct InvalidConfig;

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
    /// The region's header, control words, and reader table, within `backing`.
    region: *const Region,
}

// SAFETY: `region` points into `backing`, which is `Send` and `Sync`, and
// `Region` documents why sharing it is sound.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Inner {
    fn new(backing: Backing, geometry: Geometry) -> Self {
        let region = region_ptr(backing.base, geometry.max_readers);
        Self {
            backing,
            geometry,
            region,
        }
    }

    #[inline]
    fn region(&self) -> &Region {
        // SAFETY: validated geometry places the region within the live backing.
        unsafe { &*self.region }
    }

    #[inline]
    fn control(&self) -> &Control {
        &self.region().control
    }

    #[inline]
    fn slot(&self, slot: u32) -> &ReaderSlot {
        // PANIC Safety: callers pass slot < max_readers, the table's length.
        &self.region().slots[slot as usize]
    }

    /// Returns a pointer to `offset` bytes into the data region.
    ///
    /// # Safety
    /// `offset <= capacity`. The one-past pointer may only form an empty slice.
    #[inline]
    unsafe fn data_ptr(&self, offset: usize) -> *mut u8 {
        // SAFETY: validated geometry and offset <= capacity keep this within the allocation
        // or one byte position past its end.
        unsafe { self.backing.base.add(self.geometry.data_offset + offset) }
    }

    fn slowest_active_cursor(&self) -> Option<u64> {
        // Pair with the registration fence: a scan sees the claim or the reader sees publication.
        fence(SeqCst);
        self.region()
            .slots
            .iter()
            .map(|slot| slot.cursor.load(Acquire))
            .filter(|&cursor| cursor != FREE_SLOT)
            .min()
    }

    /// Writes a record's header and payload at `offset` without publishing it.
    ///
    /// The header's first word holds the payload length in its low 32 bits.
    /// The rest of the header is reserved.
    ///
    /// # Safety
    /// The caller holds the writer claim. The record is aligned, contiguous, and
    /// fits in the data region. Its length fits in a `u32`, and no reader cursor
    /// pins the destination.
    unsafe fn write_record(&self, offset: usize, payload: &[u8]) {
        // SAFETY: offset is 8-aligned and in-bounds (caller contract).
        let p = unsafe { self.data_ptr(offset) };
        let len = payload.len() as u64;
        // SAFETY: p is 8-aligned; [offset, offset+frame_len) is in-bounds.
        unsafe {
            (p as *mut u64).write(len);
            std::ptr::copy_nonoverlapping(
                payload.as_ptr(),
                p.add(RECORD_HEADER_SIZE),
                payload.len(),
            );
        }
    }

    /// Reads the payload length from the record header at `offset`.
    ///
    /// Only the low 32 bits hold the length. The mask drops the reserved high
    /// bits, which a peer could have set.
    ///
    /// # Safety
    /// `offset` is the aligned position of a record header with eight readable
    /// bytes. The record was published before this read, and a reader cursor
    /// keeps it pinned.
    unsafe fn read_len(&self, offset: usize) -> u64 {
        // SAFETY: offset is 8-aligned and in-bounds (caller contract); the read
        // is ordered after the write via `committed`.
        let hdr = unsafe { (self.data_ptr(offset) as *const u64).read() };
        hdr & 0xFFFF_FFFF
    }

    /// Finds the record at `r`, skipping a wrap gap if `r` sits on one.
    ///
    /// Returns the position after any skip, and the record if one was published
    /// before `committed`. Returns [`ReadError`] if `r` is more than a lap behind
    /// `committed` or is not on a 16-byte boundary.
    fn locate_before(&self, r: u64, committed: u64) -> Result<(u64, Option<Located>), ReadError> {
        if r >= committed {
            return Ok((r, None));
        }
        if committed - r > self.geometry.capacity || !r.is_multiple_of(PAYLOAD_ALIGNMENT as u64) {
            return Err(ReadError);
        }
        // A gap runs to the end of its lap, so skipping it lands on a lap start,
        // which is aligned and still within a lap of `committed`.
        let r = if r == self.control().hwm.load(Acquire) {
            self.next_lap(r).ok_or(ReadError)?
        } else {
            r
        };
        if r >= committed {
            return Ok((r, None));
        }
        let loc = self.record_at(r, committed)?;
        Ok((r, Some(loc)))
    }

    /// Reads and bounds-checks the header of the record at `r`.
    ///
    /// `r` is aligned, pinned, and within a lap below `committed`.
    fn record_at(&self, r: u64, committed: u64) -> Result<Located, ReadError> {
        let offset = (r & self.geometry.mask()) as usize;
        // SAFETY: r is pinned and below an acquired committed position.
        // Alignment and capacity >= 16 leave room for the header.
        let len = unsafe { self.read_len(offset) };
        if !payload_in_bounds(len, offset as u64, self.geometry.capacity) {
            return Err(ReadError);
        }
        let payload_len = len as usize;
        let frame_len = frame_len(payload_len);
        if frame_len as u64 > committed - r {
            return Err(ReadError);
        }
        Ok(Located {
            start: r,
            offset,
            payload_len,
            frame_len,
        })
    }

    /// Returns the start of the lap after `pos`, or `None` on overflow.
    #[inline]
    fn next_lap(&self, pos: u64) -> Option<u64> {
        (pos & !self.geometry.mask()).checked_add(self.geometry.capacity)
    }

    /// Borrows a located record's payload.
    ///
    /// # Safety
    /// A reader cursor at or before `loc.start` must stay in place for as long
    /// as the returned slice lives. That cursor is what stops the writer from
    /// reusing the bytes. `loc` must come from `locate_before`.
    unsafe fn payload(&self, loc: &Located) -> &[u8] {
        // SAFETY: the record is contiguous and in-bounds (caller contract).
        unsafe {
            let p = self.data_ptr(loc.offset + RECORD_HEADER_SIZE);
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
    /// Returns [`InvalidConfig`] if `cfg` cannot form a valid region.
    pub fn create_in_memory(cfg: Config) -> Result<Self, InvalidConfig> {
        let geometry = Geometry::checked_from_config(&cfg).ok_or(InvalidConfig)?;
        let backing = Backing::heap(geometry.total_size);
        // SAFETY: fresh, aligned storage large enough for the checked geometry.
        Ok(unsafe { Self::initialize(backing, geometry) })
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
    /// Panics if `cfg` cannot form a valid region (see [`InvalidConfig`]).
    pub unsafe fn create_raw(base: *mut u8, len: usize, cfg: Config) -> Result<Self, AttachError> {
        if !(base as usize).is_multiple_of(PAYLOAD_ALIGNMENT) {
            return Err(AttachError::Misaligned);
        }
        let geometry = Geometry::from_config(&cfg);
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
    /// Panics if `cfg` cannot form a valid region (see [`InvalidConfig`]).
    #[cfg(feature = "mmap")]
    pub unsafe fn create_mmap(path: &std::path::Path, cfg: Config) -> std::io::Result<Self> {
        let geometry = Geometry::from_config(&cfg);
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
            inner: Arc::new(Inner::new(backing, geometry)),
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
    /// writer releases it. The writer notifies `wake` after each publish. Pass
    /// [`NoWake`] if readers poll instead of waiting.
    pub fn writer<WD: WakeSource>(&self, wake: WD) -> Result<Writer<WD>, WriterClaimed> {
        // Acquire the state released by the previous writer or reclaimer.
        self.inner
            .control()
            .writer
            .compare_exchange(0, owner_tag(), Acquire, Relaxed)
            .map_err(|_| WriterClaimed)?;
        Ok(Writer {
            committed: Cell::new(self.inner.control().committed.load(Relaxed)),
            inner: self.inner.clone(),
            wake,
        })
    }

    /// Releases the writer claim and reader slots held by a dead process.
    ///
    /// # Safety
    /// `pid` is the nonzero ID of a process that has exited and been reaped. Its
    /// stores have finished, and its handles will never be used again. PID reuse
    /// must not have given that ID to a live claimant.
    pub unsafe fn reclaim_owner(&self, pid: u64) {
        for slot in &self.inner.region().slots {
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
    /// data through `wake`.
    pub fn view<RD: WakeSink>(&self, wake: RD) -> Result<View<RD>, FullReaderTable> {
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
                    wake,
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
    Geometry::checked_from_config(cfg).map(|geometry| geometry.total_size)
}

/// The ring's single writer.
///
/// Dropping it releases the writer claim, so another writer can take over.
/// `Writer` is `Send` but not `Sync`, so writes through `&Writer` never overlap:
///
/// ```compile_fail
/// use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
///
/// let ring = RingBuffer::create_in_memory(Config { capacity: 1024, max_readers: 1 }).unwrap();
/// let writer = ring.writer(NoWake).unwrap();
/// std::thread::scope(|s| {
///     s.spawn(|| writer.try_write(b"a"));
///     writer.try_write(b"b")
/// });
/// ```
pub struct Writer<WD: WakeSource> {
    inner: Arc<Inner>,
    wake: WD,
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
        let (start_abs, mut gap) = frame_start(c, rec, cap);
        // Readers may not have released the space for the gap and the frame yet.
        if !self.fits(c, gap + rec)? {
            // When only the gap fits, publish it alone. Readers then skip to the
            // next lap and release the space at its start.
            if gap == 0 || !self.fits(c, gap)? {
                return Err(WriteError::WouldBlock);
            }
            self.inner.control().hwm.store(c, Release);
            self.publish(start_abs);
            c = start_abs;
            gap = 0;
            if !self.fits(c, rec)? {
                self.wake.notify();
                return Err(WriteError::WouldBlock);
            }
        }
        // SAFETY: reservation is contiguous, the length is valid, and the reader scan
        // allows this writer to reuse the destination.
        unsafe { self.commit(c, start_abs, start_abs + rec, gap, bytes) };
        Ok(())
    }

    #[inline]
    fn fits(&self, committed: u64, write_len: u64) -> Result<bool, WriteError> {
        let slowest_cursor = self.inner.slowest_active_cursor().unwrap_or(committed);
        fits(
            committed,
            slowest_cursor,
            write_len,
            self.inner.geometry.capacity,
        )
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
        let offset = (start_abs & self.inner.geometry.mask()) as usize;
        // SAFETY: record is contiguous and in-bounds (caller contract).
        unsafe { self.inner.write_record(offset, bytes) };
        if gap > 0 {
            // Publish the wrap marker before the position that crosses it.
            self.inner.control().hwm.store(committed, Release);
        }
        // Release publishes record bytes to acquiring readers.
        self.publish(end_abs);
        self.wake.notify();
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
    offset: usize,
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
    wake: RD,
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
        &self.wake
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
