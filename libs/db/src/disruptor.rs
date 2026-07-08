use std::{
    cell::UnsafeCell,
    ops::{Deref, DerefMut, Range},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicPtr, AtomicU64, Ordering},
    },
};
use stellarator::sync::WaitQueue;

#[derive(Clone)]
pub struct Disruptor {
    core: Arc<DistruptorCore>,
}

impl Disruptor {
    pub fn new(capacity: usize) -> Self {
        let core = Arc::new(DistruptorCore {
            ringbuf: (0..capacity).map(|_| UnsafeCell::new(0)).collect(),
            write_head: WriteHead {
                committed: AtomicU64::new(0),
                // `u64::MAX` means "no pending wrap gap"; only a real wrap sets a
                // concrete pre-gap end. See `Reader::poll_grant`.
                high_water_mark: AtomicU64::new(u64::MAX),
                write_lock: std::sync::Mutex::new(()),
            },
            readers: Readers::new(),
            new_data_queue: WaitQueue::new(),
        });
        Self { core }
    }

    // TODO(sphw): this function "works", but can cause deadlocks due to the mutex lock
    // pub async fn grant(&self, len: usize) -> Result<WriteGrant<'_>, DisruptorError> {
    //     let Disruptor { core } = self;
    //     let _lock_guard = core.write_head.write_lock.lock().expect("poisoned lock");
    //     let mut write = (core.write_head.committed.load(Ordering::Acquire)
    //         % core.ringbuf.len() as u64) as usize;
    //     let max = core.ringbuf.len();
    //     if len > max {
    //         return Err(DisruptorError::InsufficientCapacity);
    //     }
    //     let _ = core
    //         .buffer_full_cell
    //         .wait_for(|| can_write(core, len, write, max))
    //         .await;

    //     if write + len > max {
    //         write = 0;
    //         core.write_head
    //             .high_water_mark
    //             .store(write as u64, Ordering::Release);
    //     } else if write + len > core.write_head.high_water_mark.load(Ordering::Acquire) as usize {
    //         core.write_head
    //             .high_water_mark
    //             .store((write + len) as u64, Ordering::Release);
    //     }

    //     Ok(WriteGrant {
    //         range: write..write + len,
    //         disruptor: self.core.as_ref(),
    //         _lock_guard,
    //     })
    // }

    pub fn try_grant(&self, len: usize) -> Result<WriteGrant<'_>, DisruptorError> {
        let Disruptor { core } = self;
        let cap = core.ringbuf.len() as u64;
        if len as u64 > cap {
            return Err(DisruptorError::InsufficientCapacity);
        }
        let _lock_guard = core.write_head.write_lock.lock().expect("poisoned");

        // All cursors are absolute, monotonically increasing byte counts; the
        // physical ring offset of an absolute position `a` is `a % cap`.
        let committed = core.write_head.committed.load(Ordering::Acquire);
        let phys = committed % cap;
        // If the write would not fit contiguously in the tail, wrap to offset 0
        // and leave a gap spanning the rest of this lap.
        let gap = if phys + len as u64 > cap {
            cap - phys
        } else {
            0
        };
        let need = gap + len as u64;

        // Backpressure: the slowest reader must not be overwritten. With every
        // cursor absolute, in-flight bytes = committed - slowest_cursor; a reader
        // advancing concurrently only frees space, so a stale (older) cursor is
        // conservatively safe.
        let slowest = core.slowest_cursor().unwrap_or(committed);
        let in_use = committed - slowest;
        if in_use + need > cap {
            return Err(DisruptorError::WouldBlock);
        }

        let start_abs = committed + gap;
        if gap > 0 {
            // Publish where valid data ends before the gap so readers skip it.
            core.write_head
                .high_water_mark
                .store(committed, Ordering::Release);
        }

        let start = (start_abs % cap) as usize;
        Ok(WriteGrant {
            range: start..start + len,
            end_abs: start_abs + len as u64,
            disruptor: self.core.as_ref(),
            _lock_guard,
        })
    }

    pub fn reader(&self) -> Reader {
        // A new reader only ever sees data committed from now on.
        let write = self.core.write_head.committed.load(Ordering::Acquire);
        let node = self.core.readers.push(write);
        Reader {
            node,
            core: self.core.clone(),
        }
    }

    pub fn reader_count(&self) -> usize {
        self.core.readers.reader_count()
    }
}

pub struct DistruptorCore {
    ringbuf: Box<[UnsafeCell<u8>]>,
    write_head: WriteHead,
    readers: Readers,
    new_data_queue: WaitQueue,
}

// SAFETY: `ringbuf` is interior-mutable byte storage. Writes are serialized by
// `write_head.write_lock` (single writer at a time); readers only touch byte
// ranges below `committed`, published via the `committed` Release/Acquire
// handshake, and backpressure (`slowest_cursor`) guarantees the writer never
// overwrites a region a reader is still reading. So no two threads ever access
// the same byte without a happens-before edge between them.
unsafe impl Sync for DistruptorCore {}

impl DistruptorCore {
    /// Raw pointer to byte `offset` of the ring, carrying whole-buffer
    /// provenance. Deriving from the slice base (not an indexed `UnsafeCell`)
    /// keeps the provenance wide enough to form multi-byte grant slices, and the
    /// `UnsafeCell`s make it `SharedReadWrite` so writing through it is sound.
    fn byte_ptr(&self, offset: usize) -> *mut u8 {
        let base = self.ringbuf.as_ptr() as *mut u8;
        // SAFETY: callers only pass offsets within the reserved range, which is
        // within the buffer.
        unsafe { base.add(offset) }
    }

    /// The smallest (oldest) absolute cursor across all live readers, or `None`
    /// if there are no readers.
    fn slowest_cursor(&self) -> Option<u64> {
        let mut slowest: Option<u64> = None;
        let mut cursor = self.readers.first();
        while let Some(node) = cursor {
            let read = node.cursor.load(Ordering::Acquire);
            if read != FREE_SLOT {
                slowest = Some(slowest.map_or(read, |s| s.min(read)));
            }
            cursor = node.next();
        }
        slowest
    }
}

pub struct WriteHead {
    committed: AtomicU64,
    high_water_mark: AtomicU64,
    write_lock: std::sync::Mutex<()>,
}

pub struct WriteGrant<'a> {
    /// Physical byte range within `ringbuf` (already offset by any wrap gap).
    range: Range<usize>,
    /// Absolute commit position once this grant is dropped.
    end_abs: u64,
    disruptor: &'a DistruptorCore,
    _lock_guard: std::sync::MutexGuard<'a, ()>,
}

impl Deref for WriteGrant<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: `range` is the physical span reserved for this grant; the
        // write lock gives us exclusive access to it.
        let p = self.disruptor.byte_ptr(self.range.start);
        unsafe { std::slice::from_raw_parts(p as *const u8, self.range.len()) }
    }
}

impl DerefMut for WriteGrant<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: the write lock makes our access to `range` exclusive, and
        // `byte_ptr` has whole-buffer write provenance.
        let p = self.disruptor.byte_ptr(self.range.start);
        unsafe { std::slice::from_raw_parts_mut(p, self.range.len()) }
    }
}

impl Drop for WriteGrant<'_> {
    fn drop(&mut self) {
        // Publishing `committed` (Release) hands the freshly written bytes to
        // readers, which load it with Acquire before touching the ring.
        self.disruptor
            .write_head
            .committed
            .store(self.end_abs, Ordering::Release);
        self.disruptor.new_data_queue.wake_all();
    }
}

pub struct Reader {
    node: Arc<ReadNode>,
    core: Arc<DistruptorCore>,
}

impl Reader {
    /// Compute the next readable physical range, or `None` if no data is
    /// available. Returns `(physical_range, absolute_end)`. May advance this
    /// reader's own cursor to skip a wrap gap (it owns its `ReadNode`, and the
    /// writer only ever *reads* the cursor, for backpressure).
    fn poll_grant(&self) -> Option<(Range<usize>, u64)> {
        let node = self.node.as_ref();
        let cap = self.core.ringbuf.len() as u64;
        loop {
            let read = node.cursor.load(Ordering::Acquire);
            let write = self.core.write_head.committed.load(Ordering::Acquire);
            if read >= write {
                return None; // caught up (absolute counters → unambiguous)
            }
            let hwm = self.core.write_head.high_water_mark.load(Ordering::Acquire);
            let lap_end = (read / cap + 1) * cap;

            // `hwm` marks the end of valid data before a pending wrap gap. When
            // our cursor reaches it exactly, skip the rest of the lap (the gap)
            // and resume at the next lap boundary. `hwm == u64::MAX` means no
            // gap; gaps never land on a lap boundary, so this can't false-fire
            // on a clean wrap.
            if read == hwm {
                node.cursor.store(lap_end, Ordering::Release);
                continue;
            }

            let run_end = if hwm > read && hwm < lap_end {
                hwm
            } else {
                write.min(lap_end)
            };
            let len = (run_end - read) as usize;
            let start = (read % cap) as usize;
            return Some((start..start + len, read + len as u64));
        }
    }

    pub async fn next(&mut self) -> ReadGrant<'_> {
        let (range, end_abs) = self
            .core
            .new_data_queue
            .wait_for_value(|| self.poll_grant())
            .await
            .expect("queue closed");
        ReadGrant {
            range,
            end_abs,
            reader: self,
        }
    }

    pub fn try_next(&mut self) -> Option<ReadGrant<'_>> {
        let (range, end_abs) = self.poll_grant()?;
        Some(ReadGrant {
            range,
            end_abs,
            reader: self,
        })
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // Free this reader's slot for reuse. The node stays linked (the list is
        // grow-only), so this is a single wait-free store with no relinking and
        // no refcount surgery — a later `Readers::push` reclaims it.
        self.node.cursor.store(FREE_SLOT, Ordering::Release);
    }
}

pub struct ReadGrant<'a> {
    /// Physical byte range within `ringbuf`.
    range: Range<usize>,
    /// Absolute cursor position once this grant is dropped.
    end_abs: u64,
    reader: &'a mut Reader,
}

impl Deref for ReadGrant<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        // SAFETY: `range` is below `committed`, which we loaded with Acquire, so
        // the writer has finished these bytes and (by backpressure) will not
        // overwrite them while this grant is alive.
        let p = self.reader.core.byte_ptr(self.range.start);
        unsafe { std::slice::from_raw_parts(p as *const u8, self.range.len()) }
    }
}

impl Drop for ReadGrant<'_> {
    fn drop(&mut self) {
        self.reader
            .node
            .cursor
            .store(self.end_abs, Ordering::Release);
    }
}

pub struct Readers(ReadNode);

impl Default for Readers {
    fn default() -> Self {
        Self::new()
    }
}

/// Sentinel cursor value marking a [`ReadNode`] slot as free for reuse. An
/// absolute byte cursor can never reach this (it would mean 16 EiB committed).
const FREE_SLOT: u64 = u64::MAX;

impl Readers {
    pub fn new() -> Self {
        Self(ReadNode {
            cursor: AtomicU64::new(FREE_SLOT),
            next: ArcAtomic::null(),
        })
    }

    pub fn first(&self) -> Option<Arc<ReadNode>> {
        self.0.next.load_ref(Ordering::Acquire)
    }

    /// Register a reader starting at absolute position `cursor`.
    ///
    /// Nodes are never physically unlinked (see [`Reader`]'s drop): a dropped
    /// reader marks its slot [`FREE_SLOT`], and this reuses such a slot with a
    /// single CAS, or prepends a fresh node when none is free. So the list only
    /// ever grows, bounded by the peak number of concurrent readers, and every
    /// node stays alive for the life of the core — which is what lets traversals
    /// upgrade a raw `next` pointer with `load_ref` without a use-after-free
    /// race.
    pub fn push(&self, cursor: u64) -> Arc<ReadNode> {
        // Reuse a free slot: one CAS atomically claims it and sets the start
        // position.
        let mut node = self.first();
        while let Some(n) = node {
            if n
                .cursor
                .compare_exchange(FREE_SLOT, cursor, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return n;
            }
            node = n.next();
        }
        self.prepend(cursor)
    }

    /// Prepend a fresh node with a lock-free Treiber push.
    fn prepend(&self, cursor: u64) -> Arc<ReadNode> {
        let node = Arc::new(ReadNode {
            cursor: AtomicU64::new(cursor),
            next: ArcAtomic::null(),
        });
        let node_ptr = Arc::as_ptr(&node) as *mut ReadNode;
        loop {
            let head = self.0.next.ptr.load(Ordering::Acquire);
            // Tentatively adopt the current head as our successor. `node` is not
            // reachable by anyone until the CAS below links it, so this store is
            // private. On success we inherit the list's existing strong count on
            // `head` (the raw CAS moves the pointer without touching refcounts);
            // on failure we drop the borrow and retry without ever owning it.
            node.next.ptr.store(head, Ordering::Relaxed);
            if self
                .0
                .next
                .ptr
                .compare_exchange(head, node_ptr, Ordering::Release, Ordering::Acquire)
                .is_ok()
            {
                // The list now owns one strong count on `node`; we keep the one
                // we return to the caller.
                std::mem::forget(node.clone());
                return node;
            }
            node.next.ptr.store(ptr::null_mut(), Ordering::Relaxed);
        }
    }

    pub fn reader_count(&self) -> usize {
        let mut count = 0;
        let mut cursor = self.first();
        while let Some(node) = cursor {
            if node.cursor.load(Ordering::Relaxed) != FREE_SLOT {
                count += 1;
            }
            cursor = node.next();
        }
        count
    }
}

pub struct ReadNode {
    cursor: AtomicU64,
    next: ArcAtomic<ReadNode>,
}

impl ReadNode {
    pub fn next(&self) -> Option<Arc<ReadNode>> {
        self.next.load_ref(Ordering::Acquire)
    }
}

pub struct ArcAtomic<T> {
    pub(crate) ptr: AtomicPtr<T>,
}

impl<T> ArcAtomic<T> {
    pub fn new(val: T) -> Self {
        let arc = Arc::new(val);
        let ptr = Arc::into_raw(arc);
        let ptr = AtomicPtr::new(ptr as *mut _);
        Self { ptr }
    }

    pub fn null() -> Self {
        Self {
            ptr: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub fn load(self, ordering: Ordering) -> Option<Arc<T>> {
        let ptr = self.ptr.load(ordering);
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { Arc::from_raw(ptr) })
    }

    pub fn load_ref(&self, ordering: Ordering) -> Option<Arc<T>> {
        let ptr = self.ptr.load(ordering);
        if ptr.is_null() {
            return None;
        }
        Some(unsafe {
            Arc::increment_strong_count(ptr);
            Arc::from_raw(ptr)
        })
    }

    pub fn store(&self, arc: Arc<T>, ordering: Ordering) {
        let ptr = Arc::into_raw(arc);
        let old = self.ptr.swap(ptr as *mut _, ordering);
        if old.is_null() {
            return;
        }
        unsafe {
            Arc::decrement_strong_count(old);
        }
    }

    pub fn swap(&self, arc: Arc<T>, ordering: Ordering) -> Option<Arc<T>> {
        let ptr = Arc::into_raw(arc);
        let old = self.ptr.swap(ptr as *mut _, ordering);
        if old.is_null() {
            None
        } else {
            Some(unsafe { Arc::from_raw(old) })
        }
    }

    /// Swap the pointer to null and take ownership of the stored value.
    /// Only sound while the caller has exclusive access (e.g. in `Drop`).
    pub(crate) fn take(&self, ordering: Ordering) -> Option<Arc<T>> {
        let old = self.ptr.swap(ptr::null_mut(), ordering);
        if old.is_null() {
            None
        } else {
            Some(unsafe { Arc::from_raw(old) })
        }
    }
}

impl<T> From<Arc<T>> for ArcAtomic<T> {
    fn from(arc: Arc<T>) -> Self {
        let ptr = Arc::into_raw(arc);
        Self {
            ptr: AtomicPtr::new(ptr as *mut _),
        }
    }
}

impl<T> Drop for ArcAtomic<T> {
    fn drop(&mut self) {
        let ptr = self.ptr.load(Ordering::Acquire);
        if !ptr.is_null() {
            unsafe {
                Arc::decrement_strong_count(ptr);
            }
        }
    }
}

impl<T> Clone for ArcAtomic<T> {
    fn clone(&self) -> Self {
        let ptr = self.ptr.load(Ordering::Acquire);
        if !ptr.is_null() {
            unsafe {
                Arc::increment_strong_count(ptr);
            }
        }
        Self {
            ptr: AtomicPtr::new(ptr),
        }
    }
}

#[derive(Debug, Clone)]
pub enum DisruptorError {
    WouldBlock,
    InsufficientCapacity,
}

#[cfg(test)]
mod tests {

    use super::*;

    #[cfg(not(miri))] // async runtime drives kqueue/io_uring, unsupported by Miri
    #[stellarator::test]
    async fn test_single_reader_writer() {
        let disruptor = Disruptor::new(1024);
        let mut reader = disruptor.reader();
        let mut write = disruptor.try_grant(11).unwrap();
        write.copy_from_slice(b"hello world");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"hello world");
        }
        let mut write = disruptor.try_grant(3).unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"foo");
        }
    }

    #[cfg(not(miri))] // async runtime drives kqueue/io_uring, unsupported by Miri
    #[stellarator::test]
    async fn test_multiple_reader_single_writer() {
        let disruptor = Disruptor::new(1024);
        let mut a = disruptor.reader();
        let mut b = disruptor.reader();
        let mut write = disruptor.try_grant(11).unwrap();
        write.copy_from_slice(b"hello world");
        drop(write);
        {
            let grant = a.next().await;
            assert_eq!(&grant[..], b"hello world");
        }
        let mut write = disruptor.try_grant(3).unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = a.next().await;
            assert_eq!(&grant[..], b"foo");
        }

        let grant = b.next().await;
        assert_eq!(&grant[..], b"hello worldfoo");
    }

    #[cfg(not(miri))] // async runtime drives kqueue/io_uring, unsupported by Miri
    #[stellarator::test]
    async fn test_single_reader_writer_wrap() {
        let disruptor = Disruptor::new(12);
        let mut reader = disruptor.reader();
        let mut write = disruptor.try_grant(11).unwrap();
        write.copy_from_slice(b"hello world");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"hello world");
        }
        let mut write = disruptor.try_grant(3).unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"foo");
        }

        let mut write = disruptor.try_grant(3).unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"foo");
        }
    }

    #[cfg(not(miri))] // async runtime drives kqueue/io_uring, unsupported by Miri
    #[stellarator::test]
    async fn test_multiple_reader_multiple_writer_wrap() {
        let disruptor = Disruptor::new(12);
        let mut a = disruptor.reader();
        let mut b = disruptor.reader();
        let mut write = disruptor.try_grant(11).unwrap();
        write.copy_from_slice(b"hello world");
        drop(write);
        {
            let grant = a.next().await;
            assert_eq!(&grant[..], b"hello world");
        }
        {
            let grant = b.next().await;
            assert_eq!(&grant[..], b"hello world");
        }
        let mut write = disruptor.try_grant(3).unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = a.next().await;
            assert_eq!(&grant[..], b"foo");
        }
    }

    // ----- Miri-runnable tests -----
    //
    // These avoid the async runtime (which drives io_uring and cannot run under
    // Miri) by using only the synchronous `try_grant`/`try_next` APIs and
    // `std::thread`. They exercise the unsafe pointer/refcount paths so Miri can
    // check provenance, leaks, and data races. See `libs/db/MIRI.md`.

    use std::thread;

    /// Drives `WriteGrant::deref_mut` (the `ringbuf.as_ptr() as *mut u8` write)
    /// and the full commit/read cycle without the async runtime.
    #[test]
    fn write_grant_roundtrip_sync() {
        let disruptor = Disruptor::new(1024);
        let mut reader = disruptor.reader();

        let mut write = disruptor.try_grant(11).unwrap();
        write.copy_from_slice(b"hello world");
        drop(write);
        {
            let grant = reader.try_next().expect("data committed");
            assert_eq!(&grant[..], b"hello world");
        }
        assert!(reader.try_next().is_none(), "nothing left to read");

        let mut write = disruptor.try_grant(3).unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = reader.try_next().expect("data committed");
            assert_eq!(&grant[..], b"foo");
        }
    }

    /// Forces the wrap-around + `high_water_mark` reset path with only sync APIs.
    #[test]
    fn wraparound_sync() {
        let disruptor = Disruptor::new(12);
        let mut reader = disruptor.reader();

        let mut write = disruptor.try_grant(11).unwrap();
        write.copy_from_slice(b"hello world");
        drop(write);
        assert_eq!(&reader.try_next().expect("first")[..], b"hello world");

        // 11 + 3 > 12, so this write wraps back to the start of the ring.
        let mut write = disruptor.try_grant(3).unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        assert_eq!(&reader.try_next().expect("wrapped")[..], b"foo");

        let mut write = disruptor.try_grant(3).unwrap();
        write.copy_from_slice(b"bar");
        drop(write);
        assert_eq!(&reader.try_next().expect("after wrap")[..], b"bar");
    }

    /// Exercises every `ArcAtomic` op that the disruptor relies on. There are no
    /// assertions about leaks here on purpose: Miri's end-of-run leak check is
    /// the oracle, and it fails the run if any manual strong-count is imbalanced.
    #[test]
    fn arc_atomic_refcount_balance() {
        let a = ArcAtomic::new(5u32);

        // load_ref hands out independent owners of the same allocation.
        let r1 = a.load_ref(Ordering::Acquire).unwrap();
        let r2 = a.load_ref(Ordering::Acquire).unwrap();
        assert!(Arc::ptr_eq(&r1, &r2));
        assert_eq!(*r1, 5);
        drop(r1);
        drop(r2);

        // clone duplicates the slot but shares the pointee.
        let b = a.clone();
        assert!(Arc::ptr_eq(
            &a.load_ref(Ordering::Acquire).unwrap(),
            &b.load_ref(Ordering::Acquire).unwrap(),
        ));

        // store replaces and must drop the previous count.
        a.store(Arc::new(7u32), Ordering::AcqRel);
        assert_eq!(*a.load_ref(Ordering::Acquire).unwrap(), 7);

        // swap returns the displaced owner.
        let old = a.swap(Arc::new(9u32), Ordering::AcqRel).unwrap();
        assert_eq!(*old, 7);
        drop(old);

        // take empties the slot and yields the last owner.
        let taken = a.take(Ordering::AcqRel).unwrap();
        assert_eq!(*taken, 9);
        drop(taken);
        assert!(a.load_ref(Ordering::Acquire).is_none());

        // The clone still holds the original value, proving store/swap on `a`
        // did not disturb its sibling.
        assert_eq!(*b.load_ref(Ordering::Acquire).unwrap(), 5);

        // From<Arc> adopts an existing count without leaking.
        let c = ArcAtomic::from(Arc::new(13u32));
        assert_eq!(*c.load_ref(Ordering::Acquire).unwrap(), 13);
    }

    /// Creates and drops `Reader`s in varied orders to exercise the intrusive
    /// `Readers` list: `Readers::push` (slot reuse / Treiber prepend) and
    /// `Reader::drop` (slot free). `reader_count` counts only live slots, so it
    /// must track create/drop even though freed nodes linger for reuse.
    /// Leak/use-after-free are caught by Miri.
    #[test]
    fn reader_create_drop_unlinks() {
        let disruptor = Disruptor::new(64);
        assert_eq!(disruptor.reader_count(), 0);

        let a = disruptor.reader();
        let b = disruptor.reader();
        let c = disruptor.reader();
        assert_eq!(disruptor.reader_count(), 3);

        // Drop the middle one: its slot is freed (and later reusable).
        drop(b);
        assert_eq!(disruptor.reader_count(), 2);

        // Drop the newest one.
        drop(c);
        assert_eq!(disruptor.reader_count(), 1);

        // Drop the last live one.
        drop(a);
        assert_eq!(disruptor.reader_count(), 0);

        // Registering again reuses a freed slot rather than growing the list.
        let d = disruptor.reader();
        assert_eq!(disruptor.reader_count(), 1);
        drop(d);
        assert_eq!(disruptor.reader_count(), 0);
    }

    /// A writer thread and a reader thread sharing a small ring concurrently,
    /// with far more data than capacity so the stream wraps many times. The
    /// reader must reconstruct the full logical byte stream in order. This is
    /// both a correctness regression test (the writer must never lap the reader
    /// — backpressure) and the data-race coverage for Miri (the writer's raw
    /// `deref_mut` store and the reader's `deref` load synchronized only through
    /// the `committed` Release/Acquire handshake). Loop bounds shrink under Miri.
    #[test]
    fn concurrent_writer_reader_wraps() {
        const MSG: usize = 8;
        let messages: u64 = if cfg!(miri) { 64 } else { 20_000 };
        let total = messages as usize * MSG;
        // Small ring → the 8-byte messages wrap the 100-byte buffer constantly
        // (and 100 is not a multiple of 8, so wrap gaps are exercised too).
        let disruptor = Disruptor::new(100);

        let mut reader = disruptor.reader();
        let consumer = thread::spawn(move || {
            let mut got: Vec<u8> = Vec::with_capacity(total);
            while got.len() < total {
                if let Some(grant) = reader.try_next() {
                    got.extend_from_slice(&grant);
                } else {
                    thread::yield_now();
                }
            }
            got
        });

        let producer = {
            let disruptor = disruptor.clone();
            thread::spawn(move || {
                for i in 0..messages {
                    loop {
                        match disruptor.try_grant(MSG) {
                            Ok(mut grant) => {
                                grant.copy_from_slice(&i.to_le_bytes());
                                break;
                            }
                            Err(DisruptorError::WouldBlock) => thread::yield_now(),
                            Err(e) => panic!("unexpected grant error: {e:?}"),
                        }
                    }
                }
            })
        };

        producer.join().unwrap();
        let expected: Vec<u8> = (0..messages).flat_map(|i| i.to_le_bytes()).collect();
        assert_eq!(consumer.join().unwrap(), expected, "reader lost/reordered data");
    }

    /// Single writer, multiple readers, wrapping constantly. Each reader
    /// independently reconstructs the full stream. This is the case the old
    /// position-based implementation got wrong (a lagging reader was lapped and
    /// silently lost a lap of data); with absolute cursors + correct
    /// backpressure every reader must see every byte. Bounds shrink under Miri.
    #[test]
    fn concurrent_multi_reader_wrap() {
        const MSG: usize = 8;
        let messages: u64 = if cfg!(miri) { 48 } else { 8_000 };
        let reader_count = if cfg!(miri) { 2 } else { 4 };
        let total = messages as usize * MSG;
        let disruptor = Disruptor::new(100);

        let readers: Vec<_> = (0..reader_count)
            .map(|_| {
                let mut reader = disruptor.reader();
                thread::spawn(move || {
                    let mut got: Vec<u8> = Vec::with_capacity(total);
                    while got.len() < total {
                        if let Some(grant) = reader.try_next() {
                            got.extend_from_slice(&grant);
                        } else {
                            thread::yield_now();
                        }
                    }
                    got
                })
            })
            .collect();

        let producer = {
            let disruptor = disruptor.clone();
            thread::spawn(move || {
                for i in 0..messages {
                    loop {
                        match disruptor.try_grant(MSG) {
                            Ok(mut grant) => {
                                grant.copy_from_slice(&i.to_le_bytes());
                                break;
                            }
                            Err(DisruptorError::WouldBlock) => thread::yield_now(),
                            Err(e) => panic!("unexpected grant error: {e:?}"),
                        }
                    }
                }
            })
        };

        producer.join().unwrap();
        let expected: Vec<u8> = (0..messages).flat_map(|i| i.to_le_bytes()).collect();
        for reader in readers {
            assert_eq!(reader.join().unwrap(), expected, "a reader lost/reordered data");
        }
    }

    /// Hammer reader registration/teardown from several threads while a writer
    /// runs. This is what stresses the lock-free `Readers` list itself: the
    /// Treiber `push` (concurrent prepend, must not lose nodes), slot reuse
    /// (concurrent CAS claims), and the wait-free slot free in `Reader::drop`.
    /// At the end every slot must be free again (`reader_count() == 0`) and the
    /// list still usable. Miri additionally checks there are no leaks or races.
    #[test]
    fn concurrent_reader_churn() {
        let churn_threads = if cfg!(miri) { 3 } else { 8 };
        let rounds = if cfg!(miri) { 4 } else { 200 };
        let disruptor = Disruptor::new(128);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // A writer keeps committing so registrations land at varied positions
        // and `slowest_cursor` traverses a live, changing list.
        let writer = {
            let disruptor = disruptor.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(mut grant) = disruptor.try_grant(8) {
                        grant.copy_from_slice(&i.to_le_bytes());
                        i = i.wrapping_add(1);
                    }
                    thread::yield_now();
                }
            })
        };

        let churners: Vec<_> = (0..churn_threads)
            .map(|_| {
                let disruptor = disruptor.clone();
                thread::spawn(move || {
                    for _ in 0..rounds {
                        let mut reader = disruptor.reader();
                        // Drain whatever is available, then drop (frees the slot).
                        for _ in 0..3 {
                            let _ = reader.try_next();
                        }
                    }
                })
            })
            .collect();

        for c in churners {
            c.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        // Every churned reader is gone, so all slots must read as free.
        assert_eq!(disruptor.reader_count(), 0);
        // And the list is still usable.
        let r = disruptor.reader();
        assert_eq!(disruptor.reader_count(), 1);
        drop(r);
        assert_eq!(disruptor.reader_count(), 0);
    }
}
