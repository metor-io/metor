//! Redlines: conditions that abort a sequence.
//!
//! A sequence's step checks the redlines before each poll of the body.
//! [`redline!`](crate::redline) pins a [`Redline`] inside the body's future
//! and links it into the [`Seq`]'s intrusive list. The list cannot know each
//! redline's condition type, so it links a type-erased [`Node`], which is
//! the first field of every `Redline`. A node's `check` function casts the
//! node back to its `Redline<C>` to call the condition.

use core::cell::Cell;
use core::marker::PhantomPinned;
use core::ptr::NonNull;

use cordyceps::Linked;
use cordyceps::list::Links;

use super::Seq;

/// A named condition that aborts a sequence when it returns true.
///
/// A `Redline` does nothing until it is pinned and linked into a [`Seq`],
/// usually with [`redline!`](crate::redline). Dropping it unlinks it.
#[repr(C)]
pub struct Redline<C: Fn() -> bool> {
    pub(super) node: Node,
    cond: C,
}

/// The type-erased part of a [`Redline`] that a `Seq` links.
pub(crate) struct Node {
    links: Links<Node>,
    pub(super) name: &'static str,
    /// Calls the `cond` of the `Redline` that contains this node.
    check: unsafe fn(NonNull<Node>) -> bool,
    /// The `Seq` this node is linked into, if any, so that dropping the node
    /// can unlink it. The `Seq`'s lifetime is erased to `'static`, which is
    /// fine because unlinking never calls a condition.
    seq: Cell<Option<NonNull<Seq<'static>>>>,
    _pin: PhantomPinned,
}

impl<C: Fn() -> bool> Redline<C> {
    /// Creates a redline called `name`.
    ///
    /// Once linked, it trips the first cycle `cond` returns true.
    pub fn new(name: &'static str, cond: C) -> Self {
        Self {
            node: Node {
                links: Links::new(),
                name,
                check: check::<C>,
                seq: Cell::new(None),
                _pin: PhantomPinned,
            },
            cond,
        }
    }
}

/// Calls the condition of the `Redline<C>` that contains `node`.
///
/// # Safety
/// `node` must point to the node of a live `Redline<C>`, and must have been
/// derived from a pointer to the whole redline.
unsafe fn check<C: Fn() -> bool>(node: NonNull<Node>) -> bool {
    // SAFETY: `Redline` is `repr(C)` with `node` as its first field, so the
    // node's address is the redline's address. The caller guarantees the
    // pointer has provenance over the whole redline. Conditions only take
    // shared borrows.
    let redline = unsafe { node.cast::<Redline<C>>().as_ref() };
    (redline.cond)()
}

impl Node {
    /// Marks this node as linked into `seq`.
    ///
    /// Returns `false` without changing anything if the node is already
    /// linked.
    pub(crate) fn claim(&self, seq: &Seq<'_>) -> bool {
        if self.seq.get().is_some() {
            return false;
        }
        self.seq.set(Some(NonNull::from(seq).cast()));
        true
    }

    /// Forgets the `Seq` this node was linked into.
    ///
    /// The `Seq` calls this while it is being dropped.
    pub(crate) fn release(&self) {
        self.seq.set(None);
    }

    /// Returns whether the redline's condition is true.
    ///
    /// # Safety
    /// `node` must be linked. A linked node belongs to a live redline, and
    /// its pointer was derived from a pointer to the whole redline.
    pub(crate) unsafe fn holds(node: NonNull<Node>) -> bool {
        // SAFETY: the caller guarantees `node` is live and points to a whole
        // redline.
        unsafe { (node.as_ref().check)(node) }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let Some(seq) = self.seq.take() else {
            return;
        };
        // SAFETY: a `Seq` releases every node it holds when it is dropped.
        // Only this crate creates a `Seq`, and it never moves one after a
        // node is linked. So if a node still points to a `Seq`, that `Seq`
        // is live.
        let seq = unsafe { seq.as_ref() };
        seq.unlink(NonNull::from(self));
    }
}

// SAFETY: a `Node` stays pinned inside its `Redline` for as long as it is
// linked. The pin guarantee means its `Drop` runs before its memory is
// reused, and that `Drop` unlinks it. Handles are raw pointers, so the list
// owns nothing.
unsafe impl Linked<Links<Node>> for Node {
    type Handle = NonNull<Node>;

    fn into_ptr(node: NonNull<Node>) -> NonNull<Node> {
        node
    }

    unsafe fn from_ptr(ptr: NonNull<Node>) -> NonNull<Node> {
        ptr
    }

    unsafe fn links(ptr: NonNull<Node>) -> NonNull<Links<Node>> {
        // SAFETY: `ptr` points at a live node; `&raw mut` makes no reference.
        unsafe { NonNull::new_unchecked(&raw mut (*ptr.as_ptr()).links) }
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::mem::ManuallyDrop;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};

    use super::*;

    #[test]
    fn test_redline_unlinks_when_its_block_ends() {
        let hot = Cell::new(false);
        let seq = Seq::new();
        {
            crate::redline!(seq, "hot", || hot.get());
            assert_eq!(seq.tripped(), None);
            hot.set(true);
            assert_eq!(seq.tripped(), Some("hot"));
            hot.set(false);
        }
        hot.set(true);
        assert_eq!(seq.tripped(), None);
    }

    #[test]
    fn test_first_holding_redline_in_link_order_trips() {
        let checked = Cell::new(0);
        let check = |holds: bool| {
            checked.set(checked.get() + 1);
            holds
        };
        let seq = Seq::new();
        crate::redline!(seq, "a", || check(false));
        crate::redline!(seq, "b", || check(true));
        crate::redline!(seq, "c", || check(true));
        assert_eq!(seq.tripped(), Some("b"));
        assert_eq!(checked.get(), 2);
    }

    #[test]
    fn test_link_during_walk_is_refused() {
        let late = pin!(Redline::new("late", || true));
        let late = late.as_ref();
        let seq = Seq::new();
        let handle = &raw const seq;
        crate::redline!(seq, "linker", move || {
            // SAFETY: `seq` outlives this redline, which is declared after it.
            unsafe { &*handle }.redline(late);
            false
        });
        assert_eq!(seq.tripped(), None);
        assert_eq!(seq.tripped(), None);
        seq.redline(late);
        assert_eq!(seq.tripped(), Some("late"));
    }

    #[test]
    fn test_second_link_is_ignored() {
        let seq = Seq::new();
        let other = Seq::new();
        let hot = pin!(Redline::new("hot", || true));
        seq.redline(hot.as_ref());
        seq.redline(hot.as_ref());
        other.redline(hot.as_ref());
        assert_eq!(seq.tripped(), Some("hot"));
        assert_eq!(other.tripped(), None);
    }

    #[test]
    fn test_leaked_frame_leaves_no_dangling_node() {
        let seq = Seq::new();
        let mut frame = ManuallyDrop::new(Box::pin(Redline::new("leaked", || true)));
        seq.redline(frame.as_ref());
        assert_eq!(seq.tripped(), Some("leaked"));
        drop(seq);
        // SAFETY: dropped once, after its `Seq`, which released it.
        unsafe { ManuallyDrop::drop(&mut frame) };
    }

    #[test]
    fn test_redline_in_a_suspended_body() {
        let hot = Cell::new(false);
        let seq = Seq::new();
        let mut body = Box::pin(async {
            crate::redline!(seq, "hot", || hot.get());
            seq.next_cycle().await;
            seq.next_cycle().await;
        });
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(body.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(seq.tripped(), None);
        hot.set(true);
        assert_eq!(seq.tripped(), Some("hot"));
        assert_eq!(body.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(seq.tripped(), Some("hot"));
        drop(body);
        assert_eq!(seq.tripped(), None);
    }
}
