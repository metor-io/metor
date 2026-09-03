//! A single-writer, multi-reader ring buffer that lives in one shared memory
//! region.
//!
//! One task or process holds the [`Writer`] while any number of [`View`]s
//! consume the stream, each at its own pace. A write that would overwrite
//! data a reader has not consumed yet fails with
//! [`WouldBlock`](WriteError::WouldBlock) until the slowest reader frees
//! space. Since a record can never be
//! overwritten while a reader still wants it, reads can borrow directly from
//! the region ([`View::try_read`], [`View::try_latest`]) without copying and
//! without having to check afterwards that the bytes held still.
//!
//! # Example
//!
//! ```
//! use metor_fsw_ring::{Config, NoWake, RingBuffer};
//!
//! let ring = RingBuffer::create_in_memory(Config { capacity: 1024, max_readers: 4 });
//! let mut writer = ring.writer(NoWake)?;
//! let mut view = ring.view(NoWake)?;
//!
//! writer.try_write(b"hello")?;
//!
//! let grant = view.try_read()?.expect("a record was committed");
//! assert_eq!(&grant[..], b"hello");
//! drop(grant); // consuming the grant frees the bytes for the writer
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Region layout
//!
//! All shared state lives in one contiguous region that holds offsets and
//! atomics but no pointers, so the same code runs over a heap allocation, a
//! memory-mapped file, or any region the caller provides
//! ([`RingBuffer::attach_raw`]).
//!
//! ```text
//! 0x00 ┌───────────────────────────┐
//!      │ RegionHeader (immutable)  │ magic, version, geometry, arch tag
//! 0x40 ├───────────────────────────┤
//!      │ Control (all atomics)     │ committed, high-water mark, writer claim
//! 0x80 ├───────────────────────────┤
//!      │ ReaderSlot × max_readers  │ one cache line each: cursor + owner
//!      ├───────────────────────────┤
//!      │ data (capacity bytes)     │ the records
//!      └───────────────────────────┘
//! ```
//!
//! The `#[repr(C)]` declarations below are the region format; reordering a
//! field changes it. Attaching validates the magic, version, architecture
//! tag, and geometry before it forms a single reference into the region.
//! Fields are stored native-endian, and the architecture tag rejects regions
//! written by a different endianness or pointer width.
//!
//! # Cursors and records
//!
//! Positions are absolute byte counts that only grow. The capacity is a power
//! of two, so the physical offset of a position is `abs & (capacity - 1)`. A
//! record is an 8-byte header holding the `u32` payload length, followed by
//! the payload padded out to an 8-byte boundary ([`frame_len`]). Every record
//! therefore starts 8-aligned.
//!
//! Records never straddle the wrap. When one would, the writer moves it to
//! the start of the next lap and publishes where valid data ends in the
//! `high_water_mark`. A reader whose cursor lands on that mark skips the wrap
//! gap and continues at the lap boundary.
//!
//! Publication is the usual single-writer handshake. The writer fills the
//! record with plain stores and then Release-stores the new `committed`, and
//! readers Acquire-load `committed` before touching record bytes. Everything
//! below `committed` stays immutable until every reader cursor has moved past
//! it, which is what makes the borrowing reads sound.
//!
//! # Backpressure and reader registration
//!
//! Before each write the writer scans the reader table for the slowest active
//! cursor and refuses to advance more than `capacity` bytes past it. Reader
//! registration is the one racy edge. A writer could scan the table an
//! instant before a new reader's claim lands, validate a write against
//! nobody, and overwrite bytes the reader believes are pinned.
//! Release/Acquire alone cannot close this hole. It is Dekker's pattern (the
//! reader stores its cursor and loads `committed`, the writer stores
//! `committed` and loads cursors), and StoreLoad reordering lets both sides
//! read the old value. Both sides therefore fence with `SeqCst`.
//! [`RingBuffer::view`] claims a slot, fences, and re-reads `committed` until
//! it holds still, and the writer fences between publishing `committed` and
//! scanning cursors. In the total order of those fences one side must observe
//! the other, so a stable `committed` proves that any write the scan might
//! have missed was validated against some other cursor at or behind the new
//! view's.
//!
//! # Waking
//!
//! The shared region carries no wake state. Waking is a pair of traits
//! injected per handle, [`WakeSource`] to notify and [`WakeSink`] to await.
//! [`NoWake`] serves synchronous consumers, and [`Notifier`] wakes tasks
//! within one process. Only the data direction wakes: writers use the
//! non-blocking [`Writer::try_write`], so nothing ever waits for space.
//!
//! # Verification
//!
//! Three passes, each covering what the others cannot, documented in the crate
//! root. `MIRI.md`: the synchronous tests under Miri, for provenance, leaks and
//! data races over real allocations. `KANI.md`: the position arithmetic and the
//! geometry validation every `unsafe` deref here cites, proved over symbolic
//! inputs rather than sampled. `LOOM.md`: the atomic orderings below, including
//! the registration handshake, under every interleaving.

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod wake;

mod sync;

use crate::sync::Ordering::{AcqRel, Acquire, Relaxed, Release, SeqCst};
use crate::sync::{Arc, AtomicU64, fence};
use std::cell::UnsafeCell;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// ---------------------------------------------------------------------------
// Region layout
// ---------------------------------------------------------------------------

/// Region magic, the bytes `b"MFR1"` read as a native `u32`.
const MAGIC: u32 = u32::from_ne_bytes(*b"MFR1");

/// Layout version, bumped on any incompatible change. Regions are ephemeral
/// IPC state rather than archives, so a stale region is recreated, never
/// migrated. v3 added the reader-slot `owner` tag and made the writer claim
/// carry the claimant's process id. v4 dropped the reserved cross-process wake
/// word from the control block. v5 dropped the reader-slot `epoch` word.
const VERSION: u16 = 5;

/// The identifying metadata at the start of every region, occupying cache
/// line 0. `init_region` writes it once before the region is published and
/// nothing mutates it afterwards, which is what makes the transient shared
/// byte view in `read_header` sound.
///
/// The `IntoBytes` derive doubles as a compile-time proof that the struct has
/// no padding. The `align(8)` matters on 32-bit targets like i686, where
/// `u64` alone is only 4-aligned.
#[derive(FromBytes, IntoBytes, Immutable, KnownLayout)]
#[repr(C, align(8))]
struct RegionHeader {
    magic: u32,
    version: u16,
    flags: u16,
    capacity: u64,
    data_offset: u64,
    max_readers: u32,
    reader_table_offset: u32,
    total_size: u64,
    arch_tag: u64,
}

/// The words the writer and readers coordinate through, occupying cache
/// line 1. Every field is an atomic, so a shared `&Control` over the live
/// region is sound. It is never parsed from bytes, only pointer-cast, with
/// `#[repr(C)]` fixing the field offsets.
#[repr(C)]
struct Control {
    committed: AtomicU64,
    hwm: AtomicU64,
    /// The writer claim: `0` means free, nonzero is the claimant's process-id
    /// tag (see [`RingBuffer::writer`] and [`RingBuffer::reclaim_owner`]).
    writer: AtomicU64,
}

/// One reader's shared cursor and owner tag, padded out to a cache line to
/// avoid false sharing.
///
/// `owner` is the claiming process's id, stamped by [`RingBuffer::view`] so
/// [`RingBuffer::reclaim_owner`] can free what a dead process left behind. It
/// is diagnostic state, not synchronization: only read under the reclaim
/// contract (the owner is dead), and left stale on a free slot.
///
/// `_pad` is not interior-mutable, so a shared `&ReaderSlot` freezes it. That
/// is sound only because the pad is written exactly once, in `init_region`,
/// before the region is published. Never stash state there.
#[repr(C)]
struct ReaderSlot {
    cursor: AtomicU64,
    owner: AtomicU64,
    _pad: [u8; READER_SLOT_PAD],
}

/// Bytes of padding that round a [`ReaderSlot`] out to a cache line. Zero
/// under loom, whose atomics carry tracking state and blow past 64 bytes on
/// their own.
#[cfg(not(ring_loom))]
const READER_SLOT_PAD: usize = 48;
#[cfg(ring_loom)]
const READER_SLOT_PAD: usize = 0;

/// Byte offset of the control block. Cache line 1, after the header.
const OFF_CONTROL: usize = 0x40;

/// Header plus control block size. The reader table starts here.
///
/// Fixed at two cache lines for the real layout, since that is the region
/// format. Under loom the control block outgrows one line, so the constant is
/// derived instead and the reader table simply starts further in; nothing but
/// the loom models ever observes the difference.
#[cfg(not(ring_loom))]
const HEADER_SIZE: usize = 0x80;
#[cfg(ring_loom)]
const HEADER_SIZE: usize = (OFF_CONTROL + size_of::<Control>()).next_multiple_of(64);

// The shipped region format, pinned so the loom fork above can never move it.
#[cfg(not(ring_loom))]
const _: () = {
    assert!(size_of::<RegionHeader>() <= OFF_CONTROL);
    assert!(OFF_CONTROL + size_of::<Control>() <= HEADER_SIZE);
    assert!(size_of::<ReaderSlot>() == 64);
    assert!(align_of::<ReaderSlot>() == 8);
};
/// One reader-table slot's stride.
const READER_SLOT_SIZE: usize = size_of::<ReaderSlot>();

/// Sentinel cursor value marking a reader slot as free. A real cursor can
/// never reach it; that would take 16 EiB of committed data.
const FREE_SLOT: u64 = u64::MAX;
/// `high_water_mark` value meaning no wrap gap is pending.
const HWM_NONE: u64 = u64::MAX;

/// A tag identifying the architecture that wrote a region: the byte pattern of
/// this native `u64` differs across endianness, so comparing tags on attach
/// rejects a region written by an incompatible target.
///
/// Pointer width is deliberately *not* encoded. Nothing in the region format
/// is `usize`-shaped — every header, control, and reader-slot field is an
/// explicit width — and the one place pointer width genuinely matters, a
/// capacity too large for the attaching target's `usize`, is already caught
/// as [`AttachError::BadGeometry`], which is both narrower and more accurate
/// than refusing the region outright.
///
/// Encoding it would therefore reject a compatible region for no reason, and
/// the case is not hypothetical: a wasm guest's `usize` is four bytes where
/// its 64-bit host's is eight, and the two share ring regions in the guest's
/// linear memory (`metor_fsw_2::wasm`).
const fn arch_tag() -> u64 {
    (0x0102_0304u32 as u64) << 32
}

/// The tag stamped on writer and reader claims: the claiming process's id,
/// which is how [`RingBuffer::reclaim_owner`] identifies a dead process's
/// leftovers. Never `0` (that is the free writer claim). Pid reuse could
/// alias two claimants across a pid wrap-around, so reclaim promptly after
/// reaping a dead peer, before spawning replacements.
#[inline]
#[cfg(all(not(kani), not(target_arch = "wasm32")))]
fn owner_tag() -> u64 {
    std::process::id() as u64
}

/// Wasm has no process id — `std::process::id` is unsupported there and
/// panics — and a guest is not a process anyway: its regions live and die with
/// the instance, so there is no peer to outlive them and nothing to reclaim.
/// A fixed non-zero tag keeps the "never `0`" invariant.
#[inline]
#[cfg(all(not(kani), target_arch = "wasm32"))]
fn owner_tag() -> u64 {
    1
}

/// Kani has no process to ask, and the harnesses never depend on the tag's
/// value, only on it being nonzero.
#[inline]
#[cfg(kani)]
fn owner_tag() -> u64 {
    1
}

/// Round `n` up to the next multiple of 8.
#[inline]
pub const fn round_up8(n: usize) -> usize {
    (n + 7) & !7
}

/// Total size of a record carrying `payload_len` bytes of payload, which is
/// an 8-byte header plus the payload padded to an 8-byte boundary.
///
/// Public so callers can size a buffer for the records they intend to write
/// without re-deriving the header rule.
#[inline]
pub const fn frame_len(payload_len: usize) -> usize {
    8 + round_up8(payload_len)
}

// ---------------------------------------------------------------------------
// Position arithmetic
//
// The three predicates below carry the crate's load-bearing invariants:
// records never straddle the wrap, the writer never laps a reader, and a
// length read out of the region is bounded before it becomes a slice length.
// They are free functions over plain integers so `verify.rs` can prove them
// for every input rather than for the handful the unit tests pick.
// ---------------------------------------------------------------------------

/// Where a record of `rec` bytes goes when written at absolute position
/// `committed`, as `(start_abs, gap)`. A nonzero `gap` is the wrap gap left
/// unused at the end of the current lap because the record would not fit
/// contiguously.
///
/// `capacity` is a power of two and `rec <= capacity`.
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

/// Whether a write needing `need` bytes fits without overwriting the slowest
/// active reader.
///
/// Assumes `slowest <= committed`: a cursor is only ever moved to a position
/// the writer has already published. Violating it costs a wrapped subtraction,
/// which reads as "full" or overflows the sum, never as space that is not
/// there — see `KANI.md`. `Writer::fits` checks the assumption under the
/// verification cfgs.
#[inline]
const fn fits(committed: u64, slowest: u64, need: u64, capacity: u64) -> bool {
    committed.wrapping_sub(slowest) + need <= capacity
}

/// Whether a record whose length field reads `len` fits inside the data region
/// when its header starts at physical offset `phys`.
///
/// This is the straddle predicate `phys + frame_len(len) <= capacity`, kept in
/// `u64` and rewritten to avoid overflow. `capacity - 8 - phys` cannot
/// underflow (`phys <= capacity - 8` for an 8-aligned in-bounds header) and is
/// a multiple of 8, and for a multiple of 8 `m`, `round_up8(len) > m` exactly
/// when `len > m` — so comparing the raw length is the same test as comparing
/// the padded one, without the padding's chance to wrap.
///
/// The caller must not convert `len` to `usize` first: on a 32-bit target
/// `frame_len` of a garbage length wraps and defeats this very check.
#[inline]
const fn record_fits(len: u64, phys: u64, capacity: u64) -> bool {
    len <= capacity - 8 - phys
}

// ---------------------------------------------------------------------------
// Public config / errors
// ---------------------------------------------------------------------------

/// The sizes fixed when a ring buffer is created.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Data-region size in bytes. **Must be a power of two.**
    pub capacity: usize,
    /// Number of reader-table slots. Over-provision: a crashed reader's slot
    /// is only freed by an explicit [`RingBuffer::reclaim_owner`].
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
    /// The region violated a structural invariant. A record's length field
    /// claims the record straddles the wrap or overruns the data region,
    /// which nothing in this crate can produce. This variant exists so that a
    /// corrupted shared mapping degrades into an error instead of an
    /// out-of-bounds borrow.
    Corrupt,
}

impl core::fmt::Display for ReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ring region violates a structural invariant (possible external corruption)")
    }
}

impl std::error::Error for ReadError {}

/// The reader table is full; there is no free slot for another view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullReaderTable;

impl core::fmt::Display for FullReaderTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the ring's reader table is full; no free slot for another view")
    }
}

impl std::error::Error for FullReaderTable {}

/// A writer already exists for this buffer, or a crashed process leaked its
/// claim (reclaimable via [`RingBuffer::reclaim_owner`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterClaimed;

impl core::fmt::Display for WriterClaimed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("a writer already exists for this ring (or a crashed process leaked its claim)")
    }
}

impl std::error::Error for WriterClaimed {}

/// A region's header was invalid or written by an incompatible build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    BadMagic,
    BadVersion,
    /// Pointer width or endianness mismatch (see [`arch_tag`]).
    ArchMismatch,
    /// Region shorter than the fixed header.
    TooSmall,
    /// [`RingBuffer::attach_raw`] base pointer not 8-byte aligned.
    Misaligned,
    /// Header fields are internally inconsistent. The capacity is not a
    /// nonzero power of two or does not fit this target's `usize`, or an
    /// offset is misaligned, overlapping, or overflowing.
    BadGeometry,
    /// Header is self-consistent but `total_size` exceeds the backing region,
    /// as with a truncated file.
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
                f.write_str("region was written by a different pointer width or endianness")
            }
            AttachError::TooSmall => f.write_str("region is shorter than the fixed header"),
            AttachError::Misaligned => f.write_str("region base pointer is not 8-byte aligned"),
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

// ---------------------------------------------------------------------------
// Backing storage
// ---------------------------------------------------------------------------

/// One 8-byte, interior-mutable word, the unit of the heap backing. The
/// explicit `align(8)` matters because `u64` is only 4-aligned on some 32-bit
/// targets and the region's `AtomicU64` words need 8.
#[repr(C, align(8))]
struct Word(UnsafeCell<u64>);

/// The memory a ring lives in, reduced to a `(base, len)` byte range and its
/// concrete owner.
///
/// Every constructor guarantees the region contract that all of the ring's
/// `unsafe` relies on. `base..base + len` is a single allocation that is
/// interior-mutable (writing through pointers derived from `base` is sound),
/// 8-byte aligned, and stable for the backing's lifetime. The ring forms
/// `&AtomicU64` references and plain byte slices over it.
struct Backing {
    base: *mut u8,
    len: usize,
    _owner: BackingOwner,
}

enum BackingOwner {
    /// The heap allocation, held as a raw pointer rather than a live `Box`.
    ///
    /// Retaining the `Box` would be unsound. Moving it into this enum is a
    /// `Unique` retag, which invalidates every pointer already derived from
    /// it — and `Backing::base` is derived from it, so every access the ring
    /// ever makes would be through a dead tag. Miri reports it as the first
    /// write in `init_region`.
    Heap { words: *mut [Word] },
    #[cfg(feature = "mmap")]
    Mmap { _map: memmap2::MmapMut },
    Raw,
}

impl Backing {
    /// Allocate a zeroed heap region of at least `size` bytes.
    ///
    /// The region is a `Box<[Word]>` given up to a raw pointer, which makes it
    /// interior-mutable and 8-aligned on every target. `Drop` reconstructs the
    /// box to free it.
    pub fn heap(size: usize) -> Self {
        let count = size.div_ceil(8);
        let buf: Box<[Word]> = (0..count).map(|_| Word(UnsafeCell::new(0u64))).collect();
        let len = buf.len() * 8;
        // Give up the box before deriving `base`, so the pointer the ring uses
        // is the one that owns the allocation and nothing retags it later.
        let words = Box::into_raw(buf);
        Self {
            base: words.cast::<u8>(),
            len,
            _owner: BackingOwner::Heap { words },
        }
    }

    /// A non-owning backing over a region someone else owns. Dropping it
    /// frees nothing.
    ///
    /// # Safety
    /// `base..base + len` must satisfy the region contract (see [`Backing`]),
    /// must outlive every [`Writer`] and [`View`] produced over it, and must
    /// not be torn down concurrently.
    pub unsafe fn raw(base: *mut u8, len: usize) -> Self {
        Self {
            base,
            len,
            _owner: BackingOwner::Raw,
        }
    }

    /// An mmap-backed region. The mapping is unmapped when the backing drops.
    #[cfg(feature = "mmap")]
    fn mmap(map: memmap2::MmapMut) -> Self {
        let (base, len) = (map.as_ptr() as *mut u8, map.len());
        Self {
            base,
            len,
            _owner: BackingOwner::Mmap { _map: map },
        }
    }

    /// Base of the region.
    #[inline]
    fn base(&self) -> *mut u8 {
        self.base
    }

    /// Length of the region in bytes.
    #[inline]
    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        if let BackingOwner::Heap { words } = self._owner {
            // SAFETY: `words` came from `Box::into_raw` in `heap`, is the sole
            // owner of that allocation, and is reconstructed exactly once,
            // here. The mmap and raw variants free themselves or nothing.
            drop(unsafe { Box::from_raw(words) });
        }
    }
}

// SAFETY: the raw pointers make `Backing` `!Send + !Sync` by default, but the
// ring upholds both. Region bytes are only touched through atomics or under
// the `committed` Release/Acquire handshake, and the constructor contracts
// allow the concrete owner to drop on whichever thread drops the last handle.
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}

// ---------------------------------------------------------------------------
// Async wake abstraction (kept out of the shared layout)
// ---------------------------------------------------------------------------

/// Signals that progress was made, either new data committed or space freed.
pub trait WakeSource {
    fn notify(&self);
}

/// Awaits progress. `wait_until` completes once `ready()` returns true, and
/// implementations must re-check `ready` after arming to avoid lost wakeups.
#[allow(async_fn_in_trait)]
pub trait WakeSink {
    async fn wait_until<F: FnMut() -> bool>(&self, ready: F);
}

/// A wake implementation that does nothing.
///
/// The `try_*` paths never touch the wake hooks, so this is the right choice
/// for synchronous consumers. Under `NoWake` the async paths degenerate to
/// polling driven by the caller.
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

/// A shared wait queue that wakes tasks within one process. The writer and
/// its views share clones of a single `Notifier`, so a commit wakes the
/// awaiting readers.
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

// ---------------------------------------------------------------------------
// Inner: the shared region + cached, process-local geometry
// ---------------------------------------------------------------------------

struct Inner {
    backing: Backing,
    capacity: u64,
    mask: u64,
    data_offset: usize,
    reader_table_offset: usize,
    max_readers: u32,
}

impl Inner {
    #[inline]
    fn base(&self) -> *mut u8 {
        self.backing.base()
    }

    /// The control block.
    #[inline]
    fn control(&self) -> &Control {
        // SAFETY: `OFF_CONTROL + size_of::<Control>() <= HEADER_SIZE <= region
        // len` (established by `layout()` at create and `read_header` at
        // attach), and `base + OFF_CONTROL` is 8-aligned. `Control` is
        // all-atomic, so a shared reference over the shared backing is sound.
        unsafe { &*(self.base().add(OFF_CONTROL) as *const Control) }
    }

    #[inline]
    fn committed(&self) -> &AtomicU64 {
        &self.control().committed
    }

    #[inline]
    fn hwm(&self) -> &AtomicU64 {
        &self.control().hwm
    }

    /// The writer-claim word, where `0` means free (see
    /// [`RingBuffer::writer`]).
    #[inline]
    fn writer_claim(&self) -> &AtomicU64 {
        &self.control().writer
    }

    /// Reader-table slot `slot`.
    #[inline]
    fn slot(&self, slot: u32) -> &ReaderSlot {
        let off = self.reader_table_offset + slot as usize * READER_SLOT_SIZE;
        // SAFETY: `slot < max_readers`, so the slot lies within the reader
        // table (attach/layout geometry), 8-aligned. The atomic fields are
        // interior-mutable, and `_pad` is frozen by this shared borrow, which
        // is sound because it is written exactly once, before publication.
        unsafe { &*(self.base().add(off) as *const ReaderSlot) }
    }

    #[inline]
    fn slot_cursor(&self, slot: u32) -> &AtomicU64 {
        &self.slot(slot).cursor
    }

    #[inline]
    fn slot_owner(&self, slot: u32) -> &AtomicU64 {
        &self.slot(slot).owner
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

    /// The smallest cursor over all active readers, or `None` if there are
    /// none. This is the writer's in-use check.
    fn slowest_active_cursor(&self) -> Option<u64> {
        // The writer's half of the registration handshake described in the
        // crate docs. The fence pairs with the one in `RingBuffer::view` and
        // sits between the writer's previous `committed` store and this scan,
        // so either the scan observes a new reader's claim or that reader's
        // registration recheck observes the store.
        fence(SeqCst);
        (0..self.max_readers)
            .map(|slot| self.slot_cursor(slot).load(Acquire))
            .filter(|&cursor| cursor != FREE_SLOT)
            .min()
    }

    /// Compute the wrap for a record of `rec` bytes written at absolute
    /// position `committed`. Returns `(start_abs, gap)`, where a nonzero
    /// `gap` is a wrap gap left at the end of the current lap.
    #[inline]
    fn reserve(&self, committed: u64, rec: u64) -> (u64, u64) {
        reserve(committed, rec, self.capacity)
    }

    /// Write a record's header and payload at physical offset `phys`. Plain
    /// stores are race-free here. The in-use check guarantees no reader is
    /// inside these bytes, and publication through `committed` orders the
    /// stores before every read.
    ///
    /// # Safety
    /// `phys + frame_len(payload.len()) <= capacity` (the record does not
    /// straddle the wrap), and the caller holds the single-writer role.
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

    /// Read a record's length field at physical offset `phys`.
    ///
    /// Returns the raw `u32` widened to `u64`. A corrupted region can carry
    /// arbitrary bytes, so the caller must validate the value against the
    /// data region before converting to `usize` or doing pointer math; on a
    /// 32-bit target, `frame_len` of a garbage length would wrap `usize`.
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
        unsafe { std::ptr::copy_nonoverlapping(self.data_ptr(phys + 8), dst.as_mut_ptr(), len) };
    }
}

// ---------------------------------------------------------------------------
// RingBuffer
// ---------------------------------------------------------------------------

/// A cloneable handle to one shared region, from which the single [`Writer`]
/// and any number of [`View`]s are created. The writer and views produced
/// from a handle may outlive it.
#[derive(Clone)]
pub struct RingBuffer {
    inner: Arc<Inner>,
}

impl RingBuffer {
    /// Create an in-process, heap-backed ring buffer.
    ///
    /// Panics if `capacity` is not a non-zero power of two, or `max_readers`
    /// is zero.
    pub fn create_in_memory(cfg: Config) -> Self {
        let (reader_table_offset, data_offset, total) = layout(&cfg);
        let backing = Backing::heap(total);
        // SAFETY: `backing` is a fresh region of `total` bytes, exclusively
        // owned here (no other handle exists yet).
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

    /// Format a ring into a region the caller owns, then attach to it.
    ///
    /// The counterpart to [`create_in_memory`](Self::create_in_memory) for a
    /// host that must place the region somewhere specific rather than on its
    /// own heap — a wasm host puts rings inside the guest's linear memory so
    /// the guest can attach to them by offset, since a guest cannot follow a
    /// pointer into the host.
    ///
    /// `len` must be at least [`region_len`] for `cfg`, and `base` must be
    /// 8-aligned; both are checked. The region is left formatted, so a later
    /// [`attach_raw`](Self::attach_raw) over the same bytes reads the same
    /// geometry back.
    ///
    /// # Safety
    /// `base..base + len` is a live, exclusively-owned region that outlives
    /// every [`Writer`] and [`View`] produced here, and nothing else is
    /// reading it while this formats it.
    pub unsafe fn create_raw(base: *mut u8, len: usize, cfg: Config) -> Result<Self, AttachError> {
        if !(base as usize).is_multiple_of(8) {
            return Err(AttachError::Misaligned);
        }
        let (reader_table_offset, data_offset, total) = layout(&cfg);
        if len < total {
            return Err(AttachError::TooSmall);
        }
        // SAFETY: caller asserts a live region of at least `len` bytes, and
        // `total <= len` was just checked.
        let backing = unsafe { Backing::raw(base, total) };
        // SAFETY: same region, exclusively owned by the caller, and nothing
        // else observes it until this returns.
        unsafe { init_region(&backing, &cfg, reader_table_offset, data_offset, total) };
        // SAFETY: the region was just formatted, so its header is valid.
        unsafe { Self::attach_raw(base, total) }
    }

    /// Create a new mmap-backed region at `path`, truncating any existing
    /// file.
    #[cfg(feature = "mmap")]
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
        let backing = Backing::mmap(map);
        // SAFETY: freshly sized, exclusively-held region.
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
    /// The caller asserts `path` is a ring-buffer region created by a
    /// compatible build and is not being concurrently torn down. The
    /// magic/version/arch handshake guards against accidental misuse but not
    /// deliberate corruption.
    #[cfg(feature = "mmap")]
    pub unsafe fn attach_mmap(path: &std::path::Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;
        // SAFETY: caller asserts a valid region; mapping read+write shared.
        let map = unsafe { memmap2::MmapMut::map_mut(&file)? };
        // SAFETY: caller asserts a live, valid region; `attach` reads and
        // checks the header before forming any reference into it.
        unsafe { Self::attach(Backing::mmap(map)) }
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Attach a non-owning handle to a region the caller manages, validating
    /// its header and reading the geometry back out of it. Use
    /// [`RingBuffer::region`] to obtain the `(base, len)` of an existing
    /// buffer in the same process.
    ///
    /// # Safety
    /// `base..base + len` is a live ring region that outlives every
    /// [`Writer`] and [`View`] produced here and is not torn down
    /// concurrently.
    pub unsafe fn attach_raw(base: *mut u8, len: usize) -> Result<Self, AttachError> {
        // This path takes an arbitrary pointer (the heap and mmap backings
        // are aligned by construction), so alignment is checked here.
        // Everything else is validated by the shared `read_header` path.
        if !(base as usize).is_multiple_of(8) {
            return Err(AttachError::Misaligned);
        }
        // SAFETY: caller asserts a live region of `len` readable bytes (the
        // `Backing::raw` contract); `read_header` bounds every header read
        // against that length.
        unsafe { Self::attach(Backing::raw(base, len)) }
    }

    /// Attach over an already-initialized concrete backing, validating its
    /// header and reading the geometry back out of it.
    ///
    /// # Safety
    /// `backing`'s region is live (all [`Backing::len`] bytes are readable)
    /// and is not being torn down concurrently. `read_header` validates
    /// everything else before any reference is formed into the region.
    unsafe fn attach(backing: Backing) -> Result<Self, AttachError> {
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

    /// The backing region's base pointer and length, suitable for
    /// [`RingBuffer::attach_raw`] from elsewhere in the same process. The
    /// pointer is only valid while something keeps the region alive.
    pub fn region(&self) -> (*mut u8, usize) {
        (self.inner.backing.base(), self.inner.backing.len())
    }

    /// The absolute number of bytes published so far.
    pub fn committed(&self) -> u64 {
        self.inner.committed().load(Acquire)
    }

    /// Create the single writer for this buffer, claiming the writer role in
    /// the shared region.
    ///
    /// The claim lives in the region itself, so it is enforced across handles
    /// and across processes. Dropping the [`Writer`] frees it; a crashed
    /// process leaks it, and [`reclaim_owner`](RingBuffer::reclaim_owner)
    /// frees it.
    ///
    /// `data` is notified after every commit. Pass [`NoWake`] if only the
    /// synchronous APIs are used.
    pub fn writer<WD: WakeSource>(&self, data: WD) -> Result<Writer<WD>, WriterClaimed> {
        // Acquire on success pairs with the Release store in `Writer::drop`
        // and `reclaim_owner`, handing the region state (committed,
        // hwm, and the data bytes) from the previous writer to this one. The
        // claimed value is this process's owner tag, so `reclaim_owner` can
        // free a dead claimant's role.
        self.inner
            .writer_claim()
            .compare_exchange(0, owner_tag(), Acquire, Relaxed)
            .map_err(|_| WriterClaimed)?;
        Ok(Writer {
            inner: self.inner.clone(),
            data,
        })
    }

    /// Forcibly free everything a dead process left claimed in this region:
    /// every reader slot whose owner tag is `pid`, and the writer claim if
    /// that process held it. A pinned dead cursor otherwise backpressures the
    /// writer forever, so a host that detects a peer's death runs this over
    /// each region the peer had attached.
    ///
    /// Freed space wakes no one: writers never wait for space, so the next
    /// non-blocking write simply finds the room.
    ///
    /// # Safety
    /// The caller asserts the process identified by `pid` (its
    /// `std::process::id()`) is dead — exited and reaped, so none of its
    /// stores are still in flight. Reclaiming a live process's claims
    /// re-creates the very races the claim word and reader registration
    /// exist to prevent. Handles the dead process left behind must never be
    /// used again. `pid` must not alias a live claimant through pid reuse;
    /// reclaim promptly after reaping, before spawning replacements.
    pub unsafe fn reclaim_owner(&self, pid: u64) {
        for slot in 0..self.inner.max_readers {
            if self.inner.slot_cursor(slot).load(Acquire) != FREE_SLOT
                && self.inner.slot_owner(slot).load(Acquire) == pid
            {
                // Free the cursor. Release pairs with the claim CAS in `view`.
                self.inner.slot_cursor(slot).store(FREE_SLOT, Release);
            }
        }
        // Release the writer claim iff the dead process held it. Release so
        // the next claimer's Acquire CAS orders after the observed state.
        let _ = self
            .inner
            .writer_claim()
            .compare_exchange(pid, 0, Release, Relaxed);
    }

    /// Register a new view, claiming a free slot in the reader table.
    ///
    /// A fresh view only sees data committed from now on. The async
    /// [`View::read`] awaits `data`.
    pub fn view<RD: WakeSink>(&self, data: RD) -> Result<View<RD>, FullReaderTable> {
        let mut start = self.inner.committed().load(Acquire);
        for slot in 0..self.inner.max_readers {
            // Acquire pairs with the Release of FREE_SLOT in `View::drop`,
            // handing the slot from its previous owner. Visibility to the
            // writer's scan comes from the SeqCst handshake below, not from
            // this CAS.
            if self
                .inner
                .slot_cursor(slot)
                .compare_exchange(FREE_SLOT, start, AcqRel, Relaxed)
                .is_ok()
            {
                // Stamp the owner tag immediately after the claim, keeping
                // the window in which a crash leaves the slot claimed but
                // unattributed (unreclaimable) as small as possible.
                self.inner.slot_owner(slot).store(owner_tag(), Release);
                // The registration handshake described in the crate docs. The
                // fence pairs with the one in `slowest_active_cursor`. Loop
                // until `committed` is stable across the fence, which proves
                // that every write the writer validated without seeing us was
                // bounded by some other cursor at or behind ours. This
                // converges in one or two iterations, and registration is a
                // cold path, so no bound is imposed.
                loop {
                    fence(SeqCst);
                    // Acquire, so that on the break path this view's later
                    // record reads are ordered after the writes committed
                    // before `start`.
                    let c2 = self.inner.committed().load(Acquire);
                    if c2 == start {
                        break;
                    }
                    // The writer committed while our claim may have been
                    // invisible to its scan, so those writes were validated
                    // without us. Advance the claim to the new edge and
                    // re-verify. A fresh view only promises data committed
                    // from now on, so this loses nothing.
                    start = c2;
                    self.inner.slot_cursor(slot).store(start, Release);
                }
                return Ok(View {
                    inner: self.inner.clone(),
                    slot,
                    data,
                });
            }
        }
        Err(FullReaderTable)
    }

    /// Number of currently registered views. Test-only introspection into the
    /// reader table, for the slot-reclamation tests, which the loom models do
    /// not run.
    #[cfg(all(test, not(ring_loom)))]
    pub(crate) fn reader_count(&self) -> usize {
        (0..self.inner.max_readers)
            .filter(|&s| self.inner.slot_cursor(s).load(Relaxed) != FREE_SLOT)
            .count()
    }
}

/// The [`Config`] an existing region was formatted with, read back out of its
/// header.
///
/// For a caller that must build a *second* ring matching a first — a wasm host
/// giving its guest a region mirroring the coordinator's, so the two sides of
/// a copy always agree on what a record can be.
///
/// # Safety
/// `base..base + len` is a live, header-valid ring region.
pub unsafe fn config_of(base: *mut u8, len: usize) -> Result<Config, AttachError> {
    // SAFETY: caller asserts a live, header-valid region; `attach_raw`
    // validates before forming any reference into it.
    let ring = unsafe { RingBuffer::attach_raw(base, len) }?;
    Ok(Config {
        capacity: ring.inner.capacity as usize,
        max_readers: ring.inner.max_readers as usize,
    })
}

/// The number of bytes a ring with `cfg` occupies, for a caller that has to
/// allocate the region itself before [`RingBuffer::create_raw`] formats it.
///
/// Panics on the same invalid configs [`RingBuffer::create_in_memory`] does.
pub fn region_len(cfg: &Config) -> usize {
    layout(cfg).2
}

/// Compute `(reader_table_offset, data_offset, total_size)` and validate the
/// config.
fn layout(cfg: &Config) -> (usize, usize, usize) {
    assert!(
        cfg.capacity.is_power_of_two(),
        "capacity must be a power of two, got {}",
        cfg.capacity
    );
    // Below one record header no write could ever succeed, and `read_len`'s
    // `phys + 8 <= capacity` contract would break. `read_header` performs the
    // same check on attach.
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

/// Write the header and initialize the control words and reader slots.
///
/// # Safety
/// `backing` is a freshly allocated, exclusively-owned region of `total`
/// bytes, and this runs single-threaded before any handle is published.
unsafe fn init_region(
    backing: &Backing,
    cfg: &Config,
    reader_table_offset: usize,
    data_offset: usize,
    total: usize,
) {
    let base = backing.base();
    debug_assert!(backing.len() >= total);
    // SAFETY: all offsets are within `total` bytes and 8-aligned; no other
    // thread can observe these writes yet.
    unsafe {
        base.cast::<RegionHeader>().write(RegionHeader {
            magic: MAGIC,
            version: VERSION,
            flags: 0,
            capacity: cfg.capacity as u64,
            data_offset: data_offset as u64,
            max_readers: cfg.max_readers as u32,
            reader_table_offset: reader_table_offset as u32,
            total_size: total as u64,
            arch_tag: arch_tag(),
        });
        base.add(OFF_CONTROL).cast::<Control>().write(Control {
            committed: AtomicU64::new(0),
            hwm: AtomicU64::new(HWM_NONE),
            writer: AtomicU64::new(0),
        });
        for slot in 0..cfg.max_readers {
            base.add(reader_table_offset + slot * READER_SLOT_SIZE)
                .cast::<ReaderSlot>()
                .write(ReaderSlot {
                    cursor: AtomicU64::new(FREE_SLOT),
                    owner: AtomicU64::new(0),
                    _pad: [0; READER_SLOT_PAD],
                });
        }
    }
}

/// The offsets and sizes read back from an existing region's header.
struct Geometry {
    capacity: u64,
    data_offset: usize,
    reader_table_offset: usize,
    max_readers: u32,
}

/// Read and validate the immutable header fields of an existing region.
///
/// # Safety
/// `base` points at a readable region of at least `region_len` bytes.
unsafe fn read_header(base: *mut u8, region_len: usize) -> Result<Geometry, AttachError> {
    // Reject a too-small region before any header field is read. A sub-header
    // mapping of a truncated file lands here too.
    if region_len < HEADER_SIZE {
        return Err(AttachError::TooSmall);
    }
    // SAFETY: bytes `0..size_of::<RegionHeader>()` are in-bounds (checked
    // above) and were written once by `init_region` before the region was
    // published, so this transient shared view cannot race a live writer. Do
    // not widen it to `HEADER_SIZE`; attaching can happen while a live
    // region's control words are being mutated.
    let hdr_bytes =
        unsafe { core::slice::from_raw_parts(base as *const u8, size_of::<RegionHeader>()) };
    let hdr = RegionHeader::read_from_bytes(hdr_bytes).expect("exact-size slice");
    validate_header(&hdr, region_len)
}

/// Check a region header against the geometry invariant.
///
/// After `Ok`, every offset the ring will ever dereference (the control block,
/// all `max_readers` reader slots, and `data_offset + phys` for
/// `phys < capacity`) lies inside `[0, region_len)` and is 8-aligned. In other
/// words, attaching restores the same geometry invariant that `layout` and
/// `init_region` establish at creation. All arithmetic is checked `u64`, so a
/// hostile header cannot overflow its way past a bound.
///
/// Split out from `read_header` and free of raw pointers so `verify.rs` can
/// run it against a fully symbolic header.
fn validate_header(hdr: &RegionHeader, region_len: usize) -> Result<Geometry, AttachError> {
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

    // The capacity must be a power of two for the cursor mask to work, hold
    // at least one record header to satisfy `read_len`, and fit this target's
    // `usize` (a 32-bit target can attach a region sized by a 64-bit
    // creator).
    if !capacity.is_power_of_two() || capacity < 8 || capacity > usize::MAX as u64 {
        return Err(AttachError::BadGeometry);
    }
    if max_readers == 0 {
        return Err(AttachError::BadGeometry);
    }
    // The reader table must sit behind the header, 8-aligned, and end at or
    // before the data region.
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
    // The data region must be 8-aligned and end at or before total_size.
    let data_end = data_offset
        .checked_add(capacity)
        .ok_or(AttachError::BadGeometry)?;
    if !data_offset.is_multiple_of(8) || data_end > total_size {
        return Err(AttachError::BadGeometry);
    }
    // The self-declared total must fit the actual backing region. A file
    // truncated on disk gets its own variant because it is the diagnosable
    // real-world failure.
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

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// The single producer for a buffer and the sole mutator of `committed` and
/// the high-water mark. At most one exists per region, enforced by the claim
/// word that [`RingBuffer::writer`] takes.
pub struct Writer<WD: WakeSource> {
    inner: Arc<Inner>,
    data: WD,
}

impl<WD: WakeSource> Writer<WD> {
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

    /// Whether a write needing `need` bytes fits without overwriting the
    /// slowest active reader.
    #[inline]
    fn fits(&self, committed: u64, need: u64) -> bool {
        let slowest = self.inner.slowest_active_cursor().unwrap_or(committed);
        // The assumption `fits` is written against. Checked only where it is
        // being verified: a violation costs a spurious `WouldBlock`, so making
        // it a panic in every downstream debug build would trade a benign
        // outcome for a crash. Loom model
        // `cursor_never_exceeds_committed_at_fits` is what exercises it.
        #[cfg(any(test, ring_loom))]
        debug_assert!(
            slowest <= committed,
            "cursor {slowest} is ahead of committed {committed}"
        );
        fits(committed, slowest, need, self.inner.capacity)
    }

    /// Write the record bytes, publish the wrap gap (if any) and the new
    /// `committed`, and wake waiting readers.
    ///
    /// # Safety
    /// `start_abs & mask` plus `frame_len(bytes.len())` does not exceed
    /// `capacity` (the record is contiguous), we hold the single-writer role,
    /// and the in-use check passed (no reader cursor is inside the reused
    /// bytes).
    unsafe fn commit(&self, committed: u64, start_abs: u64, gap: u64, bytes: &[u8]) {
        let end_abs = start_abs + frame_len(bytes.len()) as u64;
        let phys = (start_abs & self.inner.mask) as usize;
        // SAFETY: record is contiguous and in-bounds (caller contract).
        unsafe { self.inner.write_record(phys, bytes) };
        if gap > 0 {
            // Publish where valid data ends before the gap so readers skip it.
            self.inner.hwm().store(committed, Release);
        }
        // Release hands the freshly written bytes (and the hwm store) to
        // readers, which Acquire-load `committed` before touching the ring.
        self.inner.committed().store(end_abs, Release);
        self.data.notify();
    }
}

impl<WD: WakeSource> Drop for Writer<WD> {
    fn drop(&mut self) {
        // Free the writer claim. Release pairs with the Acquire CAS in
        // `RingBuffer::writer`, handing the region state to the next claimer.
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

/// A reader with its own absolute cursor in the shared reader table,
/// registered by [`RingBuffer::view`]. Dropping it frees its slot.
pub struct View<RD: WakeSink> {
    inner: Arc<Inner>,
    slot: u32,
    data: RD,
}

impl<RD: WakeSink> View<RD> {
    /// This view's current absolute read cursor.
    pub fn cursor(&self) -> u64 {
        self.inner.slot_cursor(self.slot).load(Acquire)
    }

    /// The buffer's absolute committed position.
    pub fn committed(&self) -> u64 {
        self.inner.committed().load(Acquire)
    }

    /// Copy the next record into `buf`. Returns `Ok(true)` if a record was
    /// read and `Ok(false)` if the view is caught up.
    ///
    /// Prefer [`View::try_read`]. Borrowing is always tear-free here, so this
    /// copying form exists only for callers that must own the bytes.
    pub fn try_read_into(&mut self, buf: &mut Vec<u8>) -> Result<bool, ReadError> {
        let Some(loc) = self.locate()? else {
            return Ok(false);
        };
        // SAFETY: `loc` came from `locate`, so the record is contiguous and
        // in-bounds, and published via `committed`.
        unsafe { self.inner.copy_payload(loc.phys, loc.len, buf) };
        self.advance(loc.r + loc.rec as u64);
        Ok(true)
    }

    /// Borrow the next record without copying.
    ///
    /// The writer cannot overwrite a record this view has not consumed, so
    /// the borrow is stable for its whole lifetime. Dropping the grant
    /// consumes the record; the cursor advances past it and a waiting writer
    /// is woken.
    pub fn try_read(&mut self) -> Result<Option<ReadGrant<'_>>, ReadError> {
        let Some(loc) = self.locate()? else {
            return Ok(None);
        };
        // SAFETY: `locate` guarantees the record is in-bounds, and the writer
        // will not overwrite these bytes while the borrow lives (the cursor
        // stays at or before `loc.r` until the grant drops).
        let slice = unsafe {
            let p = self.inner.data_ptr(loc.phys + 8);
            std::slice::from_raw_parts(p as *const u8, loc.len)
        };
        Ok(Some(ReadGrant {
            inner: &self.inner,
            slot: self.slot,
            end_abs: loc.r + loc.rec as u64,
            slice,
        }))
    }

    /// Await the next record and borrow it.
    pub async fn read(&mut self) -> Result<ReadGrant<'_>, ReadError> {
        // Awaiting and borrowing are split for the borrow checker. A grant
        // taken inside the wait loop would pin the `self` borrow across every
        // iteration. Nothing can consume a record between the successful
        // `locate` and the `try_read` because we hold `&mut self`.
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

    /// Borrow the newest committed record, consuming every older unread
    /// record.
    ///
    /// The cursor parks at the returned record's start, so the record stays
    /// pinned (the writer cannot reclaim it) and calling again with no new
    /// data returns the same record. Returns `Ok(None)` only before the first
    /// record is committed, or after the stream was fully consumed through
    /// [`View::try_read`] or [`View::try_read_into`].
    pub fn try_latest(&mut self) -> Result<Option<ReadGrant<'_>>, ReadError> {
        loop {
            let Some(loc) = self.locate()? else {
                return Ok(None);
            };
            let end = loc.r + loc.rec as u64;
            // This is the newest record if nothing is committed past it. The
            // snapshot is retaken each pass, so a racing commit just means
            // one more skip iteration.
            if end >= self.inner.committed().load(Acquire) {
                // SAFETY: as in `try_read`; the cursor stays at `loc.r` (the
                // grant's drop re-stores it), so the record stays pinned.
                let slice = unsafe {
                    let p = self.inner.data_ptr(loc.phys + 8);
                    std::slice::from_raw_parts(p as *const u8, loc.len)
                };
                return Ok(Some(ReadGrant {
                    inner: &self.inner,
                    slot: self.slot,
                    // Park at the record's start rather than its end. This is
                    // the pin that makes the next `try_latest` re-serve it.
                    end_abs: loc.r,
                    slice,
                }));
            }
            // An older record. Consume it so the writer can reuse its bytes.
            self.advance(end);
        }
    }

    /// Find the next readable record, skipping a wrap gap if the cursor sits
    /// on one. Returns `Ok(None)` when the view is caught up.
    fn locate(&self) -> Result<Option<Located>, ReadError> {
        let cap = self.inner.capacity;
        loop {
            let r = self.inner.slot_cursor(self.slot).load(Acquire);
            // The load order is load-bearing; `hwm` comes after `committed`.
            // Seeing a post-wrap `committed` implies the wrap's earlier `hwm`
            // store is visible, so a reader sitting at a gap always sees the
            // marker before it could misread stale gap bytes as a record
            // header.
            let c = self.inner.committed().load(Acquire);
            let hwm = self.inner.hwm().load(Acquire);
            if r == hwm {
                // Skip the wrap gap and resume at the next lap boundary; the
                // gap bytes were never data. `hwm` values strictly increase,
                // so a given `r` can match at most one gap ever and a stale
                // `hwm` cannot re-trigger.
                let lap_end = (r & !self.inner.mask) + cap;
                self.inner.slot_cursor(self.slot).store(lap_end, Release);
                continue;
            }
            // The gap skip can transiently park the cursor ahead of
            // `committed`, because a wrap's `hwm` publishes before its
            // `committed`. Hence `>=`.
            if r >= c {
                return Ok(None); // caught up
            }
            let phys = (r & self.inner.mask) as usize;
            // SAFETY: phys is an 8-aligned, in-bounds record start (geometry
            // guarantees `capacity >= 8`, so `phys <= cap - 8`), and `r < c`
            // orders this read after the record's publication.
            let len = unsafe { self.inner.read_len(phys) };
            // A real record never straddles and the writer can never lap a
            // reader, so a failure here means the region is corrupt and must
            // not be borrowed from.
            if !record_fits(len, phys as u64, cap) {
                return Err(ReadError::Corrupt);
            }
            let len = len as usize;
            let rec = frame_len(len);
            return Ok(Some(Located { r, phys, len, rec }));
        }
    }

    /// Advance the cursor past a consumed record.
    #[inline]
    fn advance(&self, end_abs: u64) {
        self.inner.slot_cursor(self.slot).store(end_abs, Release);
    }
}

impl<RD: WakeSink> Drop for View<RD> {
    fn drop(&mut self) {
        self.inner.slot_cursor(self.slot).store(FREE_SLOT, Release);
    }
}

/// A borrowed, tear-free view of one record. Derefs to the payload bytes.
///
/// Dropping the grant moves the view's cursor to `end_abs`. For a
/// [`View::try_read`] or [`View::read`] grant that is past the record,
/// consuming it. For a [`View::try_latest`] grant it is back to the record's
/// start, keeping it pinned.
pub struct ReadGrant<'a> {
    inner: &'a Inner,
    slot: u32,
    end_abs: u64,
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
        self.inner
            .slot_cursor(self.slot)
            .store(self.end_abs, Release);
    }
}

// The std tests drive real threads and mmap; a loom atomic touched outside
// `loom::model` panics, so the two test modules are mutually exclusive.
#[cfg(all(test, not(ring_loom)))]
mod tests;

#[cfg(all(test, ring_loom))]
mod loom_tests;

#[cfg(kani)]
mod verify;
