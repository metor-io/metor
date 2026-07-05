//! Lossless, shared-memory ring buffer for `metor-fsw-2`.
//!
//! This is the transport over which fsw-2 systems exchange data. It generalizes
//! the `metor-db` disruptor (`libs/db/src/disruptor.rs`) along one axis:
//! **shared memory.** The entire stateful buffer lives in one contiguous region
//! addressed by fixed byte offsets — no `Box`/`Arc`/process-local pointers
//! inside it — so the same mechanism works in-process and (with the `mmap`
//! feature) across processes.
//!
//! The buffer is **lossless**: the writer honors the disruptor's in-use check —
//! [`Writer::try_write`] returns [`WriteError::WouldBlock`] rather than
//! overwrite bytes a registered reader has not consumed, and the async
//! [`Writer::write`] suspends until a reader frees space. Because the writer
//! can never scribble a record a reader is looking at, reads borrow tear-free
//! ([`View::try_read`], [`View::try_latest`]) with no copy and no revalidation.
//!
//! The unsafe core borrows the disruptor's proven techniques: absolute
//! monotonic `u64` cursors with `phys = abs & mask`, a `committed`
//! Release/Acquire publication handshake, and a `high_water_mark` wrap gap. On
//! top of those, the single-writer rule is enforced by an in-region claim word
//! ([`RingBuffer::writer`]), and view registration runs a `SeqCst` handshake
//! against the writer's cursor scan (see [`RingBuffer::view`]). See
//! `libs/metor-fsw-2/docs/ring-buffer.md` for the full design and
//! `libs/db/MIRI.md` for the Miri strategy this crate follows.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicU64, fence,
    Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst},
};

// ---------------------------------------------------------------------------
// Layout constants
//
// All offsets are region-relative bytes. Multi-byte fields are stored in native
// byte order; `attach` rejects regions written by a different architecture (see
// `arch_tag`), so cross-endian reinterpretation never happens.
// ---------------------------------------------------------------------------

/// Region magic: `b"MFR1"` read as a native `u32`.
const MAGIC: u32 = u32::from_ne_bytes(*b"MFR1");
/// Layout version. Bumped on any incompatible layout change.
///
/// Version 2 removed the overwrite mode: the `reserved_end` seqlock word left
/// the control block (shifting `wake_word` and the writer claim down one word)
/// and the lossless flag bit was retired. Regions are ephemeral IPC state, not
/// archives; stale dev regions are simply recreated.
const VERSION: u16 = 2;

// Header (cache line 0).
const OFF_MAGIC: usize = 0x00; // u32
const OFF_VERSION: usize = 0x04; // u16
const OFF_FLAGS: usize = 0x06; // u16
const OFF_CAPACITY: usize = 0x08; // u64
const OFF_DATA_OFFSET: usize = 0x10; // u64
const OFF_MAX_READERS: usize = 0x18; // u32
const OFF_READER_TABLE_OFFSET: usize = 0x1C; // u32
const OFF_TOTAL_SIZE: usize = 0x20; // u64
const OFF_ARCH_TAG: usize = 0x28; // u64

// Control block (cache line 1).
const OFF_COMMITTED: usize = 0x40; // AtomicU64
const OFF_HWM: usize = 0x48; // AtomicU64
const OFF_WAKE_WORD: usize = 0x50; // AtomicU64 (reserved; future cross-proc wake)
const OFF_WRITER: usize = 0x58; // AtomicU64: writer claim, 0 = free (see `RingBuffer::writer`)

/// Header + control block size; the reader table starts here.
const HEADER_SIZE: usize = 0x80;
/// One reader-table slot, padded to a cache line to avoid false sharing.
const READER_SLOT_SIZE: usize = 64;
const SLOT_OFF_CURSOR: usize = 0x00; // AtomicU64
const SLOT_OFF_EPOCH: usize = 0x08; // AtomicU64

/// `flags` bit 0: a shared-memory wake word is present (reserved; never set in
/// v2, but the bit and [`OFF_WAKE_WORD`] reserve room for cross-process wake).
#[allow(dead_code)]
const FLAG_WAKE_SHARED: u16 = 1 << 0;

/// Sentinel `cursor` value marking a reader slot as free. A real absolute byte
/// cursor can never reach this (16 EiB committed).
const FREE_SLOT: u64 = u64::MAX;
/// `high_water_mark` value meaning "no pending wrap gap".
const HWM_NONE: u64 = u64::MAX;

/// A tag identifying the architecture that wrote a region. Comparing it on
/// attach rejects regions produced by a different pointer width or endianness:
/// the byte pattern of this native `u64` differs across endianness, and the low
/// word carries the pointer width.
const fn arch_tag() -> u64 {
    ((0x0102_0304u32 as u64) << 32) | (core::mem::size_of::<usize>() as u64)
}

/// Round `n` up to the next multiple of 8.
#[inline]
pub const fn round_up8(n: usize) -> usize {
    (n + 7) & !7
}

/// Total bytes a record with `payload_len` payload occupies: an 8-byte header
/// (`u32` length + `u32` pad) plus the payload padded up to an 8-byte boundary.
/// Always a multiple of 8, so every record start stays 8-byte aligned.
///
/// Public so buffer-sizing callers (fsw-2's system ports) can size a
/// ring from a frame's `MAX_SIZE` without re-deriving the header rule.
#[inline]
pub const fn frame_len(payload_len: usize) -> usize {
    8 + round_up8(payload_len)
}

// ---------------------------------------------------------------------------
// Public config / errors
// ---------------------------------------------------------------------------

/// Buffer geometry.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Data-region size in bytes. **Must be a power of two** (so `% cap` is a
    /// mask) and is therefore also a multiple of 8 (record alignment).
    pub capacity: usize,
    /// Number of reader-table slots. Over-provision: v2 has no crash-slot
    /// reclamation (see the design doc).
    pub max_readers: usize,
}

/// A write could not be performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The single message is larger than the whole data region.
    InsufficientCapacity,
    /// Writing now would overwrite the slowest active reader.
    WouldBlock,
}

// This crate is dependency-free by design, so the error types carry manual
// `Display`/`Error` impls rather than a `thiserror` derive (S1).
impl core::fmt::Display for WriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WriteError::InsufficientCapacity => {
                write!(f, "record is larger than the ring's whole data region")
            }
            WriteError::WouldBlock => write!(
                f,
                "ring is full: writing now would overwrite the slowest active reader"
            ),
        }
    }
}

impl std::error::Error for WriteError {}

/// A read could not be performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// The region violated a structural invariant (a record's length field says
    /// it straddles the wrap or overruns the data region): possible external
    /// corruption — stop reading. Unreachable from this crate's own behavior;
    /// it exists so a corrupted shared mapping degrades to an error instead of
    /// an out-of-bounds borrow.
    Corrupt,
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReadError::Corrupt => write!(
                f,
                "ring region violates a structural invariant (possible external corruption)"
            ),
        }
    }
}

impl std::error::Error for ReadError {}

/// The reader table is full; no free slot to register another view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullReaderTable;

impl core::fmt::Display for FullReaderTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the ring's reader table is full; no free slot for another view")
    }
}

impl std::error::Error for FullReaderTable {}

/// A writer already exists for this buffer (or a crashed process leaked its
/// claim — see [`RingBuffer::force_release_writer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterClaimed;

impl core::fmt::Display for WriterClaimed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "a writer already exists for this ring (or a crashed process leaked its claim)"
        )
    }
}

impl std::error::Error for WriterClaimed {}

/// A region's header was invalid or written by an incompatible build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    BadMagic,
    BadVersion,
    /// Pointer width / endianness mismatch (see [`arch_tag`]).
    ArchMismatch,
    /// Region shorter than the fixed header.
    TooSmall,
    /// [`RingBuffer::attach_raw`] base pointer not 8-byte aligned.
    Misaligned,
    /// Header fields are internally inconsistent: capacity not a nonzero power
    /// of two (or doesn't fit this target's `usize`), offsets misaligned,
    /// overlapping, or overflowing.
    BadGeometry,
    /// Header is self-consistent but `total_size` exceeds the backing region
    /// (e.g. a truncated file).
    RegionTruncated,
}

impl core::fmt::Display for AttachError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AttachError::BadMagic => write!(f, "region header has the wrong magic"),
            AttachError::BadVersion => {
                write!(f, "region was written by an incompatible ring version")
            }
            AttachError::ArchMismatch => write!(
                f,
                "region was written by a different pointer width or endianness"
            ),
            AttachError::TooSmall => write!(f, "region is shorter than the fixed header"),
            AttachError::Misaligned => write!(f, "region base pointer is not 8-byte aligned"),
            AttachError::BadGeometry => {
                write!(f, "region header fields are internally inconsistent")
            }
            AttachError::RegionTruncated => write!(
                f,
                "region header's total_size exceeds the backing region (truncated file?)"
            ),
        }
    }
}

impl std::error::Error for AttachError {}

// ---------------------------------------------------------------------------
// Backing storage
// ---------------------------------------------------------------------------

/// Pluggable backing storage for the region.
///
/// # Safety
///
/// Implementors must guarantee that [`Backing::base`] returns a pointer to a
/// single allocation of at least [`Backing::len`] bytes that is **interior
/// mutable** (writes through `*mut` derived from it are sound), **8-byte
/// aligned**, and **stable** for the lifetime of `self`. The ring forms
/// `&AtomicU64` references and plain byte slices over this region.
pub unsafe trait Backing: Send + Sync {
    /// Base of the region. Re-derived on every access to keep provenance fresh.
    fn base(&self) -> *mut u8;
    /// Length of the region in bytes.
    fn len(&self) -> usize;
    /// Whether the region is empty (clippy-friendly companion to `len`).
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One 8-byte, explicitly 8-aligned, interior-mutable word. `u64` alone is only
/// 4-byte aligned on some 32-bit targets (i686), which would leave the region's
/// `AtomicU64` control/cursor words unaligned — the `repr(align(8))` makes the
/// backing allocation 8-aligned on every target.
#[repr(C, align(8))]
struct Word(UnsafeCell<u64>);

/// In-process, heap-backed storage. Backed by `Box<[Word]>` so the allocation is
/// interior-mutable (sound to write through, Miri-clean) and 8-byte aligned on
/// all targets for the control/cursor atomics.
pub struct BoxBacking {
    buf: Box<[Word]>,
}

impl BoxBacking {
    /// Allocate a zeroed region of at least `size` bytes.
    pub fn new(size: usize) -> Self {
        let words = size.div_ceil(8);
        let buf = (0..words).map(|_| Word(UnsafeCell::new(0u64))).collect();
        Self { buf }
    }
}

// SAFETY: `UnsafeCell<u64>` is `!Sync`, but the ring upholds `Send + Sync`
// through its own synchronization (atomics + the `committed` handshake): the
// bytes are only ever touched through that documented discipline.
unsafe impl Send for BoxBacking {}
unsafe impl Sync for BoxBacking {}

// SAFETY: `buf` is a single boxed slice of 8-aligned interior-mutable words,
// stable (heap allocation owned by `self`). `as_ptr()` carries whole-allocation
// provenance, matching the contract.
unsafe impl Backing for BoxBacking {
    fn base(&self) -> *mut u8 {
        self.buf.as_ptr() as *mut u8
    }
    fn len(&self) -> usize {
        self.buf.len() * 8
    }
}

/// mmap-backed storage (cross-process capable). Behind the `mmap` feature.
#[cfg(feature = "mmap")]
pub struct MmapBacking {
    map: memmap2::MmapMut,
}

#[cfg(feature = "mmap")]
// SAFETY: an `mmap`ed region is a stable, interior-mutable, page-aligned (hence
// 8-byte aligned) allocation, satisfying the `Backing` contract.
unsafe impl Backing for MmapBacking {
    fn base(&self) -> *mut u8 {
        self.map.as_ptr() as *mut u8
    }
    fn len(&self) -> usize {
        self.map.len()
    }
}

/// Non-owning storage over a region someone else owns (the host, or another
/// process's mmap). Its `Drop` frees nothing; the region's own atomics carry all
/// synchronization. Used by [`RingBuffer::attach_raw`] for same-process dlopen'd
/// systems that reconstruct a ring over the host's backing.
///
/// # Safety
///
/// The `(base, len)` pair handed to [`RingBuffer::attach_raw`] must name a live,
/// header-valid ring region — `base..base+len` interior-mutable and 8-byte
/// aligned (e.g. another [`Backing`]'s region) — that **outlives every
/// [`Writer`]/[`View`] produced from it** and is **not concurrently torn down**.
pub struct RawBacking {
    base: *mut u8,
    len: usize,
}

// SAFETY: like `BoxBacking`, a `RawBacking` is `!Sync` by construction (a bare
// pointer), but the ring upholds `Send + Sync` through its own synchronization
// (the region's atomics + the `committed` handshake): the bytes are only ever
// touched through that documented discipline.
unsafe impl Send for RawBacking {}
unsafe impl Sync for RawBacking {}

// SAFETY: `attach_raw`'s caller asserts `base..base+len` is a stable,
// interior-mutable, 8-byte-aligned ring region (laid out by `init_region`
// through an interior-mutable backing). `RawBacking` only borrows it — `Drop`
// frees nothing — and re-derives the pointer on every access.
unsafe impl Backing for RawBacking {
    fn base(&self) -> *mut u8 {
        self.base
    }
    fn len(&self) -> usize {
        self.len
    }
}

// ---------------------------------------------------------------------------
// Async wake abstraction (kept out of the shared layout)
// ---------------------------------------------------------------------------

/// Signals that progress was made (new data committed, or space freed).
pub trait WakeSource {
    fn notify(&self);
}

/// Awaits progress: completes once `ready()` returns true. Implementations must
/// avoid lost wakeups by re-checking `ready` after arming.
#[allow(async_fn_in_trait)]
pub trait WakeSink {
    async fn wait_until<F: FnMut() -> bool>(&self, ready: F);
}

/// No-op wake. The `try_*` paths never touch the wake hooks, so this is the
/// right choice for synchronous consumers (and the Miri tests). The async paths
/// degenerate to caller-driven polling under `NoWake`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoWake;

impl WakeSource for NoWake {
    fn notify(&self) {}
}

impl WakeSink for NoWake {
    async fn wait_until<F: FnMut() -> bool>(&self, mut ready: F) {
        // Resolve immediately; the caller's loop re-polls. Real blocking needs a
        // proper notifier (e.g. `Notifier`).
        let _ = ready();
    }
}

/// In-process notifier backed by a `stellarator` `WaitQueue`. A single clone is
/// shared by the writer and the views for one direction (data- or
/// space-available). Behind the `async` feature.
#[cfg(feature = "async")]
#[derive(Clone)]
pub struct Notifier(Arc<stellarator::sync::WaitQueue>);

#[cfg(feature = "async")]
impl Default for Notifier {
    fn default() -> Self {
        Self(Arc::new(stellarator::sync::WaitQueue::new()))
    }
}

#[cfg(feature = "async")]
impl WakeSource for Notifier {
    fn notify(&self) {
        self.0.wake_all();
    }
}

#[cfg(feature = "async")]
impl WakeSink for Notifier {
    async fn wait_until<F: FnMut() -> bool>(&self, ready: F) {
        let _ = self.0.wait_for(ready).await;
    }
}

// ---------------------------------------------------------------------------
// Inner: the shared region + cached, process-local geometry
// ---------------------------------------------------------------------------

struct Inner<B: Backing> {
    backing: B,
    capacity: u64,
    mask: u64,
    data_offset: usize,
    reader_table_offset: usize,
    max_readers: u32,
}

// SAFETY: every access to the shared region goes through atomics or the
// `committed` Release/Acquire handshake. The in-use check gives the
// writer-then-reader happens-before that makes the plain data accesses
// race-free, exactly as in the db disruptor. `B: Send + Sync`.
unsafe impl<B: Backing> Send for Inner<B> {}
unsafe impl<B: Backing> Sync for Inner<B> {}

impl<B: Backing> Inner<B> {
    #[inline]
    fn base(&self) -> *mut u8 {
        self.backing.base()
    }

    /// `&AtomicU64` at region offset `off`.
    ///
    /// # Safety
    /// `off + 8 <= region len`, and `off` is 8-byte aligned.
    #[inline]
    unsafe fn atomic_u64(&self, off: usize) -> &AtomicU64 {
        // SAFETY: caller guarantees bounds + alignment; the backing is
        // interior-mutable so an `&AtomicU64` over it is sound, and provenance is
        // re-derived from `base()`.
        unsafe { &*(self.base().add(off) as *const AtomicU64) }
    }

    #[inline]
    fn committed(&self) -> &AtomicU64 {
        // SAFETY: OFF_COMMITTED is a fixed, 8-aligned, in-bounds control word.
        unsafe { self.atomic_u64(OFF_COMMITTED) }
    }

    #[inline]
    fn hwm(&self) -> &AtomicU64 {
        // SAFETY: OFF_HWM is a fixed, 8-aligned, in-bounds control word.
        unsafe { self.atomic_u64(OFF_HWM) }
    }

    /// The writer-claim word: `0` = free, `1` = a live [`Writer`] exists (or a
    /// crashed one leaked the claim). See [`RingBuffer::writer`].
    #[inline]
    fn writer_claim(&self) -> &AtomicU64 {
        // SAFETY: OFF_WRITER is a fixed, 8-aligned, in-bounds control word.
        unsafe { self.atomic_u64(OFF_WRITER) }
    }

    #[inline]
    fn slot_cursor(&self, slot: u32) -> &AtomicU64 {
        let off = self.reader_table_offset + slot as usize * READER_SLOT_SIZE + SLOT_OFF_CURSOR;
        // SAFETY: slot < max_readers ⇒ off is within the reader table, 8-aligned.
        unsafe { self.atomic_u64(off) }
    }

    #[inline]
    fn slot_epoch(&self, slot: u32) -> &AtomicU64 {
        let off = self.reader_table_offset + slot as usize * READER_SLOT_SIZE + SLOT_OFF_EPOCH;
        // SAFETY: slot < max_readers ⇒ off is within the reader table, 8-aligned.
        unsafe { self.atomic_u64(off) }
    }

    /// Pointer to byte `phys` of the data region.
    ///
    /// # Safety
    /// `phys < capacity`.
    #[inline]
    unsafe fn data_ptr(&self, phys: usize) -> *mut u8 {
        // SAFETY: caller guarantees `phys < capacity`, and `data_offset + phys`
        // is within the region; provenance is whole-allocation from `base()`.
        unsafe { self.base().add(self.data_offset + phys) }
    }

    /// The smallest cursor over all *active* readers, or `None` if there are no
    /// active readers. Used by the in-use check (mirrors the disruptor's
    /// `slowest_cursor`).
    fn slowest_active_cursor(&self) -> Option<u64> {
        // SeqCst: pairs with the registration fence in `view()`. This is
        // Dekker's pattern on two locations — reader: `store(cursor);
        // load(committed)`, writer: `store(committed); load(cursor)` — where
        // Release/Acquire alone allows *both* loads to read the older values
        // (StoreLoad reordering). The fence sits between this writer's previous
        // `committed` store and this scan, so for every write W: either W's
        // scan observes a new reader's claim, or that reader's registration
        // recheck observes committed_{W-1} (see `view()`).
        fence(SeqCst);
        let mut slowest: Option<u64> = None;
        for slot in 0..self.max_readers {
            let v = self.slot_cursor(slot).load(Acquire);
            if v != FREE_SLOT {
                slowest = Some(slowest.map_or(v, |s| s.min(v)));
            }
        }
        slowest
    }

    /// Compute the wrap for a record of `rec` bytes written at absolute
    /// `committed`. Returns `(start_abs, gap)`; `gap > 0` means a wrap gap was
    /// left at the end of the current lap (its size is a multiple of 8).
    #[inline]
    fn reserve(&self, committed: u64, rec: u64) -> (u64, u64) {
        let phys = committed & self.mask;
        if phys + rec > self.capacity {
            let gap = self.capacity - phys;
            (committed + gap, gap)
        } else {
            (committed, 0)
        }
    }

    /// Write a record's header + payload at physical offset `phys`. Plain
    /// stores are race-free: the in-use check guarantees no reader is inside
    /// these bytes, and publication happens-before every read via the
    /// `committed` Release/Acquire pair — exactly the disruptor's write path.
    ///
    /// # Safety
    /// `phys + frame_len(payload.len()) <= capacity` (record does not straddle
    /// the wrap), and the caller holds the single-writer role.
    unsafe fn write_record(&self, phys: usize, payload: &[u8]) {
        // SAFETY: phys is 8-aligned and in-bounds (caller contract).
        let p = unsafe { self.data_ptr(phys) };
        let len = payload.len() as u64; // low 32 bits = length, high = pad.
        // SAFETY: p is 8-aligned; [phys, phys+frame_len) is in-bounds.
        unsafe {
            (p as *mut u64).write(len);
            std::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(8), payload.len());
        }
    }

    /// Read a record's length field at physical offset `phys`. Returns the raw
    /// `u32` widened to `u64`: a corrupted region can carry arbitrary bytes, so
    /// the caller must validate it against the data region *before* any `usize`
    /// conversion or pointer math (on a 32-bit target, `frame_len(garbage)`
    /// would wrap `usize` and defeat the straddle check).
    ///
    /// # Safety
    /// `phys + 8 <= capacity`, and the record was published (its write
    /// happens-before this read via `committed`).
    unsafe fn read_len(&self, phys: usize) -> u64 {
        // SAFETY: phys is 8-aligned and in-bounds (caller contract); the read
        // is ordered after the write via `committed`.
        let hdr = unsafe { (self.data_ptr(phys) as *const u64).read() };
        hdr & 0xFFFF_FFFF
    }

    /// Copy `len` payload bytes starting at `phys + 8` into `dst`.
    ///
    /// # Safety
    /// `phys + 8 + len <= capacity`, and the record was published (its write
    /// happens-before this read via `committed`).
    unsafe fn copy_payload(&self, phys: usize, len: usize, dst: &mut Vec<u8>) {
        dst.clear();
        dst.resize(len, 0);
        // SAFETY: payload occupies [phys+8, phys+8+len) (caller contract);
        // ordered after the write via `committed`; non-overlapping.
        unsafe {
            std::ptr::copy_nonoverlapping(self.data_ptr(phys + 8), dst.as_mut_ptr(), len)
        };
    }
}

// ---------------------------------------------------------------------------
// RingBuffer
// ---------------------------------------------------------------------------

/// A handle to a ring buffer region. Cheaply clonable (`Arc`-backed); the writer
/// and views are produced from it and may outlive the original handle.
pub struct RingBuffer<B: Backing> {
    inner: Arc<Inner<B>>,
}

impl<B: Backing> Clone for RingBuffer<B> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl RingBuffer<BoxBacking> {
    /// Create an in-process, heap-backed ring buffer.
    ///
    /// Panics if `capacity` is not a non-zero power of two, or `max_readers` is
    /// zero.
    pub fn create_in_memory(cfg: Config) -> Self {
        let (reader_table_offset, data_offset, total) = layout(&cfg);
        let backing = BoxBacking::new(total);
        // SAFETY: `backing` is a fresh region of `total` bytes, exclusively owned
        // here (no other handle exists yet), so single-threaded init is sound.
        unsafe { init_region(&backing, &cfg, reader_table_offset, data_offset, total) };
        RingBuffer {
            inner: Arc::new(Inner {
                backing,
                capacity: cfg.capacity as u64,
                mask: cfg.capacity as u64 - 1,
                data_offset,
                reader_table_offset,
                max_readers: cfg.max_readers as u32,
            }),
        }
    }
}

#[cfg(feature = "mmap")]
impl RingBuffer<MmapBacking> {
    /// Create a new mmap-backed region at `path` (truncating any existing file).
    pub fn create_mmap(path: &std::path::Path, cfg: Config) -> std::io::Result<Self> {
        let (reader_table_offset, data_offset, total) = layout(&cfg);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(total as u64)?;
        // SAFETY: mapping a file we just sized to `total`; we hold it exclusively.
        let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        let backing = MmapBacking { map };
        // SAFETY: freshly sized, exclusively-held region; single-threaded init.
        unsafe { init_region(&backing, &cfg, reader_table_offset, data_offset, total) };
        Ok(RingBuffer {
            inner: Arc::new(Inner {
                backing,
                capacity: cfg.capacity as u64,
                mask: cfg.capacity as u64 - 1,
                data_offset,
                reader_table_offset,
                max_readers: cfg.max_readers as u32,
            }),
        })
    }

    /// Attach to an existing mmap-backed region, validating the header.
    ///
    /// # Safety
    /// The caller asserts `path` is a ring-buffer region created by a compatible
    /// build and is not being concurrently torn down. The magic/version/arch
    /// handshake guards against accidental misuse but not deliberate corruption.
    pub unsafe fn attach_mmap(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        // SAFETY: caller asserts a valid region; mapping read+write shared.
        let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        // SAFETY: caller asserts a live, valid region; `from_validated` reads and
        // checks the header before forming any reference into it.
        unsafe { Self::from_validated(MmapBacking { map }) }
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

impl RingBuffer<RawBacking> {
    /// Attach a non-owning handle to a host-provided ring region, validating its
    /// header and reading geometry back out of it (the same-process dlopen path). The
    /// caller keeps owning the region; this handle's [`RawBacking`] frees nothing.
    ///
    /// This is [`RingBuffer::attach_mmap`] with the mapping step removed: the
    /// caller supplies the `(base, len)` directly (e.g. another backing's
    /// [`Backing::base`]/[`Backing::len`]).
    ///
    /// # Safety
    /// `base..base+len` is a live ring region (header already laid out and
    /// validated) that outlives every [`Writer`]/[`View`] produced here and is
    /// not torn down concurrently.
    pub unsafe fn attach_raw(base: *mut u8, len: usize) -> Result<Self, AttachError> {
        // The raw path takes an arbitrary pointer (mmap and `BoxBacking` are
        // aligned by construction), so alignment is checked here; everything
        // else — including the region-shorter-than-header case — is validated
        // by the shared `read_header` path before any out-of-bounds read.
        if !(base as usize).is_multiple_of(8) {
            return Err(AttachError::Misaligned);
        }
        // SAFETY: caller asserts a live region of `len` readable bytes;
        // `from_validated` bounds every header read against that length.
        unsafe { Self::from_validated(RawBacking { base, len }) }
    }
}

impl<B: Backing> RingBuffer<B> {
    /// Rebuild a handle over an already-laid-out region: validate its header and
    /// read the geometry (capacity/offsets/`max_readers`) back out of it rather
    /// than from arguments. Shared by [`RingBuffer::attach_mmap`] and
    /// [`RingBuffer::attach_raw`].
    ///
    /// # Safety
    /// `backing`'s region is live (all [`Backing::len`] bytes readable) and is
    /// not being concurrently torn down. `read_header` validates everything
    /// else — size, magic/version/arch, and geometry — before any reference is
    /// formed into the region.
    unsafe fn from_validated(backing: B) -> Result<Self, AttachError> {
        let base = backing.base();
        // SAFETY: `backing.len()` bytes are readable (caller contract);
        // `read_header` bounds every read against that length.
        let geo = unsafe { read_header(base, backing.len()) }?;
        Ok(RingBuffer {
            inner: Arc::new(Inner {
                backing,
                capacity: geo.capacity,
                mask: geo.capacity - 1,
                data_offset: geo.data_offset,
                reader_table_offset: geo.reader_table_offset,
                max_readers: geo.max_readers,
            }),
        })
    }

    /// The backing region's `(base, len)`, for handing a second handle the same
    /// bytes to attach to (the same-process dlopen scenario): the host reads
    /// `(base, len)` here to fill an `FswRing` the system reconstructs
    /// via [`RingBuffer::attach_raw`]. The pointer is only valid while `self` (or
    /// another handle/backing keeping the region alive) lives.
    pub fn region(&self) -> (*mut u8, usize) {
        (self.inner.backing.base(), self.inner.backing.len())
    }

    /// The absolute number of bytes published so far.
    pub fn committed(&self) -> u64 {
        self.inner.committed().load(Acquire)
    }

    /// Create the single writer for this buffer, claiming the writer role in the
    /// shared region. Returns [`WriterClaimed`] if a writer already exists — the
    /// claim lives in the region itself, so enforcement is cross-handle and
    /// cross-process (a second process attaching to a claimed region is rejected
    /// too). The claim is freed when the [`Writer`] drops; a crashed process
    /// leaks it (see [`RingBuffer::force_release_writer`]).
    ///
    /// `data` is notified after every commit (data-available); `space` is awaited
    /// by the async [`Writer::write`] until a reader frees room. Use
    /// [`NoWake`] for both when only the synchronous APIs are used.
    pub fn writer<WD: WakeSource, WS: WakeSink>(
        &self,
        data: WD,
        space: WS,
    ) -> Result<Writer<B, WD, WS>, WriterClaimed> {
        // Acquire on success: pairs with the Release store in `Writer::drop` /
        // `force_release_writer`, so drop→claim forms a synchronizes-with edge
        // handing the whole region state (committed/hwm and data bytes) from
        // the previous writer to this one. Relaxed on failure: no state is read
        // when the claim is lost.
        self.inner
            .writer_claim()
            .compare_exchange(0, 1, Acquire, Relaxed)
            .map_err(|_| WriterClaimed)?;
        Ok(Writer {
            inner: self.inner.clone(),
            data,
            space,
        })
    }

    /// Forcibly release a leaked writer claim.
    ///
    /// # Safety
    /// The caller asserts the claiming writer no longer exists (its process
    /// crashed or its [`Writer`] was leaked) and none of its stores are still in
    /// flight. Calling this while the writer is alive re-creates the very
    /// two-writer race the claim word exists to prevent.
    pub unsafe fn force_release_writer(&self) {
        // Release: matches `Writer::drop` so the next claimer's Acquire CAS
        // orders after whatever state the caller observed before reclaiming.
        self.inner.writer_claim().store(0, Release);
    }

    /// Register a new view (reader), claiming a free slot in the reader table.
    ///
    /// `data` is awaited by the async [`View::read_into`] for new data; `space`
    /// is notified whenever the view advances (waking a waiting writer).
    /// A fresh view only sees data committed from now on.
    pub fn view<RD: WakeSink, RS: WakeSource>(
        &self,
        data: RD,
        space: RS,
    ) -> Result<View<B, RD, RS>, FullReaderTable> {
        // A new reader starts at the current commit point (mirrors
        // `Disruptor::reader`): it never sees older data.
        let mut start = self.inner.committed().load(Acquire);
        for slot in 0..self.inner.max_readers {
            // Claim ordering: AcqRel — Acquire pairs with `View::drop`'s Release
            // store of FREE_SLOT (slot-state handoff between successive owners);
            // Release publishes the claim store itself. NOTE: visibility of this
            // claim to the *writer's* `fits()` scan is guaranteed by the SeqCst
            // registration handshake below, not by this CAS. The epoch word is a
            // generation counter reserved for crash reclamation; it is written
            // (Release) but never yet read. If reclamation is ever implemented,
            // the reclaimer must bump the epoch *before* freeing the cursor,
            // and every cursor store made through a View handle must be
            // preceded by an epoch check (or become a CAS on (epoch, cursor)) —
            // a plain Release store on a reclaimed slot would corrupt the new
            // owner's cursor.
            if self
                .inner
                .slot_cursor(slot)
                .compare_exchange(FREE_SLOT, start, AcqRel, Relaxed)
                .is_ok()
            {
                // Bump the generation so a stale handle to a reused slot is
                // distinguishable (reserved for future crash reclamation).
                self.inner.slot_epoch(slot).fetch_add(1, Release);
                // Registration handshake: loop until the claim is provably
                // stable. Until the writer's `fits()` scan observes the claim,
                // its in-use check is vacuous and it could write past
                // `start + capacity` — after which the borrow path would hand
                // out overwritten bytes. The SeqCst fence pairs with the one in
                // `slowest_active_cursor()`: in the fence total order, either
                // the writer's scan is later and must observe our cursor store,
                // or our recheck is later and must observe that writer's
                // `committed` — requiring a *stable* `committed` therefore
                // proves every unseen write was bounded by some other cursor
                // <= ours. Converges in 1-2 iterations (each extra one needs a
                // commit inside a ~3-instruction window); registration is a
                // cold path, so no iteration bound is imposed.
                loop {
                    fence(SeqCst);
                    // Acquire: on the break path, orders this view's later
                    // record reads after the writes committed before `start`.
                    let c2 = self.inner.committed().load(Acquire);
                    if c2 == start {
                        break;
                    }
                    // The writer committed while our claim may not have been
                    // visible to its scan; those writes were validated
                    // without us. Advance the claim to the new edge and
                    // re-verify. "A fresh view only sees data committed from
                    // now on" makes this a semantic no-op.
                    start = c2;
                    // Release: publishes the cursor for the writer's scan.
                    self.inner.slot_cursor(slot).store(start, Release);
                }
                return Ok(View {
                    inner: self.inner.clone(),
                    slot,
                    data,
                    space,
                });
            }
        }
        Err(FullReaderTable)
    }

    /// Number of currently registered views.
    pub fn reader_count(&self) -> usize {
        (0..self.inner.max_readers)
            .filter(|&s| self.inner.slot_cursor(s).load(Relaxed) != FREE_SLOT)
            .count()
    }
}

/// Compute `(reader_table_offset, data_offset, total_size)` and validate config.
fn layout(cfg: &Config) -> (usize, usize, usize) {
    assert!(
        cfg.capacity.is_power_of_two(),
        "capacity must be a power of two, got {}",
        cfg.capacity
    );
    // Mirrors the attach-side geometry check: below one record header no write
    // can ever succeed, and `read_len`'s `phys + 8 <= capacity` contract breaks.
    assert!(
        cfg.capacity >= 8,
        "capacity must hold at least one record header (8 bytes), got {}",
        cfg.capacity
    );
    assert!(cfg.max_readers > 0, "max_readers must be > 0");
    let reader_table_offset = HEADER_SIZE;
    let data_offset = HEADER_SIZE + cfg.max_readers * READER_SLOT_SIZE;
    let total = data_offset + cfg.capacity;
    (reader_table_offset, data_offset, total)
}

/// Write the header and initialize control words + reader slots.
///
/// # Safety
/// `backing` is a freshly allocated, exclusively-owned region of `total` bytes;
/// this runs single-threaded before any handle is published.
unsafe fn init_region<B: Backing>(
    backing: &B,
    cfg: &Config,
    reader_table_offset: usize,
    data_offset: usize,
    total: usize,
) {
    let base = backing.base();
    debug_assert!(backing.len() >= total);
    // SAFETY: all offsets are within `total` bytes and correctly aligned; no
    // other thread can observe these writes yet.
    unsafe {
        (base.add(OFF_MAGIC) as *mut u32).write(MAGIC);
        (base.add(OFF_VERSION) as *mut u16).write(VERSION);
        (base.add(OFF_FLAGS) as *mut u16).write(0);
        (base.add(OFF_CAPACITY) as *mut u64).write(cfg.capacity as u64);
        (base.add(OFF_DATA_OFFSET) as *mut u64).write(data_offset as u64);
        (base.add(OFF_MAX_READERS) as *mut u32).write(cfg.max_readers as u32);
        (base.add(OFF_READER_TABLE_OFFSET) as *mut u32).write(reader_table_offset as u32);
        (base.add(OFF_TOTAL_SIZE) as *mut u64).write(total as u64);
        (base.add(OFF_ARCH_TAG) as *mut u64).write(arch_tag());
        (base.add(OFF_COMMITTED) as *mut u64).write(0);
        (base.add(OFF_HWM) as *mut u64).write(HWM_NONE);
        (base.add(OFF_WAKE_WORD) as *mut u64).write(0);
        (base.add(OFF_WRITER) as *mut u64).write(0);
        for slot in 0..cfg.max_readers {
            let so = reader_table_offset + slot * READER_SLOT_SIZE;
            (base.add(so + SLOT_OFF_CURSOR) as *mut u64).write(FREE_SLOT);
            (base.add(so + SLOT_OFF_EPOCH) as *mut u64).write(0);
        }
    }
}

/// The geometry `read_header` recovers and validates from a region's header.
struct Geometry {
    capacity: u64,
    data_offset: usize,
    reader_table_offset: usize,
    max_readers: u32,
}

/// Validate and read the immutable header fields from an existing region.
///
/// After `Ok`, every offset the ring ever dereferences — control words, all
/// `max_readers` reader slots, and `data_offset + phys` for `phys < capacity` —
/// is inside `[0, region_len)` and 8-aligned: attach restores the same geometry
/// invariant `layout()` + `init_region` establish at creation, which is what
/// the `SAFETY` comments on `atomic_u64`/`data_ptr` assume. All arithmetic is
/// checked `u64`, so a hostile header cannot overflow its way past a bound.
///
/// # Safety
/// `base` points at a readable region of at least `region_len` bytes.
unsafe fn read_header(base: *mut u8, region_len: usize) -> Result<Geometry, AttachError> {
    // A region too small to hold the header cannot carry a valid one; reject it
    // before any header field is read so nothing touches out-of-bounds bytes
    // (a sub-header mmap of a truncated file lands here too).
    if region_len < HEADER_SIZE {
        return Err(AttachError::TooSmall);
    }
    // SAFETY: header fields are within HEADER_SIZE <= region_len and written
    // once at creation; reading them by value is sound.
    unsafe {
        if (base.add(OFF_MAGIC) as *const u32).read() != MAGIC {
            return Err(AttachError::BadMagic);
        }
        if (base.add(OFF_VERSION) as *const u16).read() != VERSION {
            return Err(AttachError::BadVersion);
        }
        if (base.add(OFF_ARCH_TAG) as *const u64).read() != arch_tag() {
            return Err(AttachError::ArchMismatch);
        }
        let capacity = (base.add(OFF_CAPACITY) as *const u64).read();
        let data_offset = (base.add(OFF_DATA_OFFSET) as *const u64).read();
        let max_readers = (base.add(OFF_MAX_READERS) as *const u32).read();
        let reader_table_offset = (base.add(OFF_READER_TABLE_OFFSET) as *const u32).read() as u64;
        let total_size = (base.add(OFF_TOTAL_SIZE) as *const u64).read();

        // Capacity: a power of two (so `mask = capacity - 1` is well-defined),
        // at least one record header (8 bytes — `read_len`'s `phys + 8 <=
        // capacity` contract needs it, and it keeps every 8-aligned record
        // start dereferenceable), and fitting this target's `usize` (a 32-bit
        // target can attach a region sized by a 64-bit creator).
        if !capacity.is_power_of_two() || capacity < 8 || capacity > usize::MAX as u64 {
            return Err(AttachError::BadGeometry);
        }
        if max_readers == 0 {
            return Err(AttachError::BadGeometry);
        }
        // Reader table in bounds: behind the header, 8-aligned, and ending at
        // or before the data region.
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
        // Data region in bounds: 8-aligned and ending at or before total_size.
        let data_end = data_offset
            .checked_add(capacity)
            .ok_or(AttachError::BadGeometry)?;
        if !data_offset.is_multiple_of(8) || data_end > total_size {
            return Err(AttachError::BadGeometry);
        }
        // The self-declared total must fit the actual backing region — the
        // truncated-on-disk file case gets its own variant because it is the
        // diagnosable real-world failure.
        if total_size > region_len as u64 {
            return Err(AttachError::RegionTruncated);
        }

        Ok(Geometry {
            capacity,
            // Both fit usize: they are <= total_size <= region_len: usize.
            data_offset: data_offset as usize,
            reader_table_offset: reader_table_offset as usize,
            max_readers,
        })
    }
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// The single producer for a buffer. There must be at most one live writer per
/// buffer; the writer is the sole mutator of `committed`/`high_water_mark`.
pub struct Writer<B: Backing, WD: WakeSource, WS: WakeSink> {
    inner: Arc<Inner<B>>,
    data: WD,
    space: WS,
}

impl<B: Backing, WD: WakeSource, WS: WakeSink> Writer<B, WD, WS> {
    /// Write one message without blocking. Returns [`WriteError::WouldBlock`]
    /// if writing now would overwrite the slowest active reader.
    pub fn try_write(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        let rec = frame_len(bytes.len()) as u64;
        if rec > self.inner.capacity {
            return Err(WriteError::InsufficientCapacity);
        }
        let c = self.inner.committed().load(Relaxed); // sole writer
        let (start_abs, gap) = self.inner.reserve(c, rec);
        if !self.fits(c, gap + rec) {
            return Err(WriteError::WouldBlock);
        }
        // SAFETY: rec <= capacity and the wrap was computed, so the record does
        // not straddle; we are the single writer.
        unsafe { self.commit(c, start_abs, gap, bytes) };
        Ok(())
    }

    /// Write one message, suspending until there is room.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        let rec = frame_len(bytes.len()) as u64;
        if rec > self.inner.capacity {
            return Err(WriteError::InsufficientCapacity);
        }
        loop {
            let c = self.inner.committed().load(Relaxed);
            let (start_abs, gap) = self.inner.reserve(c, rec);
            if self.fits(c, gap + rec) {
                // SAFETY: see `try_write`.
                unsafe { self.commit(c, start_abs, gap, bytes) };
                return Ok(());
            }
            // Wait for a reader to free enough bytes, then re-evaluate.
            let inner = self.inner.clone();
            self.space
                .wait_until(|| {
                    let c = inner.committed().load(Relaxed);
                    let (_, gap) = inner.reserve(c, rec);
                    let slowest = inner.slowest_active_cursor().unwrap_or(c);
                    c.wrapping_sub(slowest) + gap + rec <= inner.capacity
                })
                .await;
        }
    }

    /// Whether a write needing `need` bytes fits without overwriting the slowest
    /// active reader (the in-use check; mirrors the disruptor).
    #[inline]
    fn fits(&self, committed: u64, need: u64) -> bool {
        let slowest = self.inner.slowest_active_cursor().unwrap_or(committed);
        committed.wrapping_sub(slowest) + need <= self.inner.capacity
    }

    /// Write the record bytes, publish the wrap gap (if any) and the new
    /// `committed`, and wake waiting readers.
    ///
    /// # Safety
    /// `start_abs & mask` plus `frame_len(bytes.len())` does not exceed
    /// `capacity` (record is contiguous), we hold the single-writer role, and
    /// the in-use check passed (no reader cursor inside the reused bytes).
    unsafe fn commit(&self, committed: u64, start_abs: u64, gap: u64, bytes: &[u8]) {
        let end_abs = start_abs + frame_len(bytes.len()) as u64;
        let phys = (start_abs & self.inner.mask) as usize;
        // SAFETY: record is contiguous and in-bounds (caller contract).
        unsafe { self.inner.write_record(phys, bytes) };
        if gap > 0 {
            // Publish where valid data ends before the gap so readers skip it.
            self.inner.hwm().store(committed, Release);
        }
        // Release: hands the freshly written bytes (and the hwm store) to readers,
        // which Acquire-load `committed` before touching the ring.
        self.inner.committed().store(end_abs, Release);
        self.data.notify();
    }
}

impl<B: Backing, WD: WakeSource, WS: WakeSink> Drop for Writer<B, WD, WS> {
    fn drop(&mut self) {
        // Free the region's writer claim. Release: pairs with the Acquire CAS in
        // `RingBuffer::writer`, handing the whole region state (committed / hwm
        // and the data bytes) to the next claimer.
        self.inner.writer_claim().store(0, Release);
    }
}

// ---------------------------------------------------------------------------
// View (reader)
// ---------------------------------------------------------------------------

/// Where a readable record lives.
struct Located {
    /// Absolute cursor at the record start.
    r: u64,
    /// Physical offset of the record start.
    phys: usize,
    /// Payload length.
    len: usize,
    /// Total record bytes (header + padded payload).
    rec: usize,
}

/// A reader into a buffer, with its own absolute cursor stored in the shared
/// reader table. Drops free its slot.
pub struct View<B: Backing, RD: WakeSink, RS: WakeSource> {
    inner: Arc<Inner<B>>,
    slot: u32,
    data: RD,
    space: RS,
}

impl<B: Backing, RD: WakeSink, RS: WakeSource> View<B, RD, RS> {
    /// This view's current absolute read cursor.
    pub fn cursor(&self) -> u64 {
        self.inner.slot_cursor(self.slot).load(Acquire)
    }

    /// The buffer's absolute committed position.
    pub fn committed(&self) -> u64 {
        self.inner.committed().load(Acquire)
    }

    /// Copy the next record into `buf`. `Ok(true)` = a record was read,
    /// `Ok(false)` = caught up (nothing new). Prefer [`View::try_read`] — the
    /// writer can never overwrite an unread record, so borrowing is always
    /// tear-free; this copying form exists for callers that must own the bytes.
    pub fn try_read_into(&mut self, buf: &mut Vec<u8>) -> Result<bool, ReadError> {
        let Some(loc) = self.locate()? else {
            return Ok(false);
        };
        // SAFETY: `loc` came from `locate`, so the record is contiguous and
        // in-bounds (`rec <= capacity`), and published via `committed`.
        unsafe { self.inner.copy_payload(loc.phys, loc.len, buf) };
        self.advance(loc.r + loc.rec as u64);
        Ok(true)
    }

    /// Await and copy the next record (async consumers that must own the bytes).
    pub async fn read_into(&mut self, buf: &mut Vec<u8>) -> Result<(), ReadError> {
        loop {
            if self.try_read_into(buf)? {
                return Ok(());
            }
            let inner = self.inner.clone();
            let slot = self.slot;
            self.data
                .wait_until(|| {
                    inner.committed().load(Acquire) > inner.slot_cursor(slot).load(Acquire)
                })
                .await;
        }
    }

    /// Tear-free, zero-copy borrow of the next record. The writer cannot
    /// overwrite a record this view has not consumed (the in-use check), so the
    /// borrow is stable for its whole lifetime. The returned grant holds this
    /// view's cursor until dropped, at which point the cursor advances past the
    /// record and a waiting writer is woken.
    pub fn try_read(&mut self) -> Result<Option<ReadGrant<'_, B, RS>>, ReadError> {
        let Some(loc) = self.locate()? else {
            return Ok(None);
        };
        // SAFETY: `locate` ⇒ the record is in-bounds, and the writer will not
        // overwrite these bytes while the borrow lives (in-use check: the
        // cursor stays at or before `loc.r` until the grant drops).
        let slice = unsafe {
            let p = self.inner.data_ptr(loc.phys + 8);
            std::slice::from_raw_parts(p as *const u8, loc.len)
        };
        Ok(Some(ReadGrant {
            inner: &self.inner,
            space: &self.space,
            slot: self.slot,
            end_abs: loc.r + loc.rec as u64,
            slice,
        }))
    }

    /// Await the next record and borrow it (event-driven async consumers) — the
    /// grant twin of [`View::read_into`].
    pub async fn read(&mut self) -> Result<ReadGrant<'_, B, RS>, ReadError> {
        // Await-then-borrow is split from `try_read` for borrowck: a grant
        // taken inside the wait loop would pin the `self` borrow across every
        // iteration. Nothing else can consume between the successful `locate`
        // and the `try_read` — we hold `&mut self`.
        while self.locate()?.is_none() {
            let inner = self.inner.clone();
            let slot = self.slot;
            self.data
                .wait_until(|| {
                    inner.committed().load(Acquire) > inner.slot_cursor(slot).load(Acquire)
                })
                .await;
        }
        Ok(self.try_read()?.expect("record located above"))
    }

    /// Tear-free, zero-copy borrow of the **newest** committed record,
    /// consuming (freeing for the writer) every older unread record — the
    /// latest-wins read. The cursor parks at the returned record's *start*, so
    /// the record stays pinned (the writer cannot reclaim it) and a later call
    /// with no new data returns the same record again. `Ok(None)` only before
    /// the first record is committed (or after the stream was fully consumed
    /// through [`View::try_read`]/[`View::try_read_into`]).
    pub fn try_latest(&mut self) -> Result<Option<ReadGrant<'_, B, RS>>, ReadError> {
        loop {
            let Some(loc) = self.locate()? else {
                return Ok(None);
            };
            let end = loc.r + loc.rec as u64;
            // Newest iff nothing is committed past it (re-snapshotted each
            // pass; a racing commit just means one more skip iteration).
            if end >= self.inner.committed().load(Acquire) {
                // SAFETY: as in `try_read`; the cursor stays at `loc.r` (the
                // grant's drop re-stores it), so the record stays pinned.
                let slice = unsafe {
                    let p = self.inner.data_ptr(loc.phys + 8);
                    std::slice::from_raw_parts(p as *const u8, loc.len)
                };
                return Ok(Some(ReadGrant {
                    inner: &self.inner,
                    space: &self.space,
                    slot: self.slot,
                    // Park at the record start, not its end: the pin that makes
                    // the next `try_latest` re-serve this record.
                    end_abs: loc.r,
                    slice,
                }));
            }
            // An older record: consume it so the writer can reuse its bytes.
            self.advance(end);
        }
    }

    /// Find the next readable record, skipping a wrap gap if the cursor sits on
    /// one. Returns `Ok(None)` when caught up.
    fn locate(&self) -> Result<Option<Located>, ReadError> {
        let cap = self.inner.capacity;
        loop {
            let r = self.inner.slot_cursor(self.slot).load(Acquire);
            // Load order is load-bearing: `hwm` AFTER `committed`. Seeing a
            // post-wrap `committed` (Acquire, from the commit's Release store)
            // implies the wrap's earlier `hwm` store is visible — so a reader
            // whose next bytes are a gap always sees the marker before it could
            // misread stale gap bytes as a record header. (hwm-first would allow
            // fresh `committed` + stale `hwm`.)
            let c = self.inner.committed().load(Acquire);
            let hwm = self.inner.hwm().load(Acquire);
            if r == hwm {
                // Skip the wrap gap and resume at the next lap boundary (the
                // gap bytes were never data). The skip itself never accepts
                // data; the next iteration re-runs against a fresh `committed`.
                // A given `r` can match at most one gap ever (`hwm` values
                // strictly increase), so a stale `hwm` cannot re-trigger.
                let lap_end = (r & !self.inner.mask) + cap;
                self.inner.slot_cursor(self.slot).store(lap_end, Release);
                self.space.notify();
                continue;
            }
            // Caught-up check: the gap skip can transiently park the cursor
            // *ahead* of `committed` (the wrap's `hwm` publishes before its
            // `committed`); a reader at or ahead of the live edge has nothing
            // to read.
            if r >= c {
                return Ok(None); // caught up
            }
            let phys = (r & self.inner.mask) as usize;
            // SAFETY: phys is an 8-aligned, in-bounds record start (attach
            // geometry guarantees `capacity >= 8`, so `phys <= cap - 8`), and
            // `r < c` orders this read after the record's publication.
            let len = unsafe { self.inner.read_len(phys) };
            // Bound the length in u64 *before* any usize conversion or pointer
            // math: a corrupted region can carry arbitrary header bytes, and on
            // a 32-bit target `frame_len(garbage)` would wrap `usize` and
            // defeat this very check. `len > cap - 8 - phys` is the straddle
            // predicate `phys + rec > cap` rewritten overflow-free
            // (`cap - phys - 8` is a multiple of 8 and cannot underflow since
            // record starts are 8-aligned with `phys <= cap - 8`; for a
            // multiple of 8 `m`, `round_up8(len) > m <=> len > m`). A real
            // record never straddles the wrap, and the writer can never lap a
            // reader, so no in-crate behavior can explain it — the region is
            // structurally corrupt and must not be borrowed from.
            if len > cap - 8 - phys as u64 {
                return Err(ReadError::Corrupt);
            }
            // In-bounds now: len <= cap <= usize::MAX and phys + rec <= cap, so
            // every downstream `unsafe` (copy_payload / from_raw_parts)
            // inherits an in-bounds range.
            let len = len as usize;
            let rec = frame_len(len);
            return Ok(Some(Located { r, phys, len, rec }));
        }
    }

    /// Advance the cursor (Release) and wake a waiting writer.
    #[inline]
    fn advance(&self, end_abs: u64) {
        self.inner.slot_cursor(self.slot).store(end_abs, Release);
        self.space.notify();
    }
}

impl<B: Backing, RD: WakeSink, RS: WakeSource> Drop for View<B, RD, RS> {
    fn drop(&mut self) {
        // Free this view's slot for reuse (a single wait-free store).
        self.inner.slot_cursor(self.slot).store(FREE_SLOT, Release);
    }
}

/// A borrowed, tear-free view of one record. Derefs to the payload bytes; on
/// drop, moves the view's cursor to `end_abs` — past the record for a
/// [`View::try_read`]/[`View::read`] grant (consume), back to its start for a
/// [`View::try_latest`] grant (pin) — and wakes a waiting writer.
pub struct ReadGrant<'a, B: Backing, RS: WakeSource> {
    inner: &'a Inner<B>,
    space: &'a RS,
    slot: u32,
    end_abs: u64,
    slice: &'a [u8],
}

impl<B: Backing, RS: WakeSource> std::ops::Deref for ReadGrant<'_, B, RS> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.slice
    }
}

impl<B: Backing, RS: WakeSource> Drop for ReadGrant<'_, B, RS> {
    fn drop(&mut self) {
        self.inner.slot_cursor(self.slot).store(self.end_abs, Release);
        self.space.notify();
    }
}

#[cfg(test)]
mod tests;
