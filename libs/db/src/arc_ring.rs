use std::{
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::Deref,
    sync::{Arc, atomic::Ordering},
};

use crate::disruptor::ArcAtomic;

pub struct ArcProj<A, T: ?Sized, F = for<'a> fn(&'a A) -> &'a T> {
    arc: Arc<A>,
    phantom: PhantomData<T>,
    proj: F,
}

impl<A, T: ?Sized, F: Fn(&A) -> &T> std::ops::Deref for ArcProj<A, T, F> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        (self.proj)(&*self.arc)
    }
}

pub struct AtomicStack<T> {
    head: ArcAtomic<AtomicNode<T>>,
}

impl<T> Default for AtomicStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> AtomicStack<T> {
    pub fn new() -> Self {
        Self {
            head: ArcAtomic::null(),
        }
    }

    pub fn push(&self, val: T) {
        let new_node = ManuallyDrop::new(Arc::new(AtomicNode {
            value: val,
            prev: ArcAtomic::null(),
        }));

        loop {
            let head_ptr = self.head.ptr.load(Ordering::Acquire);
            new_node.prev.ptr.store(head_ptr, Ordering::Relaxed);
            if self
                .head
                .ptr
                .compare_exchange_weak(
                    head_ptr,
                    Arc::as_ptr(&new_node) as *mut _,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                break;
            }
        }
    }

    pub fn try_push(&self, val: T) -> Result<(), T> {
        let new_node = ManuallyDrop::new(Arc::new(AtomicNode {
            value: val,
            prev: ArcAtomic::null(),
        }));

        let head_ptr = self.head.ptr.load(Ordering::Acquire);
        new_node.prev.ptr.store(head_ptr, Ordering::Relaxed);
        match self.head.ptr.compare_exchange_weak(
            head_ptr,
            Arc::as_ptr(&new_node) as *mut _,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(_) => {
                println!("somoene else pushed to head");
                let node = Arc::into_inner(ManuallyDrop::into_inner(new_node))
                    .expect("we are the only ones meant to have this");
                Err(node.value)
            }
        }
    }

    pub fn iter(&self) -> AtomicStackIter<T> {
        AtomicStackIter {
            cursor: self.head.clone(),
        }
    }

    pub fn head(&self) -> Option<Arc<AtomicNode<T>>> {
        self.head.load_ref(Ordering::Acquire)
    }
}

pub struct AtomicNode<T> {
    value: T,
    prev: ArcAtomic<AtomicNode<T>>,
}

impl<T: std::fmt::Debug> std::fmt::Debug for AtomicNode<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtomicNode")
            .field("value", &self.value)
            .finish()
    }
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

pub struct AtomicStackIter<T> {
    cursor: ArcAtomic<AtomicNode<T>>,
}

impl<T> AtomicStackIter<T> {
    pub fn new(cursor: ArcAtomic<AtomicNode<T>>) -> Self {
        Self { cursor }
    }
}

impl<T> Iterator for AtomicStackIter<T> {
    type Item = Arc<AtomicNode<T>>;

    fn next(&mut self) -> Option<Self::Item> {
        let cursor = self.cursor.load_ref(Ordering::Acquire)?;
        self.cursor = cursor.prev.clone();
        Some(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_simple() {
        let stack = AtomicStack::new();
        for i in 0..10 {
            stack.push(i);
        }
        let iter = stack.iter();
        assert_eq!(
            iter.map(|n| *n.value()).collect::<Vec<_>>(),
            vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0]
        );
    }
}
