//! The [`Seq`] handle and the waits it hands out.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::borrow::Cow;

use metor_proto::types::Timestamp;

/// A `Seq` is a sequence's view of its cycle: the time and whether it was aborted.
#[derive(Default)]
pub struct Seq {
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

impl Seq {
    /// This cycle's time.
    pub fn now(&self) -> Timestamp {
        self.now.get()
    }

    /// Whether an abort has arrived; once true it stays true.
    pub fn aborted(&self) -> bool {
        self.aborted.get()
    }

    /// Resolves once `dur` of cycle time has passed since its first poll.
    pub fn sleep(&self, dur: Duration) -> Sleep<'_> {
        Sleep {
            seq: self,
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
            seq: self,
            timeout,
            pred,
            deadline: None,
        }
    }

    /// Resolves on the next poll with that poll's time.
    pub fn next_cycle(&self) -> NextCycle<'_> {
        NextCycle {
            seq: self,
            polled: false,
        }
    }

    pub(crate) fn set_now(&self, now: Timestamp) {
        self.now.set(now);
    }

    pub(crate) fn set_aborted(&self, aborted: bool) {
        self.aborted.set(aborted);
    }

    /// The deadline `dur` after this cycle, armed once and kept.
    fn arm(&self, deadline: &mut Option<Timestamp>, dur: Duration) -> Timestamp {
        *deadline.get_or_insert_with(|| after(self.now(), dur))
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
    seq: &'a Seq,
    dur: Duration,
    deadline: Option<Timestamp>,
}

impl Future for Sleep<'_> {
    type Output = Result<(), SeqError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.seq.aborted() {
            return Poll::Ready(Err(SeqError::Aborted));
        }
        let deadline = this.seq.arm(&mut this.deadline, this.dur);
        if this.seq.now() >= deadline {
            return Poll::Ready(Ok(()));
        }
        Poll::Pending
    }
}

/// The future [`Seq::wait_for`] returns.
#[must_use = "a wait does nothing unless awaited"]
pub struct WaitFor<'a, F> {
    seq: &'a Seq,
    timeout: Duration,
    pred: F,
    deadline: Option<Timestamp>,
}

impl<F: FnMut() -> bool + Unpin> Future for WaitFor<'_, F> {
    type Output = Result<(), SeqError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if this.seq.aborted() {
            return Poll::Ready(Err(SeqError::Aborted));
        }
        if (this.pred)() {
            return Poll::Ready(Ok(()));
        }
        let deadline = this.seq.arm(&mut this.deadline, this.timeout);
        if this.seq.now() >= deadline {
            return Poll::Ready(Err(SeqError::TimedOut));
        }
        Poll::Pending
    }
}

/// The future [`Seq::next_cycle`] returns.
#[must_use = "a cycle wait does nothing unless awaited"]
pub struct NextCycle<'a> {
    seq: &'a Seq,
    polled: bool,
}

impl Future for NextCycle<'_> {
    type Output = Timestamp;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Timestamp> {
        let this = self.get_mut();
        if core::mem::replace(&mut this.polled, true) {
            return Poll::Ready(this.seq.now());
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
        let seq = Seq::default();
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
        let seq = Seq::default();
        assert_eq!(poll(pin!(seq.sleep(Duration::ZERO))), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_sleep_saturates() {
        let seq = Seq::default();
        seq.set_now(Timestamp(i64::MAX - 1));
        let mut sleep = pin!(seq.sleep(Duration::MAX));
        assert_eq!(poll(sleep.as_mut()), Poll::Pending);
        seq.set_now(Timestamp(i64::MAX));
        assert_eq!(poll(sleep.as_mut()), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_wait_for_resolves_on_its_poll() {
        let seq = Seq::default();
        let ready = Cell::new(false);
        let mut wait = pin!(seq.wait_for(10 * MS, || ready.get()));
        assert_eq!(poll(wait.as_mut()), Poll::Pending);
        at(&seq, 3);
        ready.set(true);
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_wait_for_times_out() {
        let seq = Seq::default();
        let mut wait = pin!(seq.wait_for(10 * MS, || false));
        assert_eq!(poll(wait.as_mut()), Poll::Pending);
        at(&seq, 9);
        assert_eq!(poll(wait.as_mut()), Poll::Pending);
        at(&seq, 10);
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Err(SeqError::TimedOut)));
    }

    #[test]
    fn test_predicate_wins_on_deadline_poll() {
        let seq = Seq::default();
        let mut wait = pin!(seq.wait_for(Duration::ZERO, || true));
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
    }

    #[test]
    fn test_abort_beats_deadline() {
        let seq = Seq::default();
        let mut sleep = pin!(seq.sleep(10 * MS));
        let mut wait = pin!(seq.wait_for(10 * MS, || true));
        assert_eq!(poll(sleep.as_mut()), Poll::Pending);
        at(&seq, 10);
        seq.set_aborted(true);
        assert_eq!(poll(sleep.as_mut()), Poll::Ready(Err(SeqError::Aborted)));
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Err(SeqError::Aborted)));
    }

    #[test]
    fn test_next_cycle_ignores_abort() {
        let seq = Seq::default();
        seq.set_aborted(true);
        let mut next = pin!(seq.next_cycle());
        assert_eq!(poll(next.as_mut()), Poll::Pending);
        at(&seq, 7);
        assert_eq!(poll(next.as_mut()), Poll::Ready(Timestamp(7_000)));
    }

    #[test]
    fn test_next_cycle_on_a_repeated_time() {
        let seq = Seq::default();
        let mut next = pin!(seq.next_cycle());
        assert_eq!(poll(next.as_mut()), Poll::Pending);
        assert_eq!(poll(next.as_mut()), Poll::Ready(Timestamp(0)));
    }
}
