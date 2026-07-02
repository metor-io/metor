//! Overwrite-on-lap, shared-memory ring buffer for `metor-fsw-2`.
//!
//! This is the transport over which fsw-2 systems exchange data. It generalizes
//! the `metor-db` disruptor (`libs/db/src/disruptor.rs`) along two axes:
//!
//! 1. **Writer-chosen overrun policy.** A buffer is created in one of two modes
//!    (recorded in its header, [`Overrun`]):
//!    - [`Overrun::Overwrite`] (default): the writer never blocks; when it would
//!      reuse bytes a slow reader has not consumed it overwrites them, and the
//!      reader detects this via [`View::is_lapped`] / [`ReadError::Lapped`].
//!    - [`Overrun::Lossless`]: the writer honors the disruptor's in-use check —
//!      [`Writer::try_write`] returns [`WriteError::WouldBlock`] and the async
//!      [`Writer::write`] suspends until a reader frees space. It never laps a
//!      reader, so reads may borrow tear-free ([`View::try_read`]).
//! 2. **Shared memory.** The entire stateful buffer lives in one contiguous
//!    region addressed by fixed byte offsets — no `Box`/`Arc`/process-local
//!    pointers inside it — so the same mechanism works in-process and (with the
//!    `mmap` feature) across processes.
//!
//! The unsafe core borrows the disruptor's proven techniques: absolute monotonic
//! `u64` cursors with `phys = abs & mask`, a `committed` Release/Acquire
//! publication handshake, and a `high_water_mark` wrap gap. On top of those,
//! overwrite-mode reads are guarded by a seqlock-style write reservation
//! (`reserved_end`, see `Writer::commit`), the single-writer rule is enforced by
//! an in-region claim word ([`RingBuffer::writer`]), and lossless registration
//! runs a `SeqCst` handshake against the writer's cursor scan (see
//! [`RingBuffer::view`]). See `libs/metor-fsw-2/docs/ring-buffer.md` for the
//! full design and `libs/db/MIRI.md` for the Miri strategy this crate follows.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{
    AtomicU8, AtomicU64, fence,
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
/// Pre-1.0 note: the layout changed under version 1 without a bump — `reserved_end`
/// (0x50) went live as the seqlock write reservation and the writer-claim word
/// (0x60) was added. Regions are ephemeral IPC state, not archives; stale dev
/// regions are simply recreated. Post-1.0, any such change bumps this.
const VERSION: u16 = 1;

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
const OFF_RESERVED_END: usize = 0x50; // AtomicU64: seqlock write reservation (see `Writer::commit`)
const OFF_WAKE_WORD: usize = 0x58; // AtomicU64 (reserved; future cross-proc wake)
const OFF_WRITER: usize = 0x60; // AtomicU64: writer claim, 0 = free (see `RingBuffer::writer`)

/// Header + control block size; the reader table starts here.
const HEADER_SIZE: usize = 0x80;
/// One reader-table slot, padded to a cache line to avoid false sharing.
const READER_SLOT_SIZE: usize = 64;
const SLOT_OFF_CURSOR: usize = 0x00; // AtomicU64
const SLOT_OFF_EPOCH: usize = 0x08; // AtomicU64

/// `flags` bit 0: a shared-memory wake word is present (reserved; never set in
/// v1, but the bit and [`OFF_WAKE_WORD`] reserve room for cross-process wake).
#[allow(dead_code)]
const FLAG_WAKE_SHARED: u16 = 1 << 0;
/// `flags` bit 1: the buffer is in [`Overrun::Lossless`] mode.
const FLAG_LOSSLESS: u16 = 1 << 1;

/// Sentinel `cursor` value marking a reader slot as free. A real absolute byte
/// cursor can never reach this (16 EiB committed).
const FREE_SLOT: u64 = u64::MAX;
/// `high_water_mark` value meaning "no pending wrap gap".
const HWM_NONE: u64 = u64::MAX;

/// A tag identifying the architecture that wrote a region. Comparing it on
/// attach rejects regions produced by a different pointer width or endianness:
/// the byte pattern of this native `u64` differs across endianness, and the low
/// word carries the pointer width.
fn arch_tag() -> u64 {
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
// Public modes / config / errors
// ---------------------------------------------------------------------------

/// Overrun policy, fixed at creation and recorded in the header. It is the
/// read-soundness contract every view relies on, so a buffer cannot mix modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overrun {
    /// Writer never blocks; overwrites slow readers. Reads copy + lap-recheck.
    Overwrite,
    /// Writer honors the in-use check (error or wait). Reads may borrow.
    Lossless,
}

/// Buffer geometry.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Data-region size in bytes. **Must be a power of two** (so `% cap` is a
    /// mask) and is therefore also a multiple of 8 (record alignment).
    pub capacity: usize,
    /// Number of reader-table slots. Over-provision: v1 has no crash-slot
    /// reclamation (see the design doc).
    pub max_readers: usize,
    /// Overrun policy.
    pub overrun: Overrun,
}

/// A write could not be performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// The single message is larger than the whole data region.
    InsufficientCapacity,
    /// Lossless mode: writing now would overwrite the slowest active reader.
    WouldBlock,
}

/// A read could not be performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    /// The writer lapped this view; its data was overwritten. Stop reading
    /// (or [`View::resync`] to skip to the live edge).
    Lapped,
    /// A tear-free borrow ([`View::try_read`]) was requested on an overwrite
    /// buffer, where the writer can race the reader. Use [`View::try_read_into`].
    BorrowNotSupported,
    /// The region violated a structural invariant (a record's length field says
    /// it straddles the wrap or overruns the data region) on a lossless buffer,
    /// where no lap can explain it: possible external corruption — stop reading.
    /// Unreachable from this crate's own behavior; it exists so a corrupted
    /// shared mapping degrades to an error instead of an out-of-bounds borrow.
    Corrupt,
}

/// The reader table is full; no free slot to register another view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullReaderTable;

/// A writer already exists for this buffer (or a crashed process leaked its
/// claim — see [`RingBuffer::force_release_writer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterClaimed;

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
/// `&AtomicU64`/`&AtomicU8` references and plain byte slices over this region.
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
    overrun: Overrun,
}

// SAFETY: every access to the shared region goes through atomics or the
// `committed` Release/Acquire handshake. In overwrite mode the data bytes are
// touched only via relaxed atomics (so a writer/reader overlap is a defined
// atomic race, resolved by the lap recheck, not UB); in lossless mode the
// in-use check gives the writer-then-reader happens-before that makes plain
// accesses race-free, exactly as in the db disruptor. `B: Send + Sync`.
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

    /// The seqlock write reservation: the absolute end of the record the writer
    /// has *started* writing (stored before any data byte; `committed <=
    /// reserved_end` at every instant). See `Writer::commit`.
    #[inline]
    fn reserved_end(&self) -> &AtomicU64 {
        // SAFETY: OFF_RESERVED_END is a fixed, 8-aligned, in-bounds control word.
        unsafe { self.atomic_u64(OFF_RESERVED_END) }
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
    /// active readers. Used by the lossless in-use check (mirrors the
    /// disruptor's `slowest_cursor`).
    fn slowest_active_cursor(&self) -> Option<u64> {
        // SeqCst: pairs with the registration fence in `view()` (only lossless
        // paths call this scan). This is Dekker's pattern on two locations —
        // reader: `store(cursor); load(committed)`, writer: `store(committed);
        // load(cursor)` — where Release/Acquire alone allows *both* loads to
        // read the older values (StoreLoad reordering). The fence sits between
        // this writer's previous `committed` store and this scan, so for every
        // write W: either W's scan observes a new reader's claim, or that
        // reader's registration recheck observes committed_{W-1} (see `view()`).
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

    /// Write a record's header + payload at physical offset `phys`.
    ///
    /// # Safety
    /// `phys + frame_len(payload.len()) <= capacity` (record does not straddle
    /// the wrap), and the caller holds the single-writer role.
    unsafe fn write_record(&self, phys: usize, payload: &[u8]) {
        // SAFETY: phys is 8-aligned and in-bounds (caller contract).
        let p = unsafe { self.data_ptr(phys) };
        let len = payload.len() as u64; // low 32 bits = length, high = pad.
        match self.overrun {
            Overrun::Overwrite => {
                // Overwrite mode races readers, so every store is a relaxed
                // atomic. The 8-byte header goes as one `AtomicU64`; the payload
                // byte-by-byte as `AtomicU8`. The later `committed` Release
                // orders all of these before publication.
                // SAFETY: p is 8-aligned, header fits in [phys, phys+8).
                unsafe { (*(p as *const AtomicU64)).store(len, Relaxed) };
                for (i, b) in payload.iter().enumerate() {
                    // SAFETY: phys+8+i < capacity by the caller's contract.
                    unsafe { (*(p.add(8 + i) as *const AtomicU8)).store(*b, Relaxed) };
                }
            }
            Overrun::Lossless => {
                // Lossless mode never overwrites in-flight bytes, so plain stores
                // are race-free (publication via `committed`, exclusivity via the
                // in-use check) — exactly the disruptor's write path.
                // SAFETY: p is 8-aligned; [phys, phys+frame_len) is in-bounds.
                unsafe {
                    (p as *mut u64).write(len);
                    std::ptr::copy_nonoverlapping(payload.as_ptr(), p.add(8), payload.len());
                }
            }
        }
    }

    /// Read a record's length field at physical offset `phys`. Returns the raw
    /// `u32` widened to `u64`: under a lap race it can be arbitrary payload
    /// bytes, so the caller must validate it against the data region *before*
    /// any `usize` conversion or pointer math (on a 32-bit target,
    /// `frame_len(garbage)` would wrap `usize` and defeat the straddle check).
    ///
    /// # Safety
    /// `phys + 8 <= capacity`.
    unsafe fn read_len(&self, phys: usize) -> u64 {
        // SAFETY: phys is 8-aligned and in-bounds (caller contract).
        let p = unsafe { self.data_ptr(phys) };
        let hdr = match self.overrun {
            // SAFETY: header occupies [phys, phys+8); atomic load matches the
            // writer's atomic store.
            Overrun::Overwrite => unsafe { (*(p as *const AtomicU64)).load(Relaxed) },
            // SAFETY: lossless reads are ordered after the write via `committed`.
            Overrun::Lossless => unsafe { (p as *const u64).read() },
        };
        hdr & 0xFFFF_FFFF
    }

    /// Copy `len` payload bytes starting at `phys + 8` into `dst`.
    ///
    /// # Safety
    /// `phys + 8 + len <= capacity`.
    unsafe fn copy_payload(&self, phys: usize, len: usize, dst: &mut Vec<u8>) {
        dst.clear();
        // SAFETY: payload occupies [phys+8, phys+8+len) (caller contract).
        let p = unsafe { self.data_ptr(phys + 8) };
        match self.overrun {
            Overrun::Overwrite => {
                dst.reserve(len);
                for i in 0..len {
                    // SAFETY: phys+8+i < capacity; atomic load matches writer.
                    let b = unsafe { (*(p.add(i) as *const AtomicU8)).load(Relaxed) };
                    dst.push(b);
                }
            }
            Overrun::Lossless => {
                dst.resize(len, 0);
                // SAFETY: ordered after the write via `committed`; non-overlapping.
                unsafe { std::ptr::copy_nonoverlapping(p, dst.as_mut_ptr(), len) };
            }
        }
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
                overrun: cfg.overrun,
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
                overrun: cfg.overrun,
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
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}")))
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
    /// read the geometry (capacity/offsets/`max_readers`/overrun) back out of it
    /// rather than from arguments. Shared by [`RingBuffer::attach_mmap`] and
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
                overrun: geo.overrun,
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

    /// This buffer's overrun policy.
    pub fn overrun(&self) -> Overrun {
        self.inner.overrun
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
    /// by the async lossless [`Writer::write`] until a reader frees room. Use
    /// [`NoWake`] for both when only the synchronous APIs are used.
    pub fn writer<WD: WakeSource, WS: WakeSink>(
        &self,
        data: WD,
        space: WS,
    ) -> Result<Writer<B, WD, WS>, WriterClaimed> {
        // Acquire on success: pairs with the Release store in `Writer::drop` /
        // `force_release_writer`, so drop→claim forms a synchronizes-with edge
        // handing the whole region state (committed/hwm/reserved_end and data
        // bytes) from the previous writer to this one. Relaxed on failure: no
        // state is read when the claim is lost.
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
    /// is notified whenever the view advances (waking a lossless waiting writer).
    /// A fresh view only sees data committed from now on.
    pub fn view<RD: WakeSink, RS: WakeSource>(
        &self,
        data: RD,
        space: RS,
    ) -> Result<View<B, RD, RS>, FullReaderTable> {
        // A new reader starts at the current commit point (mirrors
        // `Disruptor::reader`): it never sees older, possibly-lapped data.
        let mut start = self.inner.committed().load(Acquire);
        for slot in 0..self.inner.max_readers {
            // Claim ordering: AcqRel — Acquire pairs with `View::drop`'s Release
            // store of FREE_SLOT (slot-state handoff between successive owners);
            // Release publishes the claim store itself. NOTE: visibility of this
            // claim to the *lossless writer's* `fits()` scan is guaranteed by the
            // SeqCst registration handshake below, not by this CAS. The epoch
            // word is a generation counter reserved for crash reclamation; it is
            // written (Release) but never yet read. If reclamation is ever
            // implemented, the reclaimer must bump the epoch *before* freeing
            // the cursor, and every cursor store made through a View handle must
            // be preceded by an epoch check (or become a CAS on (epoch, cursor))
            // — a plain Release store on a reclaimed slot would corrupt the new
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
                if self.inner.overrun == Overrun::Lossless {
                    // Registration handshake: loop until the claim is provably
                    // stable. Until the writer's `fits()` scan observes the
                    // claim, its in-use check is vacuous and it could lap past
                    // `start + capacity` — after which the lossless borrow path
                    // would hand out overwritten bytes. The SeqCst fence pairs
                    // with the one in `slowest_active_cursor()`: in the fence
                    // total order, either the writer's scan is later and must
                    // observe our cursor store, or our recheck is later and must
                    // observe that writer's `committed` — requiring a *stable*
                    // `committed` therefore proves every unseen write was bounded
                    // by some other cursor <= ours. Converges in 1-2 iterations
                    // (each extra one needs a commit inside a ~3-instruction
                    // window); registration is a cold path, so no iteration
                    // bound is imposed. Overwrite mode skips this: its reads
                    // self-validate via the seqlock recheck.
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
    let mut flags = 0u16;
    if cfg.overrun == Overrun::Lossless {
        flags |= FLAG_LOSSLESS;
    }
    // SAFETY: all offsets are within `total` bytes and correctly aligned; no
    // other thread can observe these writes yet.
    unsafe {
        (base.add(OFF_MAGIC) as *mut u32).write(MAGIC);
        (base.add(OFF_VERSION) as *mut u16).write(VERSION);
        (base.add(OFF_FLAGS) as *mut u16).write(flags);
        (base.add(OFF_CAPACITY) as *mut u64).write(cfg.capacity as u64);
        (base.add(OFF_DATA_OFFSET) as *mut u64).write(data_offset as u64);
        (base.add(OFF_MAX_READERS) as *mut u32).write(cfg.max_readers as u32);
        (base.add(OFF_READER_TABLE_OFFSET) as *mut u32).write(reader_table_offset as u32);
        (base.add(OFF_TOTAL_SIZE) as *mut u64).write(total as u64);
        (base.add(OFF_ARCH_TAG) as *mut u64).write(arch_tag());
        (base.add(OFF_COMMITTED) as *mut u64).write(0);
        (base.add(OFF_HWM) as *mut u64).write(HWM_NONE);
        (base.add(OFF_RESERVED_END) as *mut u64).write(0);
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
    overrun: Overrun,
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
        let flags = (base.add(OFF_FLAGS) as *const u16).read();

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

        let overrun = if flags & FLAG_LOSSLESS != 0 {
            Overrun::Lossless
        } else {
            Overrun::Overwrite
        };
        Ok(Geometry {
            capacity,
            // Both fit usize: they are <= total_size <= region_len: usize.
            data_offset: data_offset as usize,
            reader_table_offset: reader_table_offset as usize,
            max_readers,
            overrun,
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
    /// Write one message without blocking.
    ///
    /// - Overwrite mode: always succeeds (overwriting slow readers) unless the
    ///   message exceeds capacity.
    /// - Lossless mode: returns [`WriteError::WouldBlock`] if writing now would
    ///   overwrite the slowest active reader.
    pub fn try_write(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        let rec = frame_len(bytes.len()) as u64;
        if rec > self.inner.capacity {
            return Err(WriteError::InsufficientCapacity);
        }
        let c = self.inner.committed().load(Relaxed); // sole writer
        let (start_abs, gap) = self.inner.reserve(c, rec);
        if self.inner.overrun == Overrun::Lossless && !self.fits(c, gap + rec) {
            return Err(WriteError::WouldBlock);
        }
        // SAFETY: rec <= capacity and the wrap was computed, so the record does
        // not straddle; we are the single writer.
        unsafe { self.commit(c, start_abs, gap, bytes) };
        Ok(())
    }

    /// Write one message, suspending (lossless mode) until there is room. In
    /// overwrite mode this never actually suspends.
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        let rec = frame_len(bytes.len()) as u64;
        if rec > self.inner.capacity {
            return Err(WriteError::InsufficientCapacity);
        }
        loop {
            let c = self.inner.committed().load(Relaxed);
            let (start_abs, gap) = self.inner.reserve(c, rec);
            if self.inner.overrun == Overrun::Overwrite || self.fits(c, gap + rec) {
                // SAFETY: see `try_write`.
                unsafe { self.commit(c, start_abs, gap, bytes) };
                return Ok(());
            }
            // Wait for a reader to free `need` bytes, then re-evaluate.
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
    /// active reader (lossless in-use check; mirrors the disruptor).
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
    /// `capacity` (record is contiguous), and we hold the single-writer role.
    unsafe fn commit(&self, committed: u64, start_abs: u64, gap: u64, bytes: &[u8]) {
        let end_abs = start_abs + frame_len(bytes.len()) as u64;
        // Seqlock begin: reserve `[start_abs, end_abs)` before touching data
        // (unconditionally, both modes, so `committed <= reserved_end` is a
        // uniform invariant). `start_abs` already includes the wrap-gap skip, so
        // the reservation covers exactly the bytes `write_record` stores.
        //
        // Ordering: Relaxed store + Release *fence* — a Release store on
        // `reserved_end` would only order *prior* accesses before itself and
        // would not stop the subsequent relaxed data stores from becoming
        // visible first, reopening the torn-read window. The fence-to-fence
        // seqlock is the correct shape: if a reader's relaxed payload load reads
        // a value stored after this fence, this fence synchronizes-with the
        // reader's Acquire fence (C++ [atomics.fences]), making the reservation
        // visible to its recheck (see `try_read_into`).
        self.inner.reserved_end().store(end_abs, Relaxed);
        fence(Release);
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
        // `RingBuffer::writer`, handing the whole region state (committed / hwm /
        // reserved_end and the data bytes) to the next claimer.
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

    /// Whether the writer has lapped this view (its next bytes were overwritten).
    /// Always `false` on a lossless buffer. This is the check the coordinator
    /// runs before invoking a cyclic system.
    pub fn is_lapped(&self) -> bool {
        if self.inner.overrun == Overrun::Lossless {
            return false;
        }
        let r = self.inner.slot_cursor(self.slot).load(Acquire);
        // Load order is load-bearing (same chain as `locate`): `hwm` after
        // `committed` — seeing a post-wrap `committed` implies seeing its `hwm`
        // store — and `reserved_end` after `hwm` — the commit's reservation
        // store is sequenced before its `hwm` Release store, so a seen gap
        // marker implies `reserved_end >= lap_end` and the subtraction below
        // cannot spuriously wrap.
        let c = self.inner.committed().load(Acquire);
        let hwm = self.inner.hwm().load(Acquire);
        // A cursor parked exactly on a wrap gap effectively sits at the next lap
        // boundary (the gap bytes were never data) — evaluate the lap there, or
        // a reader at a gap start would be declared lapped while its next real
        // record is intact (see `locate`'s gap skip).
        let r_eff = if r == hwm {
            (r & !self.inner.mask) + self.inner.capacity
        } else {
            r
        };
        // Caught up (or transiently parked ahead of `committed` by the gap
        // skip): no unread bytes exist, so nothing can have been overwritten.
        // This also filters the wrap-in-flight artifact where a stale `hwm`
        // read leaves `r_eff = r` while the writer's reservation is already
        // visible past the gap.
        if r_eff >= c {
            return false;
        }
        // Compare against `reserved_end`, not `committed`: an *in-flight* write
        // that has reserved past `r_eff + capacity` is already scribbling this
        // reader's next bytes, and the read path's seqlock recheck would reject
        // the read anyway — one lap definition crate-wide.
        let re = self.inner.reserved_end().load(Acquire);
        re.wrapping_sub(r_eff) > self.inner.capacity
    }

    /// Skip to the live edge, abandoning any unread (possibly lapped) data. Use
    /// after [`ReadError::Lapped`] to resume from current data.
    pub fn resync(&self) {
        let c = self.inner.committed().load(Acquire);
        self.inner.slot_cursor(self.slot).store(c, Release);
    }

    /// Copy the next record into `buf`. `Ok(true)` = a record was read,
    /// `Ok(false)` = caught up (nothing new), `Err(Lapped)` = overwritten.
    ///
    /// Safe on both kinds of buffer: overwrite buffers copy via relaxed atomics
    /// and re-validate the lap condition (whole-buffer recheck); lossless buffers
    /// copy plainly (the recheck never fires).
    pub fn try_read_into(&mut self, buf: &mut Vec<u8>) -> Result<bool, ReadError> {
        let Some(loc) = self.locate()? else {
            return Ok(false);
        };
        // SAFETY: `loc` came from `locate`, so the record is contiguous and
        // in-bounds (`rec <= capacity`).
        unsafe { self.inner.copy_payload(loc.phys, loc.len, buf) };
        if self.inner.overrun == Overrun::Overwrite {
            // Seqlock validate: any write that touched a byte of this record sits
            // at absolute `p >= r + capacity` and reserved `reserved_end > p`
            // *before* storing that byte (behind its Release fence). If any of
            // our relaxed payload loads saw such a store, this Acquire fence
            // pairs with that Release fence, so the recheck load must observe
            // `reserved_end > r + capacity` -> `Lapped`. Contrapositive: recheck
            // passes => no copied byte came from an overlapping write. Checking
            // `committed` here is NOT enough: the writer scribbles data bytes
            // before its `committed` Release store, so a writer exactly one lap
            // ahead can tear the copy while `committed` still passes the check.
            // (Tear-old is excluded by `locate`'s Acquire on `committed`.)
            fence(Acquire);
            // Relaxed: the fence above carries the ordering. `reserved_end >=
            // committed >= r + rec > r` for any record `locate` returns, so the
            // subtraction never wraps in a passing case.
            let re = self.inner.reserved_end().load(Relaxed);
            if re.wrapping_sub(loc.r) > self.inner.capacity {
                return Err(ReadError::Lapped);
            }
        }
        self.advance(loc.r + loc.rec as u64);
        Ok(true)
    }

    /// Await and copy the next record (async systems). Propagates
    /// [`ReadError::Lapped`].
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

    /// Tear-free, zero-copy borrow of the next record. Available only on a
    /// lossless buffer (where the writer cannot overwrite a borrowed record);
    /// returns [`ReadError::BorrowNotSupported`] on an overwrite buffer. The
    /// returned grant holds this view's cursor until dropped, at which point the
    /// cursor advances and a waiting writer is woken.
    pub fn try_read(&mut self) -> Result<Option<ReadGrant<'_, B, RS>>, ReadError> {
        if self.inner.overrun != Overrun::Lossless {
            return Err(ReadError::BorrowNotSupported);
        }
        let Some(loc) = self.locate()? else {
            return Ok(None);
        };
        // SAFETY: lossless mode + `locate` ⇒ the writer will not overwrite these
        // bytes while the borrow lives (in-use check), and they are in-bounds.
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

    /// Find the next readable record, skipping a wrap gap if the cursor sits on
    /// one. Returns `Ok(None)` when caught up, `Err(Lapped)` (overwrite mode) if
    /// lapped.
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
                // Skip the wrap gap and resume at the next lap boundary — and do
                // it *before* the lap test: a cursor parked exactly on a gap
                // start effectively sits at `lap_end` (the gap bytes were never
                // data), so testing the lap at `r` would spuriously hard-stop a
                // reader whose next real record is intact. The skip itself never
                // accepts data; the next iteration re-runs the lap test against
                // `lap_end` with a fresh `committed`, so a real lap is still
                // caught. A given `r` can match at most one gap ever (`hwm`
                // values strictly increase), so a stale `hwm` cannot re-trigger.
                let lap_end = (r & !self.inner.mask) + cap;
                self.inner.slot_cursor(self.slot).store(lap_end, Release);
                self.space.notify();
                continue;
            }
            // Caught-up before the lap test: the gap skip can transiently park
            // the cursor *ahead* of `committed` (the wrap's `hwm` publishes
            // before its `committed`), and `c - r` would wrap to a huge value.
            // A reader at or ahead of the live edge has no unread bytes to lose.
            if r >= c {
                return Ok(None); // caught up
            }
            if self.inner.overrun == Overrun::Overwrite && c.wrapping_sub(r) > cap {
                return Err(ReadError::Lapped);
            }
            let phys = (r & self.inner.mask) as usize;
            // SAFETY: phys is an 8-aligned, in-bounds record start (attach
            // geometry guarantees `capacity >= 8`, so `phys <= cap - 8`).
            let len = unsafe { self.inner.read_len(phys) };
            // Bound the length in u64 *before* any usize conversion or pointer
            // math: under a lap race the header word can be arbitrary payload
            // bytes, and on a 32-bit target `frame_len(garbage)` would wrap
            // `usize` and defeat this very check. `len > cap - 8 - phys` is the
            // straddle predicate `phys + rec > cap` rewritten overflow-free
            // (`cap - phys - 8` is a multiple of 8 and cannot underflow since
            // record starts are 8-aligned with `phys <= cap - 8`; for a multiple
            // of 8 `m`, `round_up8(len) > m <=> len > m`). Validated in *both*
            // modes: a real record never straddles the wrap, so on overwrite an
            // apparent straddle means a lapping writer was rewriting the header
            // (`Lapped`); on lossless no lap can explain it — the region is
            // structurally corrupt (`Corrupt`) and must not be borrowed from.
            if len > cap - 8 - phys as u64 {
                return Err(match self.inner.overrun {
                    Overrun::Overwrite => ReadError::Lapped,
                    Overrun::Lossless => ReadError::Corrupt,
                });
            }
            // In-bounds now: len <= cap <= usize::MAX and phys + rec <= cap, so
            // every downstream `unsafe` (copy_payload / from_raw_parts) inherits
            // an in-bounds range even if the length was concurrently scribbled
            // (acceptance is then still gated by the seqlock recheck).
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

/// A borrowed, tear-free view of one record on a lossless buffer. Derefs to the
/// payload bytes; on drop, advances the cursor past this record.
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
