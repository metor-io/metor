use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use crate::disruptor::ArcAtomic;

pub struct AtomicRing<T> {
    head: AtomicUsize,
    buf: Vec<ArcAtomic<T>>,
}

impl<T> AtomicRing<T> {
    pub fn new(capacity: usize) -> Self {
        let buf = (0..capacity).map(|_| ArcAtomic::null()).collect();
        AtomicRing {
            buf,
            head: AtomicUsize::new(0),
        }
    }

    pub fn push(&self, value: Arc<T>) {
        let index = self.head.fetch_add(1, Ordering::Relaxed) % self.buf.len();
        self.buf[index].store(value, Ordering::Relaxed);
    }

    pub fn iter(&self) -> impl Iterator<Item = Arc<T>> {
        let head = self.head.load(Ordering::Acquire) % self.buf.len();
        let (end, start) = self.buf.split_at(head);
        start
            .iter()
            .chain(end.iter())
            .filter_map(|arc| arc.load_ref(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_simple() {
        let ring = AtomicRing::new(10);
        for i in 0..10 {
            ring.push(Arc::new(i));
            assert_eq!(&*ring.iter().next().unwrap(), &0);
        }
        for i in 1..10 {
            ring.push(Arc::new(i));
            assert_eq!(&*ring.iter().next().unwrap(), &i);
        }
        ring.push(Arc::new(0));
        assert_eq!(&*ring.iter().next().unwrap(), &1);
    }
}
