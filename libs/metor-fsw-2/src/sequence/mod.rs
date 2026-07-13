//! The clock and status runtime under async-fn systems and sequences.
//!
//! A sequence is an `async fn` whose [`Input`]/[`Output`] ports are moved into
//! the future it becomes, owned for the future's whole life — registered with
//! [`Pack::task`](crate::Pack::task) like any async entry. The entry's driver
//! polls that future once per cycle, and everything time-shaped inside the
//! body resolves against the host's clock rather than a timer. That makes a
//! sequence deterministic under a simulated clock, and it makes the poll
//! protocol simple enough to describe in one paragraph:
//!
//! Before each poll, the driver (`handler::FutureDriver`) refreshes the shared
//! [`CycleClock`]. It writes the cycle's `now`, and — when the entry is
//! mounted as a slot occupant — latches `cancel` if an abort frame arrived on
//! the mount-appended [`SlotControlIn`] input. It then installs the clock as
//! a thread-local ambient via [`with_clock`] and polls the future
//! synchronously. Inside the poll, the author-facing free functions [`wait`],
//! [`now`], [`progress`], [`aborted`], and [`cycle`] read that ambient clock;
//! a [`Wait`] future resolves by comparing its stored deadline against
//! `CycleClock::now`, returning [`Step::Aborted`] early if `cancel` was
//! latched. After the poll, an occupant's driver drains the accumulated
//! progress lines and publishes a [`SequenceStatus`] record for the cycle.
//!
//! There is no waker machinery in any of this. A pending [`Wait`] (or
//! [`NextCycle`]) simply stays pending until the next cycle's poll observes a
//! later `now`, because the host re-polls unconditionally every cycle.
//!
//! Port order, ring sizing, and compatibility come from the entry's
//! descriptor, computed from the fn's parameter types (`crate::handler`);
//! the [`SlotControlIn`] input and [`SequenceStatus`] output are appended by
//! the occupant *mount*, never declared by the entry (`docs/packs.md` §9).
//!
//! This module is compiled unconditionally (no `wiring` feature gate), since
//! sequences are a runtime feature independent of the config front-end.

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::mem::offset_of;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;
use std::time::Duration;

use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::dynamic::pack_str;
use crate::{Frame, FrameList, Output};

#[cfg(test)]
mod tests;

/// The per-cycle state shared between the driver and the sequence body.
///
/// The driver refreshes `now` and `cancel` before each poll and drains
/// `progress` after it; the body reads and writes the cells through the free
/// functions. It is `!Send`, which is fine because a poll is synchronous and
/// single-threaded.
#[derive(Default)]
pub struct CycleClock {
    /// The cycle's coordinator time, refreshed before each poll.
    pub now: Cell<Timestamp>,
    /// Set once an abort frame arrives, and never cleared.
    pub cancel: Cell<bool>,
    /// Progress lines pushed by the body, drained into [`SequenceStatus`]
    /// each cycle.
    pub progress: RefCell<Vec<String>>,
}

impl CycleClock {
    /// Take the accumulated progress lines, leaving the buffer empty.
    pub fn drain_progress(&self) -> Vec<String> {
        core::mem::take(&mut *self.progress.borrow_mut())
    }
}

thread_local! {
    /// The ambient clock, live only for the duration of a poll.
    static SEQ_CLOCK: RefCell<Option<Rc<CycleClock>>> = const { RefCell::new(None) };
}

/// Run `f` with `clock` installed as the ambient sequence clock.
///
/// The clock is cleared on the way out even if `f` panics (a drop guard), so
/// a caught panic in one poll never leaves a stale clock behind for the next.
pub fn with_clock<R>(clock: &Rc<CycleClock>, f: impl FnOnce() -> R) -> R {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            SEQ_CLOCK.with(|c| *c.borrow_mut() = None);
        }
    }
    SEQ_CLOCK.with(|c| *c.borrow_mut() = Some(clock.clone()));
    let _clear = Clear;
    f()
}

/// The ambient clock of the current poll, or `None` outside one.
pub fn current() -> Option<Rc<CycleClock>> {
    SEQ_CLOCK.with(|c| c.borrow().clone())
}
/// How a sequence finished. The host only needs to know the future is done;
/// the specific outcome rides in [`SequenceStatus`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The sequence ran to completion.
    Completed,
    /// The sequence observed a cancel and bailed out cooperatively.
    Aborted,
    /// The sequence gave up on an error.
    Failed,
}

impl Outcome {
    /// The `SequenceStatus::run_state` byte for this outcome. Zero is reserved
    /// for "still running", so `Completed` is 1, `Aborted` 2, `Failed` 3.
    pub fn run_state(self) -> u8 {
        match self {
            Outcome::Completed => 1,
            Outcome::Aborted => 2,
            Outcome::Failed => 3,
        }
    }
}

/// Why a [`Wait`] resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The deadline passed.
    Elapsed,
    /// The sequence was cancelled before the deadline.
    Aborted,
}

impl Step {
    /// Whether the wait ended because the sequence was aborted, for the
    /// idiomatic `if wait(..).await.aborted() { return .. }`.
    pub fn aborted(self) -> bool {
        matches!(self, Step::Aborted)
    }
}

/// A timer future driven entirely by the ambient [`CycleClock`].
///
/// It resolves once `CycleClock::now` reaches its stored deadline, or
/// immediately with [`Step::Aborted`] once `cancel` is latched. It never
/// registers a waker; the host re-polls the sequence every cycle anyway.
#[must_use = "a Wait does nothing unless .awaited"]
pub struct Wait {
    deadline: Timestamp,
}

impl Future for Wait {
    type Output = Step;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Step> {
        let clock = current().expect("Wait polled outside a sequence poll");
        if clock.cancel.get() {
            Poll::Ready(Step::Aborted)
        } else if clock.now.get() >= self.deadline {
            Poll::Ready(Step::Elapsed)
        } else {
            Poll::Pending
        }
    }
}

/// Suspend until `dur` of coordinator time has elapsed, or the sequence is
/// aborted. The deadline is fixed at the call as the ambient `now` plus `dur`.
#[must_use = "wait() returns a future that does nothing unless .awaited"]
pub fn wait(dur: Duration) -> Wait {
    let now = current()
        .expect("wait() called outside a sequence poll")
        .now
        .get();
    Wait {
        deadline: now + dur,
    }
}

/// The coordinator time of the current cycle, the same `now` every system
/// sees this cycle. Frames a sequence emits should be stamped with it, never
/// with wall time, so runs stay deterministic under a simulated clock.
pub fn now() -> Timestamp {
    current()
        .expect("now() called outside a sequence poll")
        .now
        .get()
}

/// Record a progress line for the next [`SequenceStatus`] publish.
pub fn progress(msg: impl Into<String>) {
    current()
        .expect("progress() called outside a sequence poll")
        .progress
        .borrow_mut()
        .push(msg.into());
}

/// Whether the sequence has been cancelled, for bodies that want to branch on
/// it without awaiting a [`Wait`].
pub fn aborted() -> bool {
    current()
        .expect("aborted() called outside a sequence poll")
        .cancel
        .get()
}

/// Suspend until the next cycle's poll, resolving with that cycle's `now`.
///
/// The general-system suspension point: an async-fn system holds its state
/// in locals and writes `let now = cycle().await;` at the top of its loop.
/// Like [`Wait`] there is no waker machinery — the driver re-polls every
/// cycle, and the future is ready once the ambient clock has advanced past
/// the value it was armed at. Deterministic under a simulated clock.
///
/// `cycle()` does not observe cancellation; a cancellable loop checks
/// [`aborted`] after each await and runs its own safing branch.
#[must_use = "cycle() returns a future that does nothing unless .awaited"]
pub fn cycle() -> NextCycle {
    NextCycle {
        armed_at: current()
            .expect("cycle() called outside a system poll")
            .now
            .get(),
    }
}

/// The future [`cycle`] returns; resolves at the first poll whose clock has
/// advanced past the arming cycle.
pub struct NextCycle {
    armed_at: Timestamp,
}

impl Future for NextCycle {
    type Output = Timestamp;

    fn poll(self: Pin<&mut Self>, _cx: &mut core::task::Context<'_>) -> Poll<Timestamp> {
        let clock = current().expect("NextCycle polled outside a system poll");
        let now = clock.now.get();
        if now > self.armed_at {
            Poll::Ready(now)
        } else {
            Poll::Pending
        }
    }
}

// ---------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------

/// Capacity of one progress line's message; longer lines are truncated.
pub const PROGRESS_MSG_CAP: usize = 64;
/// Most progress lines one [`SequenceStatus`] record can carry.
pub const MAX_PROGRESS: usize = 16;

/// A fixed-size text cell carrying one status message, stored as a used
/// length plus a [`PROGRESS_MSG_CAP`]-byte buffer.
#[derive(metor_fsw::AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
pub struct ProgressLine {
    pub len: u8,
    pub _pad: [u8; 7],
    pub msg: [u8; PROGRESS_MSG_CAP],
}

impl ProgressLine {
    /// Wrap a message, truncating it to [`PROGRESS_MSG_CAP`].
    pub fn new(msg: &str) -> Self {
        let (msg, len) = pack_str::<PROGRESS_MSG_CAP>(msg);
        Self {
            len,
            _pad: [0; 7],
            msg,
        }
    }
}

/// The sequence's per-cycle telemetry frame. `run_state` is zero while the
/// future is still pending and an [`Outcome::run_state`] byte once it is done;
/// `progress` carries the lines drained from the clock this cycle.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "sequence")]
pub struct SequenceStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub run_state: u8,
    pub _pad: [u8; 7],
    pub progress: FrameList<ProgressLine, MAX_PROGRESS>,
}

/// The implicit cancel input the occupant mount appends after an entry's own
/// inputs. An abort command lands here as ordinary ring data and is folded
/// into [`CycleClock::cancel`] at the top of the next poll.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "slot_control")]
pub struct SlotControlIn {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub cancel: u8,
    pub _pad: [u8; 7],
}

// ---------------------------------------------------------------------------
// The per-cycle status writer
// ---------------------------------------------------------------------------

/// Publish one [`SequenceStatus`] record with the given run state and
/// progress lines. The occupant driver drives its health `end_cycle`
/// separately.
pub fn publish_status(
    out: &mut Output<SequenceStatus>,
    now: Timestamp,
    run_state: u8,
    lines: &[String],
) {
    let frame = SequenceStatus {
        timestamp: now,
        run_state,
        _pad: [0; 7],
        progress: FrameList::EMPTY,
    };
    let _ = out.write_with(&frame, |fw| {
        fw.list(offset_of!(SequenceStatus, progress), |l| {
            for line in lines {
                l.push(ProgressLine::new(line));
            }
        });
    });
}
