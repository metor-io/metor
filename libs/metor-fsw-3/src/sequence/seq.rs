//! The [`Seq`] handle, the waits it hands out, and its abort handlers.

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
use crate::coordinator::panic_message;
use crate::log::Log;

/// A `Seq` is a sequence's view of its cycle: the time, whether it was
/// aborted, and the redlines its body has linked.
///
/// A redline's condition outlives `'a`, which is invariant. Only this crate
/// builds one, and it never moves once a redline is linked.
pub struct Seq<'a> {
    clock: Clock,
    polling: Cell<bool>,
    redlines: UnsafeCell<List<Node>>,
    walking: Cell<bool>,
    _brand: PhantomData<fn(&'a ()) -> &'a ()>,
}

/// The cycle time and abort flag the waits read.
pub(crate) struct Clock {
    now: Cell<Timestamp>,
    aborted: Cell<bool>,
}

/// Why a sequence ended without completing.
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
    fn now(&self) -> Timestamp {
        self.now.get()
    }

    fn aborted(&self) -> bool {
        self.aborted.get()
    }

    /// The deadline `dur` after this cycle, armed once and kept.
    fn arm(&self, deadline: &mut Option<Timestamp>, dur: Duration) -> Timestamp {
        *deadline.get_or_insert_with(|| after(self.now(), dur))
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

    /// This cycle's time.
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Whether the sequence was aborted; once true it stays true.
    pub fn aborted(&self) -> bool {
        self.clock.aborted()
    }

    /// Aborts the sequence: sets the abort flag and returns the error to end it with.
    pub fn abort(&self) -> SeqError {
        self.clock.aborted.set(true);
        SeqError::Aborted
    }

    /// Runs `f` if the sequence aborts while the guard lives.
    pub fn on_abort<F: FnOnce(&Seq<'a>)>(&self, f: F) -> OnAbort<'_, 'a, F> {
        OnAbort {
            seq: self,
            f: Some(f),
        }
    }

    /// Resolves once `dur` of cycle time has passed since its first poll.
    pub fn sleep(&self, dur: Duration) -> Sleep<'_> {
        Sleep {
            clock: &self.clock,
            dur,
            deadline: None,
        }
    }

    /// Resolves once `pred` holds, checked on every poll from the first,
    /// or fails once `timeout` has passed since that poll.
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

    /// Resolves on the next poll with that poll's time.
    pub fn next_cycle(&self) -> NextCycle<'_> {
        NextCycle {
            clock: &self.clock,
            polled: false,
        }
    }

    pub(crate) fn set_now(&self, now: Timestamp) {
        self.clock.now.set(now);
    }

    /// Marks the body as being polled until the guard drops.
    pub(crate) fn polling(&self) -> Raised<'_> {
        Raised::new(&self.polling)
    }

    /// Links a pinned redline; it unlinks when dropped.
    ///
    /// Use [`redline!`](crate::redline), which pins it for the rest of the
    /// block. A redline linked while the redlines are checked, or linked
    /// already, stays as it was.
    ///
    /// A condition outlives the sequence's parameters, so it cannot borrow
    /// a body local, even through a leaked redline:
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
    /// Nor through a leaked future that linked one:
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
        if self.walking.get() || !redline.node().claim(self) {
            return;
        }
        let node = NonNull::from(redline.get_ref()).cast::<Node>();
        // SAFETY: `Seq` is `!Sync`, so only this thread reaches the list, and
        // outside a walk no reference to it is live. The node is pinned, so
        // its memory lasts until its `Drop`, which unlinks it. The pointer
        // is made from the whole redline, which `check` casts back to.
        unsafe { (*self.redlines.get()).push_back(node) };
    }

    /// Unlinks a dropping node.
    pub(crate) fn unlink(&self, node: NonNull<Node>) {
        if self.walking.get() {
            // A condition dropped a linked redline, possibly its own, whose
            // memory the walk still borrows. Nothing can make that sound.
            std::process::abort();
        }
        // SAFETY: as in `redline`, this thread alone reaches the list and no
        // reference to it is live outside a walk; `node` is linked into it.
        unsafe { (*self.redlines.get()).remove(node) };
    }

    /// The name of the first redline, in link order, whose condition holds.
    ///
    /// The caller keeps alive all a condition borrows for `'a`.
    pub(crate) fn tripped(&self) -> Option<&'static str> {
        let _walking = Raised::new(&self.walking);
        // SAFETY: while walking, `redline` refuses to link and `unlink`
        // aborts, so this is the only reference to the list. Only this
        // thread reaches it, since `Seq` is `!Sync`.
        let list = unsafe { &mut *self.redlines.get() };
        list.iter_raw()
            // SAFETY: a linked node's memory is live and was made from its
            // whole redline, and the caller keeps its condition's borrows
            // alive. The body is suspended, and a condition takes only
            // shared borrows; the body's own `&mut` to its `!Unpin` frame
            // does not claim exclusive access, as for any `async fn` that
            // borrows a local across an `.await`.
            .find(|&node| unsafe { Node::holds(node) })
            // SAFETY: as above, a linked node is live.
            .map(|node| unsafe { node.as_ref() }.name())
    }
}

impl Drop for Seq<'_> {
    fn drop(&mut self) {
        let list = self.redlines.get_mut();
        while let Some(node) = list.pop_front() {
            // SAFETY: a linked node's memory is live until its `Drop` unlinks
            // it. Only the node is read; its condition is never called.
            unsafe { node.as_ref() }.release();
        }
    }
}

/// Holds a flag up until dropped, unwinding included.
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

/// The guard [`Seq::on_abort`] returns. Its handler runs if it drops with the
/// sequence aborted, or while a panic unwinds the polled body.
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

/// Runs one handler, logging its panic as a `handler` fault.
fn run_handler<'a>(seq: &Seq<'a>, f: impl FnOnce(&Seq<'a>)) {
    // PANIC Safety: caught so the handlers after it still run, and so a
    // panic during an unwind never escapes.
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| f(seq))) {
        Log.fault("handler", panic_message(&*payload).to_string());
        crate::panic::discard(payload);
    }
}

/// `now` plus `dur`, saturating rather than overflowing.
fn after(now: Timestamp, dur: Duration) -> Timestamp {
    let micros = i64::try_from(dur.as_micros()).unwrap_or(i64::MAX);
    Timestamp(now.0.saturating_add(micros))
}

/// The future [`Seq::sleep`] returns.
#[must_use = "a sleep does nothing unless awaited"]
pub struct Sleep<'a> {
    clock: &'a Clock,
    dur: Duration,
    deadline: Option<Timestamp>,
}

impl Future for Sleep<'_> {
    type Output = Result<(), SeqError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.clock.aborted() {
            return Poll::Ready(Err(SeqError::Aborted));
        }
        let deadline = this.clock.arm(&mut this.deadline, this.dur);
        if this.clock.now() >= deadline {
            return Poll::Ready(Ok(()));
        }
        Poll::Pending
    }
}

/// The future [`Seq::wait_for`] returns.
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
        if this.clock.aborted() {
            return Poll::Ready(Err(SeqError::Aborted));
        }
        if (this.pred)() {
            return Poll::Ready(Ok(()));
        }
        let deadline = this.clock.arm(&mut this.deadline, this.timeout);
        if this.clock.now() >= deadline {
            return Poll::Ready(Err(SeqError::TimedOut));
        }
        Poll::Pending
    }
}

/// The future [`Seq::next_cycle`] returns.
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
            return Poll::Ready(this.clock.now());
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use core::pin::pin;
    use core::task::Waker;

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// Sets the handle to `now` in milliseconds.
    fn at(seq: &Seq, ms: i64) {
        seq.set_now(Timestamp(ms * 1_000));
    }

    #[test]
    fn test_sleep_arms_on_first_poll() {
        let seq = Seq::new();
        at(&seq, 0);
        let mut sleep = pin!(seq.sleep(10 * MS));
        at(&seq, 5);
        assert_eq!(poll(sleep.as_mut()), Poll::Pending);
        at(&seq, 14);
        assert_eq!(poll(sleep.as_mut()), Poll::Pending);
        at(&seq, 15);
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
        at(&seq, 3);
        ready.set(true);
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_wait_for_times_out() {
        let seq = Seq::new();
        let mut wait = pin!(seq.wait_for(10 * MS, || false));
        assert_eq!(poll(wait.as_mut()), Poll::Pending);
        at(&seq, 9);
        assert_eq!(poll(wait.as_mut()), Poll::Pending);
        at(&seq, 10);
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
        at(&seq, 10);
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
        at(&seq, 7);
        assert_eq!(poll(next.as_mut()), Poll::Ready(Timestamp(7_000)));
    }

    #[test]
    fn test_next_cycle_on_a_repeated_time() {
        let seq = Seq::new();
        let mut next = pin!(seq.next_cycle());
        assert_eq!(poll(next.as_mut()), Poll::Pending);
        assert_eq!(poll(next.as_mut()), Poll::Ready(Timestamp(0)));
    }

    /// Handlers' names, in the order they ran.
    #[derive(Default)]
    struct Ran(core::cell::RefCell<Vec<&'static str>>);

    impl Ran {
        fn push(&self, name: &'static str) {
            self.0.borrow_mut().push(name);
        }

        fn take(&self) -> Vec<&'static str> {
            self.0.take()
        }
    }

    #[test]
    fn test_handler_runs_only_on_abort() {
        let seq = Seq::new();
        let ran = Ran::default();
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
        let ran = Ran::default();
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
        use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
        use metor_proto_wkt::LogEvent;

        use crate::log::{LogPort, with_log_port};
        use crate::port::{Input, Output, ring_capacity};
        use crate::record::Record;

        let ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(LogEvent::MAX_LEN, 4).expect("valid"),
            max_readers: 1,
        });
        let mut port = LogPort::new(Output::new(ring.writer(NoWake).expect("writer")));
        let mut lines = Input::<LogEvent>::new(vec![ring.view(NoWake).expect("slot")]);
        let seq = Seq::new();
        let ran = Ran::default();
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
