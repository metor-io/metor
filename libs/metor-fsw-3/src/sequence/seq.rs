//! The [`Seq`] handle, the wait futures it returns, and abort handlers.

use core::cell::{Cell, UnsafeCell};
use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll};
use core::time::Duration;
use std::borrow::Cow;
use std::panic::{AssertUnwindSafe, catch_unwind};

use cordyceps::List;
use metor_proto::types::Timestamp;

use super::redline::{Node, Redline};
use crate::log::Log;

/// A sequence's handle to its current cycle.
///
/// The body uses it to read the cycle time, check whether the sequence was
/// aborted, wait, and add redlines.
///
/// The lifetime `'a` is invariant, and every redline condition linked into a
/// `Seq<'a>` must outlive `'a`. Only this crate creates a `Seq`, and it never
/// moves one after a redline is linked.
pub struct Seq<'a> {
    clock: Clock,
    polling: Cell<bool>,
    redlines: UnsafeCell<List<Node>>,
    walking: Cell<bool>,
    _brand: PhantomData<fn(&'a ()) -> &'a ()>,
}

/// The cycle time and abort flag that the wait futures read.
pub(crate) struct Clock {
    now: Cell<Timestamp>,
    aborted: Cell<bool>,
}

/// The reason a sequence ended without completing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SeqError {
    #[error("aborted")]
    Aborted,
    #[error("timed out")]
    TimedOut,
    #[error("{0}")]
    Failed(Cow<'static, str>),
}

impl Clock {
    /// Returns the wait's deadline.
    ///
    /// The first call sets `deadline` to `dur` after the current cycle time.
    /// Later calls return that same deadline.
    fn arm(&self, deadline: &mut Option<Timestamp>, dur: Duration) -> Timestamp {
        *deadline.get_or_insert_with(|| after(self.now.get(), dur))
    }
}

impl<'a> Seq<'a> {
    pub(crate) fn new() -> Self {
        Self {
            clock: Clock {
                now: Cell::new(Timestamp(0)),
                aborted: Cell::new(false),
            },
            polling: Cell::new(false),
            redlines: UnsafeCell::new(List::new()),
            walking: Cell::new(false),
            _brand: PhantomData,
        }
    }

    /// Returns the current cycle's time.
    pub fn now(&self) -> Timestamp {
        self.clock.now.get()
    }

    /// Returns whether the sequence has been aborted.
    ///
    /// Once this is true, it stays true.
    pub fn aborted(&self) -> bool {
        self.clock.aborted.get()
    }

    /// Aborts the sequence.
    ///
    /// Returns [`SeqError::Aborted`], so the body can end with
    /// `Err(seq.abort())`.
    pub fn abort(&self) -> SeqError {
        self.clock.aborted.set(true);
        SeqError::Aborted
    }

    /// Registers `f` to run if the sequence is aborted.
    ///
    /// `f` stays registered while the returned guard is alive, and runs when
    /// the guard is dropped if the sequence has been aborted by then.
    pub fn on_abort<F: FnOnce(&Seq<'a>)>(&self, f: F) -> OnAbort<'_, 'a, F> {
        OnAbort {
            seq: self,
            f: Some(f),
        }
    }

    /// Waits for `dur` of cycle time.
    ///
    /// The time is counted from the future's first poll. It fails early with
    /// [`SeqError::Aborted`] if the sequence is aborted.
    pub fn sleep(&self, dur: Duration) -> Sleep<'_> {
        Sleep(self.wait_for(dur, || false))
    }

    /// Waits until `pred` returns true.
    ///
    /// `pred` is called on every poll, starting with the first. The future
    /// fails with [`SeqError::TimedOut`] once `timeout` has passed since that
    /// first poll, or with [`SeqError::Aborted`] if the sequence is aborted.
    pub fn wait_for<F: FnMut() -> bool + Unpin>(
        &self,
        timeout: Duration,
        pred: F,
    ) -> WaitFor<'_, F> {
        WaitFor {
            clock: &self.clock,
            timeout,
            pred,
            deadline: None,
        }
    }

    /// Waits for the next cycle.
    ///
    /// The future resolves on its second poll with that poll's cycle time. It
    /// ignores aborts.
    pub fn next_cycle(&self) -> NextCycle<'_> {
        NextCycle {
            clock: &self.clock,
            polled: false,
        }
    }

    pub(crate) fn set_now(&self, now: Timestamp) {
        self.clock.now.set(now);
    }

    /// Marks the body as being polled until the returned guard is dropped.
    pub(crate) fn polling(&self) -> Raised<'_> {
        Raised::new(&self.polling)
    }

    /// Links a pinned redline into this `Seq`.
    ///
    /// Most code should use [`redline!`](crate::redline) instead, which
    /// pins the redline for the rest of the block. The redline unlinks itself
    /// when it is dropped. This call does nothing if the redline is already
    /// linked, or if it is made while the redlines are being checked (for
    /// example, from inside a condition).
    ///
    /// A condition must outlive the sequence's parameters, so it cannot
    /// borrow a local of the body, even through a leaked redline:
    ///
    /// ```compile_fail,E0373
    /// use metor_fsw_3::{Redline, Seq, SeqError, sequence};
    ///
    /// #[sequence]
    /// async fn dangle(seq: &Seq) -> Result<(), SeqError> {
    ///     let value = Box::new(42u32);
    ///     let redline = Box::pin(Redline::new("dangling", || *value == 42));
    ///     seq.redline(redline.as_ref());
    ///     core::mem::forget(redline);
    ///     drop(value);
    ///     seq.next_cycle().await;
    ///     Ok(())
    /// }
    /// ```
    ///
    /// It also cannot borrow one through a leaked future that linked a
    /// redline:
    ///
    /// ```compile_fail,E0373
    /// use core::future::{Future, poll_fn};
    /// use core::task::Poll;
    /// use metor_fsw_3::{Seq, SeqError, redline, sequence};
    ///
    /// #[sequence]
    /// async fn dangle(seq: &Seq<'_>) -> Result<(), SeqError> {
    ///     let mut nested = Box::pin(async {
    ///         let value = Box::new(42u32);
    ///         redline!(seq, "dangling", || *value == 42);
    ///         seq.next_cycle().await;
    ///     });
    ///     poll_fn(|cx| {
    ///         let _ = nested.as_mut().poll(cx);
    ///         Poll::Ready(())
    ///     })
    ///     .await;
    ///     core::mem::forget(nested);
    ///     seq.next_cycle().await;
    ///     Ok(())
    /// }
    /// ```
    pub fn redline<C: Fn() -> bool + 'a>(&self, redline: Pin<&Redline<C>>) {
        if self.walking.get() || !redline.node.claim(self) {
            return;
        }
        let node = NonNull::from(redline.get_ref()).cast::<Node>();
        // SAFETY: `Seq` is `!Sync`, so only this thread can reach the list,
        // and outside a walk no reference to it is live. The node is pinned,
        // so its memory lasts until its `Drop`, which unlinks it. The pointer
        // is derived from the whole redline, which `check` casts back to.
        unsafe { (*self.redlines.get()).push_back(node) };
    }

    /// Unlinks a node that is being dropped.
    pub(crate) fn unlink(&self, node: NonNull<Node>) {
        if self.walking.get() {
            // A condition dropped a linked redline, possibly its own, while
            // the walk still borrows that redline's memory. There is no sound
            // way to continue.
            std::process::abort();
        }
        // SAFETY: as in `redline`, only this thread can reach the list, and
        // no reference to it is live outside a walk. `node` is linked into it.
        unsafe { (*self.redlines.get()).remove(node) };
    }

    /// Returns the name of the first redline that trips.
    ///
    /// Redlines are checked in the order they were linked.
    ///
    /// The caller must keep everything the conditions borrow alive for `'a`.
    pub(crate) fn tripped(&self) -> Option<&'static str> {
        let _walking = Raised::new(&self.walking);
        // SAFETY: while walking, `redline` refuses to link and `unlink`
        // aborts the process, so this is the only reference to the list. Only
        // this thread can reach it, since `Seq` is `!Sync`.
        let list = unsafe { &mut *self.redlines.get() };
        list.iter_raw()
            // SAFETY: a linked node's memory is live, and its pointer was
            // derived from the whole redline. The caller keeps the
            // condition's borrows alive. The body is suspended, and a
            // condition only takes shared borrows. The body's own `&mut` to
            // its `!Unpin` frame does not claim exclusive access, just as for
            // any `async fn` that borrows a local across an `.await`.
            .find(|&node| unsafe { Node::holds(node) })
            // SAFETY: as above, a linked node is live.
            .map(|node| unsafe { node.as_ref() }.name)
    }
}

impl Drop for Seq<'_> {
    fn drop(&mut self) {
        let list = self.redlines.get_mut();
        while let Some(node) = list.pop_front() {
            // SAFETY: a linked node's memory is live until its `Drop` unlinks
            // it. Only the node itself is read; its condition is never called.
            unsafe { node.as_ref() }.release();
        }
    }
}

/// Keeps a flag set while it is alive.
///
/// The flag is cleared on drop, including during unwinding.
pub(crate) struct Raised<'a>(&'a Cell<bool>);

impl<'a> Raised<'a> {
    pub(crate) fn new(flag: &'a Cell<bool>) -> Self {
        flag.set(true);
        Self(flag)
    }
}

impl Drop for Raised<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// The guard returned by [`Seq::on_abort`].
///
/// When the guard is dropped, its handler runs if the sequence has been
/// aborted. If the guard is dropped while a panic unwinds out of the body
/// being polled, it aborts the sequence first, so the handler runs then too.
#[must_use = "a handler is disarmed when its guard drops; bind it with `let _name`"]
pub struct OnAbort<'s, 'a, F: FnOnce(&Seq<'a>)> {
    seq: &'s Seq<'a>,
    f: Option<F>,
}

impl<'a, F: FnOnce(&Seq<'a>)> Drop for OnAbort<'_, 'a, F> {
    fn drop(&mut self) {
        if std::thread::panicking() && self.seq.polling.get() {
            let _ = self.seq.abort();
        }
        if self.seq.aborted()
            && let Some(f) = self.f.take()
        {
            run_handler(self.seq, f);
        }
    }
}

/// Runs one abort handler.
///
/// If the handler panics, the panic is logged as a `handler` fault instead
/// of unwinding.
fn run_handler<'a>(seq: &Seq<'a>, f: impl FnOnce(&Seq<'a>)) {
    // PANIC Safety: the panic is caught so that the remaining handlers still
    // run, and so that a panic during an unwind never escapes.
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| f(seq))) {
        Log.fault("handler", crate::panic::into_message(payload));
    }
}

/// Returns `now` plus `dur`, saturating instead of overflowing.
fn after(now: Timestamp, dur: Duration) -> Timestamp {
    let micros = i64::try_from(dur.as_micros()).unwrap_or(i64::MAX);
    Timestamp(now.0.saturating_add(micros))
}

/// The future returned by [`Seq::sleep`].
///
/// It wraps a [`WaitFor`] whose condition is never true, and treats that
/// wait's timeout as success.
#[must_use = "a sleep does nothing unless awaited"]
pub struct Sleep<'a>(WaitFor<'a, fn() -> bool>);

impl Future for Sleep<'_> {
    type Output = Result<(), SeqError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.get_mut().0).poll(cx) {
            Poll::Ready(Err(SeqError::TimedOut)) => Poll::Ready(Ok(())),
            polled => polled,
        }
    }
}

/// The future returned by [`Seq::wait_for`].
#[must_use = "a wait does nothing unless awaited"]
pub struct WaitFor<'a, F> {
    clock: &'a Clock,
    timeout: Duration,
    pred: F,
    deadline: Option<Timestamp>,
}

impl<F: FnMut() -> bool + Unpin> Future for WaitFor<'_, F> {
    type Output = Result<(), SeqError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.clock.aborted.get() {
            return Poll::Ready(Err(SeqError::Aborted));
        }
        if (this.pred)() {
            return Poll::Ready(Ok(()));
        }
        let deadline = this.clock.arm(&mut this.deadline, this.timeout);
        if this.clock.now.get() >= deadline {
            return Poll::Ready(Err(SeqError::TimedOut));
        }
        Poll::Pending
    }
}

/// The future returned by [`Seq::next_cycle`].
#[must_use = "a cycle wait does nothing unless awaited"]
pub struct NextCycle<'a> {
    clock: &'a Clock,
    polled: bool,
}

impl Future for NextCycle<'_> {
    type Output = Timestamp;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Timestamp> {
        let this = self.get_mut();
        if core::mem::replace(&mut this.polled, true) {
            return Poll::Ready(this.clock.now.get());
        }
        Poll::Pending
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use core::pin::pin;
    use core::task::Waker;

    use super::*;
    use crate::sequence::tests::{Trace, ms};

    const MS: Duration = Duration::from_millis(1);

    fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn test_sleep_arms_on_first_poll() {
        let seq = Seq::new();
        seq.set_now(ms(0));
        let mut sleep = pin!(seq.sleep(10 * MS));
        seq.set_now(ms(5));
        assert_eq!(poll(sleep.as_mut()), Poll::Pending);
        seq.set_now(ms(14));
        assert_eq!(poll(sleep.as_mut()), Poll::Pending);
        seq.set_now(ms(15));
        assert_eq!(poll(sleep.as_mut()), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_zero_sleep_resolves_at_once() {
        let seq = Seq::new();
        assert_eq!(poll(pin!(seq.sleep(Duration::ZERO))), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_sleep_saturates() {
        let seq = Seq::new();
        seq.set_now(Timestamp(i64::MAX - 1));
        let mut sleep = pin!(seq.sleep(Duration::MAX));
        assert_eq!(poll(sleep.as_mut()), Poll::Pending);
        seq.set_now(Timestamp(i64::MAX));
        assert_eq!(poll(sleep.as_mut()), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_wait_for_resolves_on_its_poll() {
        let seq = Seq::new();
        let ready = Cell::new(false);
        let mut wait = pin!(seq.wait_for(10 * MS, || ready.get()));
        assert_eq!(poll(wait.as_mut()), Poll::Pending);
        seq.set_now(ms(3));
        ready.set(true);
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_wait_for_times_out() {
        let seq = Seq::new();
        let mut wait = pin!(seq.wait_for(10 * MS, || false));
        assert_eq!(poll(wait.as_mut()), Poll::Pending);
        seq.set_now(ms(9));
        assert_eq!(poll(wait.as_mut()), Poll::Pending);
        seq.set_now(ms(10));
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Err(SeqError::TimedOut)));
    }

    #[test]
    fn test_predicate_wins_on_deadline_poll() {
        let seq = Seq::new();
        let mut wait = pin!(seq.wait_for(Duration::ZERO, || true));
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_abort_beats_deadline() {
        let seq = Seq::new();
        let mut sleep = pin!(seq.sleep(10 * MS));
        let mut wait = pin!(seq.wait_for(10 * MS, || true));
        assert_eq!(poll(sleep.as_mut()), Poll::Pending);
        seq.set_now(ms(10));
        let _ = seq.abort();
        assert_eq!(poll(sleep.as_mut()), Poll::Ready(Err(SeqError::Aborted)));
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Err(SeqError::Aborted)));
    }

    #[test]
    fn test_next_cycle_ignores_abort() {
        let seq = Seq::new();
        let _ = seq.abort();
        let mut next = pin!(seq.next_cycle());
        assert_eq!(poll(next.as_mut()), Poll::Pending);
        seq.set_now(ms(7));
        assert_eq!(poll(next.as_mut()), Poll::Ready(Timestamp(7_000)));
    }

    #[test]
    fn test_next_cycle_on_a_repeated_time() {
        let seq = Seq::new();
        let mut next = pin!(seq.next_cycle());
        assert_eq!(poll(next.as_mut()), Poll::Pending);
        assert_eq!(poll(next.as_mut()), Poll::Ready(Timestamp(0)));
    }

    #[test]
    fn test_handler_runs_only_on_abort() {
        let seq = Seq::new();
        let ran = Trace::default();
        drop(seq.on_abort(|_| ran.push("calm")));
        assert!(ran.take().is_empty());
        let guard = seq.on_abort(|seq| {
            assert!(seq.aborted());
            ran.push("aborted");
        });
        assert_eq!(seq.abort(), SeqError::Aborted);
        drop(guard);
        assert_eq!(ran.take(), ["aborted"]);
    }

    #[test]
    fn test_handlers_run_innermost_first() {
        let seq = Seq::new();
        let ran = Trace::default();
        {
            let _first = seq.on_abort(|_| ran.push("first"));
            let _second = seq.on_abort(|_| ran.push("second"));
            let _third = seq.on_abort(|_| ran.push("third"));
            let _ = seq.abort();
        }
        assert_eq!(ran.take(), ["third", "second", "first"]);
    }

    #[test]
    fn test_panicking_handler_logs_and_the_next_runs() {
        use metor_proto_wkt::LogEvent;

        use crate::log::{LogPort, with_log_port};
        use crate::tests::utils::pair;

        let (_ring, output, mut lines) = pair::<LogEvent>(4);
        let mut port = LogPort::new(output);
        let seq = Seq::new();
        let ran = Trace::default();
        with_log_port(&mut port, Timestamp(3), || {
            let _outer = seq.on_abort(|_| ran.push("outer"));
            let _inner = seq.on_abort(|_| panic!("valve stuck"));
            let _ = seq.abort();
        });
        assert_eq!(ran.take(), ["outer"]);
        let lines: Vec<_> = lines.drain().map(|l| l.expect("decodes")).collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].message, "valve stuck");
        assert_eq!(lines[0].fields, vec![("kind".into(), "handler".into())]);
    }
}
