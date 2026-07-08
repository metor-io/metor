//! The runtime a `#[sequence] async fn` compiles into.
//!
//! A sequence is an `async fn` whose [`Input`]/[`Output`] ports are moved into
//! the future it becomes, owned for the future's whole life. The host polls
//! that future once per cycle through the ABI entry points in [`crate::abi`],
//! and everything time-shaped inside the body resolves against the host's
//! clock rather than a timer. That makes a sequence deterministic under a
//! simulated clock, and it makes the poll protocol simple enough to describe
//! in one paragraph:
//!
//! Before each poll, the driver (`abi::run_seq_execute`) refreshes the shared
//! [`SeqClock`]. It writes the cycle's `now`, and it latches `cancel` if an
//! abort frame arrived on the sequence's [`SlotControlIn`] input. It then
//! installs the clock as a thread-local ambient via [`with_clock`] and polls
//! the future synchronously. Inside the poll, the author-facing free functions
//! [`wait`], [`now`], [`progress`], and [`aborted`] read that ambient clock;
//! a [`Wait`] future resolves by comparing its stored deadline against
//! `SeqClock::now`, returning [`Step::Aborted`] early if `cancel` was latched.
//! After the poll, the driver drains the accumulated progress lines and
//! publishes a [`SequenceStatus`] record for the cycle.
//!
//! There is no waker machinery in any of this. A pending `Wait` simply stays
//! pending until the next cycle's poll observes a later `now`, because the
//! host re-polls unconditionally every cycle.
//!
//! The `#[sequence]` macro generates an occupant type implementing
//! [`SeqSystem`], the seam the ABI helpers are generic over. Its
//! [`descriptor`](SeqSystem::descriptor) is the single source of truth for
//! port order, ring sizing, and compatibility checks. Inputs are the user's
//! inputs followed by the implicit [`SlotControlIn`]; outputs are the user's
//! outputs followed by the [`SequenceStatus`]/health/log tail of
//! [`SeqStatusOut`]. [`build`](SeqSystem::build) binds those same ports in
//! that same order and hands back a [`SeqBound`].
//!
//! Authors who prefer an explicit handle over the ambient free functions can
//! take a [`Seq`] argument instead; it reads the same clock.
//!
//! This module is compiled unconditionally (no `kdl` feature gate), since
//! sequences are a runtime feature independent of configuration parsing.

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::mem::offset_of;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;
use std::time::Duration;

use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::abi::RawBinder;
use crate::dynamic::pack_str;
use crate::{Frame, FrameList, Input, Out, Output, SystemDescriptor, SystemOutput};

#[cfg(test)]
mod tests;

/// The per-cycle state shared between the driver and the sequence body.
///
/// The driver refreshes `now` and `cancel` before each poll and drains
/// `progress` after it; the body reads and writes the cells through the free
/// functions or a [`Seq`] handle. It is `!Send`, which is fine because a poll
/// is synchronous and single-threaded.
#[derive(Default)]
pub struct SeqClock {
    /// The cycle's coordinator time, refreshed before each poll.
    pub now: Cell<Timestamp>,
    /// Set once an abort frame arrives, and never cleared.
    pub cancel: Cell<bool>,
    /// Progress lines pushed by the body, drained into [`SequenceStatus`]
    /// each cycle.
    pub progress: RefCell<Vec<String>>,
}

impl SeqClock {
    /// Take the accumulated progress lines, leaving the buffer empty.
    pub fn drain_progress(&self) -> Vec<String> {
        core::mem::take(&mut *self.progress.borrow_mut())
    }
}

thread_local! {
    /// The ambient clock, live only for the duration of a poll.
    static SEQ_CLOCK: RefCell<Option<Rc<SeqClock>>> = const { RefCell::new(None) };
}

/// Run `f` with `clock` installed as the ambient sequence clock.
///
/// The clock is cleared on the way out even if `f` panics (a drop guard), so
/// a caught panic in one poll never leaves a stale clock behind for the next.
pub fn with_clock<R>(clock: &Rc<SeqClock>, f: impl FnOnce() -> R) -> R {
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
pub fn current() -> Option<Rc<SeqClock>> {
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

/// A timer future driven entirely by the ambient [`SeqClock`].
///
/// It resolves once `SeqClock::now` reaches its stored deadline, or
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

/// An explicit handle over the sequence clock, for authors who prefer
/// `seq.wait(..)` to the ambient free functions. During a poll its clock is
/// the same one the free functions read, so the two styles are equivalent.
pub struct Seq {
    clock: Rc<SeqClock>,
}

impl Seq {
    /// Build a handle over the occupant's clock.
    pub fn new(clock: Rc<SeqClock>) -> Self {
        Self { clock }
    }

    /// As [`wait`], but off this handle's clock.
    #[must_use = "wait() returns a future that does nothing unless .awaited"]
    pub fn wait(&self, dur: Duration) -> Wait {
        Wait {
            deadline: self.clock.now.get() + dur,
        }
    }

    /// As [`progress`], but off this handle's clock.
    pub fn progress(&self, msg: impl Into<String>) {
        self.clock.progress.borrow_mut().push(msg.into());
    }

    /// As [`aborted`], but off this handle's clock.
    pub fn aborted(&self) -> bool {
        self.clock.cancel.get()
    }

    /// As [`now`], but off this handle's clock.
    pub fn now(&self) -> Timestamp {
        self.clock.now.get()
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

/// The implicit cancel input every sequence reserves. An abort command lands
/// here as ordinary ring data and is folded into [`SeqClock::cancel`] at the
/// top of the next poll.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "slot_control")]
pub struct SlotControlIn {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub cancel: u8,
    pub _pad: [u8; 7],
}

/// The implicit output bundle of every sequence, wrapped in [`Out`] so the
/// [`SequenceStatus`] port comes with the standard health and log ports.
#[derive(SystemOutput)]
pub struct SeqStatusOut {
    pub status: Output<SequenceStatus>,
}

// ---------------------------------------------------------------------------
// The SeqSystem seam the generated occupant implements
// ---------------------------------------------------------------------------

/// Everything the driver needs to run one sequence, handed back by
/// [`SeqSystem::build`]. The future owns the user ports; alongside it ride
/// the wrapper-owned status/health/log tail and the cancel input the driver
/// reads each cycle.
///
/// TODO: because the user's output ports live inside the future, their
/// per-port dropped-publish counters are unreachable from here, so dropped
/// publishes cannot be folded into `health.error("publish_dropped")` the way
/// a cyclic system's are. Fixing this means minting a shared counter cell in
/// the generated `build` and threading a clone into each user output before
/// the ports move into the future. Deferred, since that indirection would tax
/// every publish on every cyclic system for a sequence-only need.
pub struct SeqBound {
    pub future: Pin<Box<dyn Future<Output = Outcome>>>,
    pub status: Out<SeqStatusOut>,
    pub control: Input<SlotControlIn>,
}

/// The interface a generated sequence occupant implements, letting the ABI
/// helpers describe, create, and poll any sequence without knowing its
/// concrete type.
///
/// [`descriptor`](SeqSystem::descriptor) is the single source of truth for
/// port order, ring sizing, and compatibility checks. Its inputs are the
/// user's inputs followed by [`SlotControlIn`]; its outputs are the user's
/// outputs followed by the [`SeqStatusOut`] tail (status, health, log).
/// [`build`](SeqSystem::build) must bind those same ports in that same order.
pub trait SeqSystem {
    /// The params value decoded at create time, `()` for a paramless sequence.
    type Params;

    /// The signature-derived descriptor.
    fn descriptor() -> SystemDescriptor;

    /// Bind the ports off `binder` in `descriptor()` order, move the user
    /// ports into the future, and hand back the [`SeqBound`].
    fn build(params: Self::Params, binder: &mut RawBinder, clock: &Rc<SeqClock>) -> SeqBound;
}

// ---------------------------------------------------------------------------
// The per-cycle status writer
// ---------------------------------------------------------------------------

/// Publish one [`SequenceStatus`] record with the given run state and
/// progress lines. The caller drives `out.health().end_cycle` separately.
pub fn publish_status(
    out: &mut Out<SeqStatusOut>,
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
    let _ = out.status.write_with(&frame, |fw| {
        fw.list(offset_of!(SequenceStatus, progress), |l| {
            for line in lines {
                l.push(ProgressLine::new(line));
            }
        });
    });
}
