//! Redlines: conditions a sequence's step checks before each poll.
//!
//! A [`Redline`] is pinned in the sequence's frame by [`redline!`](crate::redline)
//! and linked into its [`Seq`]'s intrusive list. The list holds type-erased
//! [`Node`] heads; each node's `check` casts back to its `Redline<C>`.

use core::cell::Cell;
use core::marker::PhantomPinned;
use core::ptr::NonNull;

use cordyceps::Linked;
use cordyceps::list::Links;

use super::Seq;

/// One redline, pinned in the sequence's frame and linked into its [`Seq`].
#[repr(C)]
pub struct Redline<C: Fn() -> bool> {
    node: Node,
    cond: C,
}

/// The type-erased head of a [`Redline`], which the list links.
pub(crate) struct Node {
    links: Links<Node>,
    name: &'static str,
    /// Calls the `cond` of the `Redline` this node heads.
    check: unsafe fn(NonNull<Node>) -> bool,
    /// The `Seq` it is linked into, if any, to unlink from on drop. Its
    /// brand is erased; unlinking calls no condition.
    seq: Cell<Option<NonNull<Seq<'static>>>>,
    _pin: PhantomPinned,
}

impl<C: Fn() -> bool> Redline<C> {
    /// A redline named `name` that trips once `cond` holds.
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

    pub(crate) fn node(&self) -> &Node {
        &self.node
    }
}

/// Calls the condition of the `Redline<C>` that `node` heads.
///
/// # Safety
/// `node` heads a live `Redline<C>` and was made from a pointer to all of it.
unsafe fn check<C: Fn() -> bool>(node: NonNull<Node>) -> bool {
    // SAFETY: `Redline` is `repr(C)` with `node` first, so the head's address
    // is the redline's, and the caller gives the pointer the whole redline's
    // provenance. Conditions take only shared borrows.
    let redline = unsafe { node.cast::<Redline<C>>().as_ref() };
    (redline.cond)()
}

impl Node {
    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    /// Links this node into `seq`, unless it is linked already.
    pub(crate) fn claim(&self, seq: &Seq<'_>) -> bool {
        if self.seq.get().is_some() {
            return false;
        }
        self.seq.set(Some(NonNull::from(seq).cast()));
        true
    }

    /// Forgets the `Seq` it was linked into, which is dropping.
    pub(crate) fn release(&self) {
        self.seq.set(None);
    }

    /// Whether the condition of the redline `node` heads holds.
    ///
    /// # Safety
    /// `node` is linked, so it heads a live redline and was made from a
    /// pointer to all of it.
    pub(crate) unsafe fn holds(node: NonNull<Node>) -> bool {
        // SAFETY: the caller vouches `node` is live and whole.
        unsafe { (node.as_ref().check)(node) }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let Some(seq) = self.seq.take() else {
            return;
        };
        // SAFETY: a `Seq` releases every node it holds when it drops, and is
        // built only inside this crate, where it never moves once a node is
        // linked, so a node that still names its `Seq` points at a live one.
        let seq = unsafe { seq.as_ref() };
        seq.unlink(NonNull::from(self));
    }
}

// SAFETY: a `Node` is pinned inside its `Redline` for as long as it is
// linked, since the pin guarantee means its `Drop` runs before its memory is
// reused, and that `Drop` unlinks it. Handles are bare pointers, so the list
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
    fn test_unnamed_redline() {
        let seq = Seq::new();
        crate::redline!(seq, || true);
        assert_eq!(seq.tripped(), Some("redline"));
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
