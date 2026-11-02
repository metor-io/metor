//! Watches: conditions a sequence steps through an alarm cell each cycle.

use core::cell::Cell;
use core::fmt;
use core::marker::PhantomPinned;
use core::ptr::NonNull;

use cordyceps::Linked;
use cordyceps::list::Links;
use metor_proto::types::Timestamp;

use super::Seq;
use crate::alarm::{AlarmCell, AlarmSpec, Edge, Msg, Severity};

/// What a tripped watch does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnTrip {
    Raise,
    RaiseAbort,
}

/// A condition stepped through an alarm cell each cycle while linked.
///
/// A `Watch` does nothing until it is pinned and linked into a [`Seq`],
/// usually with [`alarm!`](macro@crate::alarm) or [`redline!`](crate::redline).
/// Dropping it unlinks it and clears its alarm.
#[repr(C)]
pub struct Watch<C, F> {
    pub(super) node: Node,
    cond: C,
    fmt: F,
}

/// The type-erased part of a [`Watch`] that a `Seq` links.
pub(crate) struct Node {
    links: Links<Node>,
    pub(super) spec: AlarmSpec,
    pub(super) on_trip: OnTrip,
    pub(super) cell: Cell<AlarmCell>,
    /// Calls `cond` of the `Watch` that holds this node.
    breached: unsafe fn(NonNull<Node>) -> Option<bool>,
    /// Calls `fmt` of the `Watch` that holds this node.
    format: unsafe fn(NonNull<Node>, &mut Msg) -> fmt::Result,
    /// The `Seq` this node is linked into, if any, so that dropping the node
    /// can unlink it. The `Seq`'s lifetime is erased to `'static`, which is
    /// fine because unlinking never calls a condition.
    seq: Cell<Option<NonNull<Seq<'static>>>>,
    _pin: PhantomPinned,
}

/// Writes a watch's message on a raise.
pub trait Format {
    fn format(&self, spec: &AlarmSpec, msg: &mut Msg) -> fmt::Result;
}

impl<F: Fn(&mut Msg) -> fmt::Result> Format for F {
    fn format(&self, _spec: &AlarmSpec, msg: &mut Msg) -> fmt::Result {
        self(msg)
    }
}

/// Gives a format closure its argument type, so `|m| write!(m, ..)` infers.
pub fn formatter<F: Fn(&mut Msg) -> fmt::Result>(f: F) -> F {
    f
}

/// The message is the spec's name.
pub struct SpecName;

impl Format for SpecName {
    fn format(&self, spec: &AlarmSpec, msg: &mut Msg) -> fmt::Result {
        fmt::Write::write_str(msg, spec.id)
    }
}

impl<C, L, F> Watch<C, F>
where
    C: Fn() -> L,
    L: Into<Option<bool>>,
    F: Format,
{
    /// An alarm that raises and clears with `spec`.
    pub fn alarm(spec: &AlarmSpec, cond: C, fmt: F) -> Self {
        Self::new(*spec, OnTrip::Raise, cond, fmt)
    }

    /// A critical alarm called `name` that also aborts the sequence the
    /// first cycle `cond` breaches.
    pub fn redline(name: &'static str, cond: C, fmt: F) -> Self {
        let spec = AlarmSpec {
            id: name,
            severity: Severity::Critical,
            debounce: core::num::NonZeroU16::MIN,
        };
        Self::new(spec, OnTrip::RaiseAbort, cond, fmt)
    }

    fn new(spec: AlarmSpec, on_trip: OnTrip, cond: C, fmt: F) -> Self {
        Self {
            node: Node {
                links: Links::new(),
                spec,
                on_trip,
                cell: Cell::default(),
                breached: breached::<C, L, F>,
                format: format::<C, F>,
                seq: Cell::new(None),
                _pin: PhantomPinned,
            },
            cond,
            fmt,
        }
    }
}

/// Calls the condition of the `Watch<C, F>` that holds `node`.
///
/// # Safety
/// `node` must point to the node of a live `Watch<C, F>`, and must have been
/// derived from a pointer to the whole watch.
unsafe fn breached<C: Fn() -> L, L: Into<Option<bool>>, F>(node: NonNull<Node>) -> Option<bool> {
    // SAFETY: `Watch` is `repr(C)` with `node` as its first field, so the
    // node's address is the watch's address. The caller guarantees the
    // pointer has provenance over the whole watch. Conditions only take
    // shared borrows.
    let watch = unsafe { node.cast::<Watch<C, F>>().as_ref() };
    (watch.cond)().into()
}

/// Calls the format of the `Watch<C, F>` that holds `node`.
///
/// # Safety
/// As for [`breached`].
unsafe fn format<C, F: Format>(node: NonNull<Node>, msg: &mut Msg) -> fmt::Result {
    // SAFETY: as in `breached`.
    let watch = unsafe { node.cast::<Watch<C, F>>().as_ref() };
    watch.fmt.format(&watch.node.spec, msg)
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

    /// Steps the cell with this cycle's condition.
    ///
    /// # Safety
    /// `node` must be linked. A linked node belongs to a live watch, and its
    /// pointer was derived from a pointer to the whole watch.
    pub(crate) unsafe fn step(node: NonNull<Node>, now: Timestamp) -> Option<Edge> {
        // SAFETY: the caller guarantees `node` is live and points to a whole
        // watch.
        let this = unsafe { node.as_ref() };
        let breached = unsafe { (this.breached)(node) };
        let mut cell = this.cell.get();
        let edge = cell.step(&this.spec, breached, now);
        this.cell.set(cell);
        edge
    }

    /// Writes this watch's message.
    ///
    /// # Safety
    /// As for [`Node::step`].
    pub(crate) unsafe fn format(node: NonNull<Node>, msg: &mut Msg) -> fmt::Result {
        // SAFETY: as in `step`.
        unsafe { (node.as_ref().format)(node, msg) }
    }

    /// Resets the cell, and returns its clear if it was active.
    pub(crate) fn disarm(&self) -> Option<Edge> {
        let mut cell = self.cell.get();
        let edge = cell.disarm();
        self.cell.set(cell);
        edge
    }

    /// True for a redline whose alarm is active.
    pub(crate) fn tripped(&self) -> bool {
        self.on_trip == OnTrip::RaiseAbort && self.cell.get().is_active()
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

// SAFETY: a `Node` stays pinned inside its `Watch` for as long as it is
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
    fn test_watch_unlinks_when_its_block_ends() {
        let hot = Cell::new(false);
        let seq = Seq::new();
        {
            crate::redline!(seq, "hot", || hot.get());
            assert_eq!(seq.walk(), None);
            hot.set(true);
            assert_eq!(seq.walk(), Some("hot"));
            hot.set(false);
        }
        hot.set(true);
        assert_eq!(seq.walk(), None);
    }

    #[test]
    fn test_first_tripped_in_link_order_with_every_condition_called() {
        let checked = Cell::new(0);
        let check = |holds: bool| {
            checked.set(checked.get() + 1);
            holds
        };
        let seq = Seq::new();
        crate::redline!(seq, "a", || check(false));
        crate::redline!(seq, "b", || check(true));
        crate::redline!(seq, "c", || check(true));
        assert_eq!(seq.walk(), Some("b"));
        assert_eq!(checked.get(), 3);
    }

    #[test]
    fn test_link_during_walk_is_refused() {
        let late = pin!(Watch::redline("late", || true, SpecName));
        let late = late.as_ref();
        let seq = Seq::new();
        let handle = &raw const seq;
        crate::redline!(seq, "linker", move || {
            // SAFETY: `seq` outlives this watch, which is declared after it.
            unsafe { &*handle }.watch(late);
            false
        });
        assert_eq!(seq.walk(), None);
        assert_eq!(seq.walk(), None);
        seq.watch(late);
        assert_eq!(seq.walk(), Some("late"));
    }

    #[test]
    fn test_second_link_is_ignored() {
        let seq = Seq::new();
        let other = Seq::new();
        let hot = pin!(Watch::redline("hot", || true, SpecName));
        seq.watch(hot.as_ref());
        seq.watch(hot.as_ref());
        other.watch(hot.as_ref());
        assert_eq!(seq.walk(), Some("hot"));
        assert_eq!(other.walk(), None);
    }

    #[test]
    fn test_leaked_frame_leaves_no_dangling_node() {
        let seq = Seq::new();
        let mut frame = ManuallyDrop::new(Box::pin(Watch::redline("leaked", || true, SpecName)));
        seq.watch(frame.as_ref());
        assert_eq!(seq.walk(), Some("leaked"));
        assert!(frame.node.tripped());
        drop(seq);
        assert!(!frame.node.tripped(), "the seq cleared it");
        // SAFETY: dropped once, after its `Seq`, which released it.
        unsafe { ManuallyDrop::drop(&mut frame) };
    }

    #[test]
    fn test_raise_watch_never_trips() {
        const HOT: AlarmSpec = AlarmSpec::warning("hot");
        let seq = Seq::new();
        let watch = pin!(Watch::alarm(&HOT, || true, SpecName));
        seq.watch(watch.as_ref());
        assert_eq!(seq.walk(), None);
        assert!(watch.node.cell.get().is_active());
    }

    #[test]
    fn test_watch_in_a_suspended_body() {
        let hot = Cell::new(false);
        let seq = Seq::new();
        let mut body = Box::pin(async {
            crate::redline!(seq, "hot", || hot.get());
            seq.next_cycle().await;
            seq.next_cycle().await;
        });
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(body.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(seq.walk(), None);
        hot.set(true);
        assert_eq!(seq.walk(), Some("hot"));
        assert_eq!(body.as_mut().poll(&mut cx), Poll::Pending);
        assert_eq!(seq.walk(), Some("hot"));
        drop(body);
        assert_eq!(seq.walk(), None);
    }
}
