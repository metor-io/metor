use std::{
    marker::PhantomData,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicPtr, AtomicUsize, Ordering},
    },
};

use crate::disruptor::ArcAtomic;

#[derive(Clone)]
pub struct AtomicRing<T> {
    head: Arc<AtomicUsize>,
    buf: Box<[ArcAtomic<T>]>,
}

impl<T> AtomicRing<T> {
    pub fn new(capacity: usize) -> Self {
        let buf = (0..capacity).map(|_| ArcAtomic::null()).collect();
        AtomicRing {
            buf,
            head: Arc::new(AtomicUsize::new(0)),
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

pub struct ArcProj<A, T: ?Sized, F = for<'a> fn(&'a A) -> &'a T> {
    arc: Arc<A>,
    phantom: PhantomData<T>,
    proj: F,
}

impl<A, T: ?Sized, F: Fn(&A) -> &T> std::ops::Deref for ArcProj<A, T, F> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        (&self.proj)(&*self.arc)
    }
}

pub trait ArcProjExt<A> {
    fn proj<T: ?Sized, F: Fn(&A) -> &T>(self, proj: F) -> ArcProj<A, T, F>;
    fn proj_fn<T: ?Sized>(self, proj: for<'a> fn(&'a A) -> &'a T) -> ArcProj<A, T>;
}

impl<A> ArcProjExt<A> for Arc<A> {
    fn proj<T: ?Sized, F: Fn(&A) -> &T>(self, proj: F) -> ArcProj<A, T, F> {
        ArcProj {
            arc: self,
            phantom: PhantomData,
            proj,
        }
    }

    fn proj_fn<T: ?Sized>(self, proj: for<'a> fn(&'a A) -> &'a T) -> ArcProj<A, T> {
        ArcProj {
            arc: self,
            phantom: PhantomData,
            proj,
        }
    }
}

pub struct AtomicList<T> {
    head: ArcAtomic<AtomicNode<T>>,
}

impl<T> AtomicList<T> {
    pub fn new() -> Self {
        Self {
            head: ArcAtomic::null(),
        }
    }

    pub fn iter(&self) -> AtomicListIter<T> {
        AtomicListIter {
            cursor: self.head.clone(),
        }
    }
}

impl<T> Clone for AtomicList<T> {
    fn clone(&self) -> Self {
        Self {
            head: self.head.clone(),
        }
    }
}

pub struct AtomicNode<T> {
    value: T,
    next: ArcAtomic<AtomicNode<T>>,
}

impl<T> AtomicNode<T> {
    pub fn value(&self) -> &T {
        &self.value
    }
}

impl<T> Deref for AtomicNode<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

pub struct AtomicListIter<T> {
    cursor: ArcAtomic<AtomicNode<T>>,
}

impl<T> Iterator for AtomicListIter<T> {
    type Item = Arc<AtomicNode<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        let cursor = self.cursor.load_ref(Ordering::Acquire)?;
        self.cursor = cursor.next.clone();
        Some(cursor)
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
