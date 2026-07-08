use std::{
    mem::ManuallyDrop,
    ops::Deref,
    sync::{Arc, RwLock, atomic::Ordering},
};

use crate::disruptor::ArcAtomic;

/// A lock-free-read linked list of immutable nodes, newest first.
///
/// Safety model: every pointer in the chain owns one strong count, and a
/// published node's `prev` is never mutated. `push` only *transfers* the
/// head's count into the new node's `prev`, so it never decrements and can
/// run lock-free against readers. Structural edits ([`Self::unlink`],
/// [`Self::insert_older`]) are the only operations that release a count a
/// reader might be acquiring; they rebuild fresh shells for nodes newer
/// than the edit point (sharing everything older) and swap `head` under
/// the write half of `lock`, while [`Self::head`]/[`Self::iter`] acquire
/// their `Arc` under the read half. Readers mid-iteration keep the old
/// spine alive through ordinary `Arc` counts and finish on a consistent
/// snapshot.
pub struct AtomicStack<T> {
    head: ArcAtomic<AtomicNode<T>>,
    lock: RwLock<()>,
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
            lock: RwLock::new(()),
        }
    }

    pub fn push(&self, val: T) {
        let new_node = ManuallyDrop::new(Arc::new(AtomicNode {
            value: val,
            prev: PrevLink(ArcAtomic::null()),
        }));

        loop {
            let head_ptr = self.head.ptr.load(Ordering::Acquire);
            new_node.prev.0.ptr.store(head_ptr, Ordering::Relaxed);
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
            prev: PrevLink(ArcAtomic::null()),
        }));

        let head_ptr = self.head.ptr.load(Ordering::Acquire);
        new_node.prev.0.ptr.store(head_ptr, Ordering::Relaxed);
        match self.head.ptr.compare_exchange_weak(
            head_ptr,
            Arc::as_ptr(&new_node) as *mut _,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => Ok(()),
            Err(_) => {
                let new_node = ManuallyDrop::into_inner(new_node);
                // The head's count only becomes ours on CAS success; clear
                // the borrowed pointer so the drop below doesn't release a
                // count the stack still owns.
                new_node
                    .prev
                    .0
                    .ptr
                    .store(std::ptr::null_mut(), Ordering::Relaxed);
                let node = Arc::into_inner(new_node)
                    .expect("we are the only ones meant to have this");
                Err(node.value)
            }
        }
    }

    pub fn iter(&self) -> AtomicStackIter<T> {
        let _guard = self.lock.read().unwrap();
        AtomicStackIter {
            cursor: self.head.clone(),
        }
    }

    pub fn head(&self) -> Option<Arc<AtomicNode<T>>> {
        let _guard = self.lock.read().unwrap();
        self.head.load_ref(Ordering::Acquire)
    }
}

impl<T: Clone> AtomicStack<T> {
    /// Remove `victim` from the stack. Returns false if it is no longer
    /// reachable from the head.
    ///
    /// Shells newer than `victim` are rebuilt, so their `Arc` identity
    /// changes; nodes older than `victim` are shared untouched. Callers
    /// must not unlink the node a writer is appending to — the writer
    /// would keep writing into the detached node's storage.
    pub fn unlink(&self, victim: &Arc<AtomicNode<T>>) -> bool {
        self.splice(victim, SpliceKind::Remove)
    }

    /// Insert `value` directly older than `succ`. Returns false if `succ`
    /// is no longer reachable from the head.
    ///
    /// `succ` and everything newer are rebuilt as fresh shells; nodes
    /// older than `succ` are shared untouched.
    pub fn insert_older(&self, succ: &Arc<AtomicNode<T>>, value: T) -> bool {
        self.splice(succ, SpliceKind::InsertOlder(value))
    }

    fn splice(&self, boundary: &Arc<AtomicNode<T>>, kind: SpliceKind<T>) -> bool {
        let _guard = self.lock.write().unwrap();
        let boundary_ptr = Arc::as_ptr(boundary);
        loop {
            let head_ptr = self.head.ptr.load(Ordering::Acquire);

            // Collect the prefix strictly newer than `boundary`. Under the
            // write lock the only concurrent mutator is `push`, which never
            // decrements, so every pointer in the chain stays valid.
            let mut prefix: Vec<Arc<AtomicNode<T>>> = Vec::new();
            let mut cursor_ptr = head_ptr;
            let mut found = false;
            while !cursor_ptr.is_null() {
                if std::ptr::eq(cursor_ptr, boundary_ptr) {
                    found = true;
                    break;
                }
                let cursor = unsafe {
                    Arc::increment_strong_count(cursor_ptr);
                    Arc::from_raw(cursor_ptr)
                };
                cursor_ptr = cursor.prev.0.ptr.load(Ordering::Acquire);
                prefix.push(cursor);
            }
            if !found {
                return false;
            }

            // The shared tail the new spine hangs off: for a removal the
            // boundary's elder, for an insertion a fresh node spliced in
            // above that elder.
            let mut below: Option<Arc<AtomicNode<T>>> = boundary.prev.0.load_ref(Ordering::Acquire);
            match &kind {
                SpliceKind::Remove => {}
                SpliceKind::InsertOlder(value) => {
                    below = Some(Arc::new(AtomicNode {
                        value: value.clone(),
                        prev: PrevLink(below.map(ArcAtomic::from).unwrap_or_else(ArcAtomic::null)),
                    }));
                    below = Some(Arc::new(AtomicNode {
                        value: boundary.value.clone(),
                        prev: PrevLink(below.map(ArcAtomic::from).unwrap_or_else(ArcAtomic::null)),
                    }));
                }
            }
            for node in prefix.iter().rev() {
                below = Some(Arc::new(AtomicNode {
                    value: node.value.clone(),
                    prev: PrevLink(below.map(ArcAtomic::from).unwrap_or_else(ArcAtomic::null)),
                }));
            }

            let new_head_ptr = below
                .as_ref()
                .map(Arc::as_ptr)
                .unwrap_or(std::ptr::null());
            if self
                .head
                .ptr
                .compare_exchange(
                    head_ptr,
                    new_head_ptr as *mut _,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                // The stack now owns the new spine's count; release ours on
                // the old head, which nothing references anymore.
                std::mem::forget(below);
                if !head_ptr.is_null() {
                    drop(unsafe { Arc::from_raw(head_ptr) });
                }
                return true;
            }
            // A concurrent push won the head; rebuild including its nodes.
        }
    }
}

enum SpliceKind<T> {
    Remove,
    InsertOlder(T),
}

pub struct AtomicNode<T> {
    value: T,
    prev: PrevLink<T>,
}

/// The chain link, with an iterative teardown: naive recursion through
/// `prev` on drop would overflow the stack on long chains (every spine
/// release recurses chain-length deep). A field newtype rather than
/// `Drop for AtomicNode` so `value` can still be moved out of a node.
struct PrevLink<T>(ArcAtomic<AtomicNode<T>>);

impl<T> Drop for PrevLink<T> {
    fn drop(&mut self) {
        let mut cursor = self.0.take(Ordering::Acquire);
        while let Some(node) = cursor {
            match Arc::try_unwrap(node) {
                Ok(inner) => cursor = inner.prev.0.take(Ordering::Acquire),
                // A reader or newer spine still owns the rest of the
                // chain; it finishes the teardown when it lets go.
                Err(_) => break,
            }
        }
    }
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
        self.cursor = cursor.prev.0.clone();
        Some(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(stack: &AtomicStack<i32>) -> Vec<i32> {
        stack.iter().map(|n| *n.value()).collect()
    }

    fn node_at(stack: &AtomicStack<i32>, value: i32) -> Arc<AtomicNode<i32>> {
        stack.iter().find(|n| *n.value() == value).unwrap()
    }

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

    #[test]
    fn unlink_each_position() {
        for victim in 0..4 {
            let stack = AtomicStack::new();
            for i in 0..4 {
                stack.push(i);
            }
            let node = node_at(&stack, victim);
            assert!(stack.unlink(&node));
            let expected: Vec<i32> = (0..4).rev().filter(|v| *v != victim).collect();
            assert_eq!(contents(&stack), expected);
            // A second unlink of the same node finds nothing.
            assert!(!stack.unlink(&node));
        }
    }

    #[test]
    fn unlink_only_node_empties_stack() {
        let stack = AtomicStack::new();
        stack.push(7);
        let node = node_at(&stack, 7);
        assert!(stack.unlink(&node));
        assert_eq!(contents(&stack), Vec::<i32>::new());
        stack.push(8);
        assert_eq!(contents(&stack), vec![8]);
    }

    #[test]
    fn insert_older_each_position() {
        for succ in 0..4 {
            let stack = AtomicStack::new();
            for i in 0..4 {
                stack.push(i);
            }
            let node = node_at(&stack, succ);
            assert!(stack.insert_older(&node, 100));
            let mut expected: Vec<i32> = (0..4).rev().collect();
            let pos = expected.iter().position(|v| *v == succ).unwrap();
            expected.insert(pos + 1, 100);
            assert_eq!(contents(&stack), expected);
        }
    }

    #[test]
    fn unlink_frees_the_node() {
        let stack = AtomicStack::new();
        for i in 0..3 {
            stack.push(i);
        }
        let node = node_at(&stack, 1);
        let weak = Arc::downgrade(&node);
        assert!(stack.unlink(&node));
        drop(node);
        assert!(weak.upgrade().is_none(), "unlinked node still referenced");
    }

    #[test]
    fn reader_snapshot_survives_unlink() {
        let stack = AtomicStack::new();
        for i in 0..4 {
            stack.push(i);
        }
        let mut iter = stack.iter();
        assert_eq!(*iter.next().unwrap().value(), 3);
        let node = node_at(&stack, 2);
        assert!(stack.unlink(&node));
        // The in-flight iterator still sees the old spine, including the
        // unlinked node; a fresh iterator does not.
        assert_eq!(*iter.next().unwrap().value(), 2);
        assert_eq!(*iter.next().unwrap().value(), 1);
        assert_eq!(contents(&stack), vec![3, 1, 0]);
    }

    /// Pushes a long chain and drops it, exercising `PrevLink`'s *iterative*
    /// teardown (a naive recursive drop would overflow the stack on long
    /// chains). Miri's leak check confirms every node is freed exactly once.
    #[test]
    fn deep_chain_teardown_sync() {
        let len = if cfg!(miri) { 200 } else { 100_000 };
        let stack = AtomicStack::new();
        for i in 0..len {
            stack.push(i);
        }
        assert_eq!(stack.iter().count(), len as usize);
        drop(stack);
    }

    #[test]
    fn concurrent_readers_writer_and_splices() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // Miri is ~50-100x slower, so shrink the workload there while keeping
        // full coverage of the same code paths.
        let initial = if cfg!(miri) { 16 } else { 32 };
        let reader_threads = if cfg!(miri) { 2 } else { 4 };
        let writer_limit = if cfg!(miri) { 1_200 } else { 21_000 };
        let splices = if cfg!(miri) { 40 } else { 500 };

        let stack = Arc::new(AtomicStack::new());
        for i in 0..initial {
            stack.push(i);
        }
        let stop = Arc::new(AtomicBool::new(false));

        let readers: Vec<_> = (0..reader_threads)
            .map(|_| {
                let stack = stack.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut sum = 0i64;
                    while !stop.load(Ordering::Relaxed) {
                        for node in stack.iter() {
                            sum += *node.value() as i64;
                        }
                    }
                    sum
                })
            })
            .collect();

        let writer = {
            let stack = stack.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut next = 1000;
                while !stop.load(Ordering::Relaxed) && next < writer_limit {
                    stack.push(next);
                    next += 1;
                }
            })
        };

        // Splice loop: repeatedly unlink the oldest node and re-insert a
        // replacement above the new oldest, exercising rebuilds while
        // readers and the writer run.
        for _ in 0..splices {
            let Some(oldest) = stack.iter().last() else {
                continue;
            };
            stack.unlink(&oldest);
            if let Some(tail) = stack.iter().last() {
                stack.insert_older(&tail, *oldest.value());
            }
        }
        stop.store(true, Ordering::Relaxed);
        for reader in readers {
            reader.join().unwrap();
        }
        writer.join().unwrap();
        // The stack must still be a valid chain.
        assert!(stack.iter().count() > 0);
    }
}
