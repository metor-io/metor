use std::{
    ops::{Deref, DerefMut, Range},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicPtr, AtomicU64, Ordering},
    },
};
use stellarator::sync::{Mutex, MutexGuard, WaitCell, WaitQueue};

#[derive(Clone)]
pub struct Disruptor {
    core: Arc<DistruptorCore>,
}

impl Disruptor {
    pub fn new(capacity: usize) -> Self {
        let core = Arc::new(DistruptorCore {
            ringbuf: vec![0; capacity],
            write_head: WriteHead {
                committed: AtomicU64::new(0),
                high_water_mark: AtomicU64::new(0),
                write_lock: std::sync::Mutex::new(()),
            },
            readers: Readers::new(),
            buffer_full_cell: WaitCell::new(),
            new_data_queue: WaitQueue::new(),
        });
        Self { core }
    }

    pub async fn grant(&self, len: usize) -> Result<WriteGrant<'_>, DisruptorError> {
        let Disruptor { core } = self;
        let _lock_guard = core.write_head.write_lock.lock().expect("poisoned lock");
        let mut write = (core.write_head.committed.load(Ordering::Acquire)
            % core.ringbuf.len() as u64) as usize;
        let max = core.ringbuf.len();
        if len > max {
            return Err(DisruptorError::InsufficientCapacity);
        }
        let _ = core
            .buffer_full_cell
            .wait_for(|| can_write(core, len, write, max))
            .await;

        if write + len > max {
            write = 0;
            core.write_head
                .high_water_mark
                .store(write as u64, Ordering::Release);
        } else if write + len > core.write_head.high_water_mark.load(Ordering::Acquire) as usize {
            core.write_head
                .high_water_mark
                .store((write + len) as u64, Ordering::Release);
        }

        Ok(WriteGrant {
            range: write..write + len,
            disruptor: self.core.as_ref(),
            _lock_guard,
        })
    }

    pub fn try_grant(&self, len: usize) -> Result<WriteGrant<'_>, DisruptorError> {
        let Disruptor { core } = self;
        let mut write = (core.write_head.committed.load(Ordering::Acquire)
            % core.ringbuf.len() as u64) as usize;
        let _lock_guard = core.write_head.write_lock.lock().expect("poisoned");
        let max = core.ringbuf.len();
        if len > max {
            return Err(DisruptorError::InsufficientCapacity);
        }
        if !can_write(core, len, write, max) {
            return Err(DisruptorError::WouldBlock);
        }

        if write + len > max {
            write = 0;
            core.write_head
                .high_water_mark
                .store(write as u64, Ordering::Release);
        } else if write + len > core.write_head.high_water_mark.load(Ordering::Acquire) as usize {
            core.write_head
                .high_water_mark
                .store((write + len) as u64, Ordering::Release);
        }

        Ok(WriteGrant {
            range: write..write + len,
            disruptor: self.core.as_ref(),
            _lock_guard,
        })
    }

    pub fn reader(&self) -> Reader {
        let write = self.core.write_head.committed.load(Ordering::Acquire);
        let node = self.core.readers.push(AtomicU64::new(write));
        Reader {
            node,
            core: self.core.clone(),
        }
    }
}

pub struct DistruptorCore {
    ringbuf: Vec<u8>,
    write_head: WriteHead,
    readers: Readers,
    buffer_full_cell: WaitCell,
    new_data_queue: WaitQueue,
}

pub fn can_write(core: &DistruptorCore, len: usize, write: usize, max: usize) -> bool {
    let mut buffer_full = false;
    let mut cursor = core.readers.first();
    'check_room: while let Some(node) = cursor {
        cursor = node.next();
        let read = (node.cursor.load(Ordering::Acquire) % core.ringbuf.len() as u64) as usize;
        // based on logic in https://github.com/jamesmunns/bbqueue/blob/8468029832ce2293cd93f8af10b7372be3c96ad0/core/src/bbbuffer.rs#L365C1-L397C1
        //
        // check the case where we are inverted, and the new write will overlap with a read
        if write < read && write + len >= read {
            buffer_full = true;
            break 'check_room;
        // check the case where we are not inverted,
        // but the next write will casue an inversion,
        // and that inversion will lead to an overlap with a read
        } else if write + len > max && len >= read {
            buffer_full = true;
            break 'check_room;
        }
    }

    !buffer_full
}

pub struct WriteHead {
    committed: AtomicU64,
    high_water_mark: AtomicU64,
    write_lock: std::sync::Mutex<()>,
}

pub struct WriteGrant<'a> {
    range: Range<usize>,
    disruptor: &'a DistruptorCore,
    _lock_guard: std::sync::MutexGuard<'a, ()>,
}

impl Deref for WriteGrant<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.disruptor.ringbuf[self.range.clone()]
    }
}

impl DerefMut for WriteGrant<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let len = self.range.len();
        let start = self.disruptor.ringbuf.as_ptr() as *mut u8;
        let ptr = unsafe { start.add(self.range.start) };
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }
}

impl Drop for WriteGrant<'_> {
    fn drop(&mut self) {
        let write_head = &self.disruptor.write_head;
        let commited = write_head.committed.load(Ordering::Acquire) as usize;
        let high_water_mark = write_head.high_water_mark.load(Ordering::Acquire) as usize;
        if self.range.end < commited && self.range.end != self.disruptor.ringbuf.len() {
            write_head
                .high_water_mark
                .store(commited as u64, Ordering::Relaxed);
        } else if self.range.end > high_water_mark {
            write_head
                .high_water_mark
                .store(self.disruptor.ringbuf.len() as u64, Ordering::Relaxed);
        }

        write_head
            .committed
            .store(self.range.end as u64, Ordering::Release);
        self.disruptor.new_data_queue.wake_all();
    }
}

pub struct Reader {
    node: Arc<ReadNode>,
    core: Arc<DistruptorCore>,
}

impl Reader {
    pub async fn next(&mut self) -> ReadGrant<'_> {
        let node = self.node.as_ref();
        let range: Range<usize> = self
            .core
            .new_data_queue
            .wait_for_value(|| {
                let mut read = node.cursor.load(Ordering::Acquire);
                let write = self.core.write_head.committed.load(Ordering::Acquire);
                let high_water_mark = self.core.write_head.high_water_mark.load(Ordering::Acquire);
                if read == high_water_mark && write < read {
                    read = 0;
                    node.cursor.store(0, Ordering::Release);
                }
                let len = if write < read { high_water_mark } else { write } - read;
                let len = len as usize;
                let read = read as usize;
                (len > 0).then(|| read..read + len)
            })
            .await
            .expect("queue closed");
        ReadGrant {
            range,
            reader: self,
        }
    }
}

pub struct ReadGrant<'a> {
    range: Range<usize>,
    reader: &'a mut Reader,
}

impl Deref for ReadGrant<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.reader.core.ringbuf[self.range.clone()]
    }
}

impl Drop for ReadGrant<'_> {
    fn drop(&mut self) {
        self.reader
            .node
            .cursor
            .store(self.range.end as u64, Ordering::Release);
    }
}

pub struct Readers {
    first: ArcAtomic<ReadNode>,
}

impl Readers {
    pub fn new() -> Self {
        Self {
            first: ArcAtomic::null(),
        }
    }

    pub fn first(&self) -> Option<Arc<ReadNode>> {
        self.first.load_ref(Ordering::Acquire)
    }

    pub fn push(&self, cursor: AtomicU64) -> Arc<ReadNode> {
        let node = ReadNode {
            cursor,
            next: self.first.clone(),
        };
        let first = Arc::new(node);
        self.first.swap(first.clone(), Ordering::AcqRel);
        first
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
    ptr: AtomicPtr<T>,
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

    #[stellarator::test]
    async fn test_single_reader_writer() {
        let disruptor = Disruptor::new(1024);
        let mut reader = disruptor.reader();
        let mut write = disruptor.grant(11).await.unwrap();
        write.copy_from_slice(b"hello world");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"hello world");
        }
        let mut write = disruptor.grant(3).await.unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"foo");
        }
    }

    #[stellarator::test]
    async fn test_multiple_reader_single_writer() {
        let disruptor = Disruptor::new(1024);
        let mut a = disruptor.reader();
        let mut b = disruptor.reader();
        let mut write = disruptor.grant(11).await.unwrap();
        write.copy_from_slice(b"hello world");
        drop(write);
        {
            let grant = a.next().await;
            assert_eq!(&grant[..], b"hello world");
        }
        let mut write = disruptor.grant(3).await.unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = a.next().await;
            assert_eq!(&grant[..], b"foo");
        }

        let grant = b.next().await;
        assert_eq!(&grant[..], b"hello worldfoo");
    }

    #[stellarator::test]
    async fn test_single_reader_writer_wrap() {
        let disruptor = Disruptor::new(12);
        let mut reader = disruptor.reader();
        let mut write = disruptor.grant(11).await.unwrap();
        write.copy_from_slice(b"hello world");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"hello world");
        }
        let mut write = disruptor.grant(3).await.unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"foo");
        }

        let mut write = disruptor.grant(3).await.unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = reader.next().await;
            assert_eq!(&grant[..], b"foo");
        }
    }

    #[stellarator::test]
    async fn test_multiple_reader_multiple_writer_wrap() {
        let disruptor = Disruptor::new(12);
        let mut a = disruptor.reader();
        let mut b = disruptor.reader();
        let mut write = disruptor.grant(11).await.unwrap();
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
        let mut write = disruptor.grant(3).await.unwrap();
        write.copy_from_slice(b"foo");
        drop(write);
        {
            let grant = a.next().await;
            assert_eq!(&grant[..], b"foo");
        }
    }
}
