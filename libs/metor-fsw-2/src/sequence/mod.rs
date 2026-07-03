//! The sequence-occupant runtime: the future-driven state machine a
//! `#[sequence] async fn` becomes (sequences-slots.md §4, §7).
//!
//! A sequence is an `async fn` whose `Input`/`Output` ports are **moved into the
//! future** and owned for its whole life — so there is no per-cycle port threading
//! (§4.2). The coordinator polls the future once per cycle through the dl C-ABI
//! (`abi::run_seq_*`); the future drives itself off coordinator time, not a timer
//! wheel, so it is deterministic under a [`Simulated`](crate::ClockMode) clock.
//!
//! This module is **ungated** (sequences are an ABI/runtime feature, not KDL): the
//! ambient [`SeqClock`] + the free [`wait`]/[`progress`]/[`aborted`] author API, the
//! [`Wait`] future, the occupant-side telemetry frames ([`SequenceStatus`]) and the
//! cancel frame ([`SlotControlIn`]), and the [`SeqSystem`] seam the generated
//! occupant implements (which `abi::run_seq_*` binds against).

use core::cell::{Cell, RefCell};
use core::future::Future;
use core::mem::offset_of;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::rc::Rc;
use std::time::Duration;

use metor_fsw_ring::{Backing, BoxBacking, RawBacking};
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::abi::RawBinder;
use crate::{Frame, FrameList, Input, Out, Output, SystemDescriptor, SystemOutput};

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// The per-cycle ambient: the clock cell + the task-local that carries it
// ---------------------------------------------------------------------------

/// The only state a cycle refreshes for the owned-port future (§4.3): the
/// coordinator clock `now` (so [`Wait`] resolves against coordinator time), the
/// latched `cancel` flag (folded from the [`SlotControlIn`] frame), and a
/// `progress` sink drained into [`SequenceStatus`] each cycle.
///
/// `!Send` (it holds `Cell`/`RefCell`) — fine, the poll is synchronous and
/// single-threaded (coordinator.md §3.7).
#[derive(Default)]
pub struct SeqClock {
    /// Refreshed by `run_seq_execute` before each poll; [`Wait`] compares against it.
    pub now: Cell<Timestamp>,
    /// Latched once an `Abort` control frame arrives; [`Wait`] short-circuits on it.
    pub cancel: Cell<bool>,
    /// Progress lines pushed by [`progress`], drained into [`SequenceStatus`] each cycle.
    pub progress: RefCell<Vec<String>>,
}

impl SeqClock {
    /// Take the accumulated progress lines (publishing them empties the buffer).
    pub fn drain_progress(&self) -> Vec<String> {
        core::mem::take(&mut *self.progress.borrow_mut())
    }
}

thread_local! {
    /// The current sequence clock, live **only** during a poll (`run_seq_execute`
    /// sets it around the synchronous future poll and clears it after).
    static SEQ_CLOCK: RefCell<Option<Rc<SeqClock>>> = const { RefCell::new(None) };
}

/// Run `f` with `clock` installed as the ambient sequence clock, clearing it on the
/// way out **even if `f` panics** (a drop guard, so a caught `execute` panic never
/// leaves a dangling clock for the next poll). Sound because the poll is synchronous
/// and single-threaded: the task-local is live only for the duration of `f`.
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

/// The clock installed by the current poll, or `None` outside one. The free
/// functions and [`Wait`] `expect`-`Some` it: they only ever run inside a poll.
pub fn current() -> Option<Rc<SeqClock>> {
    SEQ_CLOCK.with(|c| c.borrow().clone())
}

// ---------------------------------------------------------------------------
// Outcome + the wait Step
// ---------------------------------------------------------------------------

/// How a sequence finished — cube-sat's outcomes plus the protocol's `Failed`
/// (§4.5). The detail rides the [`SequenceStatus`] frame; the host only needs the
/// single "terminal" bit ([`FswStatus::Done`](crate::abi::FswStatus::Done)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The sequence ran to completion.
    Completed,
    /// The sequence observed `cancel` and bailed out cooperatively.
    Aborted,
    /// The sequence gave up on an error.
    Failed,
}

impl Outcome {
    /// The `SequenceStatus::run_state` byte for a terminal outcome (`0` is reserved
    /// for "still running"): `Completed=1`, `Aborted=2`, `Failed=3`.
    pub fn run_state(self) -> u8 {
        match self {
            Outcome::Completed => 1,
            Outcome::Aborted => 2,
            Outcome::Failed => 3,
        }
    }
}

/// Why a [`Wait`] resolved: its deadline elapsed, or the sequence was aborted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The deadline passed (`clock.now >= deadline`).
    Elapsed,
    /// `cancel` was latched before the deadline; the body should bail out.
    Aborted,
}

impl Step {
    /// Whether this step ended because the sequence was aborted (the idiomatic
    /// `if wait(..).await.aborted() { return … }`).
    pub fn aborted(self) -> bool {
        matches!(self, Step::Aborted)
    }
}

// ---------------------------------------------------------------------------
// The wait future + the free-function author API
// ---------------------------------------------------------------------------

/// A timer that resolves by comparing a stored `deadline` against the ambient
/// [`SeqClock::now`] — no maitake timer wheel, so it is driven entirely by
/// coordinator time and deterministic under a `Simulated` clock (§4.3). It also
/// short-circuits to [`Step::Aborted`] the moment `cancel` is latched.
pub struct Wait {
    deadline: Timestamp,
}

impl Future for Wait {
    type Output = Step;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Step> {
        // Only ever polled inside a sequence poll, where the task-local is set.
        let clock = current().expect("Wait polled outside a sequence poll");
        if clock.cancel.get() {
            Poll::Ready(Step::Aborted)
        } else if clock.now.get() >= self.deadline {
            Poll::Ready(Step::Elapsed)
        } else {
            // Re-checked next cycle; no waker interaction (the coordinator re-polls).
            Poll::Pending
        }
    }
}

/// Suspend until `dur` of coordinator time has elapsed (or the sequence is aborted).
/// The deadline is `now + dur` computed **at the call**, off the ambient clock.
pub fn wait(dur: Duration) -> Wait {
    let now = current().expect("wait() called outside a sequence poll").now.get();
    Wait { deadline: now + dur }
}

/// The coordinator time of the current cycle (review E7) — the same value every
/// system's `execute` receives as `now` this cycle, refreshed before each poll. The
/// timestamp a sequence stamps the frames it emits with (never wall time, so it is
/// deterministic under a [`Simulated`](crate::ClockMode) clock).
pub fn now() -> Timestamp {
    current()
        .expect("now() called outside a sequence poll")
        .now
        .get()
}

/// Record a progress line for the next [`SequenceStatus`] publish (§7).
pub fn progress(msg: impl Into<String>) {
    current()
        .expect("progress() called outside a sequence poll")
        .progress
        .borrow_mut()
        .push(msg.into());
}

/// Whether the sequence has been aborted — a poll-free check, complementing
/// [`Step::aborted`] (for bodies that branch on cancel without awaiting a [`Wait`]).
pub fn aborted() -> bool {
    current()
        .expect("aborted() called outside a sequence poll")
        .cancel
        .get()
}

/// The opt-in explicit handle (§4.3): for authors who prefer `seq.wait(..)` over the
/// ambient free functions. It delegates to its own [`SeqClock`], which during a poll
/// **is** the task-local clock, so a [`Wait`] it produces reads the same state.
pub struct Seq {
    clock: Rc<SeqClock>,
}

impl Seq {
    /// Build the handle over the occupant's clock (the macro passes `clock.clone()`).
    pub fn new(clock: Rc<SeqClock>) -> Self {
        Self { clock }
    }

    /// As [`wait`], but off this handle's clock.
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
// Frames (mirroring src/health.rs)
// ---------------------------------------------------------------------------

/// Fixed capacity of one progress line's message (longer lines are truncated).
pub const PROGRESS_MSG_CAP: usize = 64;
/// Max progress lines carried in one [`SequenceStatus`] record.
pub const MAX_PROGRESS: usize = 16;

/// One progress line: a used-length and a fixed-size message buffer. The `msg` array
/// is a `U8` component (`sequence.progress.N.msg`), mirroring [`LogLine`](crate::LogLine).
#[derive(metor_fsw::AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
pub struct ProgressLine {
    pub len: u8,
    pub _pad: [u8; 7],
    pub msg: [u8; PROGRESS_MSG_CAP],
}

impl ProgressLine {
    /// Wrap a progress message, truncating to [`PROGRESS_MSG_CAP`] like `LogLine::new`.
    pub fn new(msg: &str) -> Self {
        let bytes = msg.as_bytes();
        let len = bytes.len().min(PROGRESS_MSG_CAP);
        let mut buf = [0u8; PROGRESS_MSG_CAP];
        buf[..len].copy_from_slice(&bytes[..len]);
        Self {
            len: len as u8,
            _pad: [0; 7],
            msg: buf,
        }
    }
}

/// The occupant-side telemetry frame (§7): the current `run_state` (`0` running,
/// else [`Outcome::run_state`]) and the drained `progress` details. Published each
/// cycle by `run_seq_execute` from the wrapper-owned `Out` tail.
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

/// The implicit cancel input every slot reserves (§4.4): an `Abort` command targeting
/// the slot is written here as ordinary ring data, folded into [`SeqClock::cancel`] at
/// the top of the next `run_seq_execute`.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "slot_control")]
pub struct SlotControlIn {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub cancel: u8,
    pub _pad: [u8; 7],
}

/// The macro's implicit output bundle: the single [`SequenceStatus`] port, wrapped by
/// the framework's [`Out`] so it also carries the standard health/log pair
/// (`src/system/mod.rs`). `#[derive(SystemOutput)]` gives it both `descriptors()` and
/// `BindPorts`.
#[derive(SystemOutput)]
pub struct SeqStatusOut<B: Backing = BoxBacking> {
    pub status: Output<SequenceStatus, B>,
}

// ---------------------------------------------------------------------------
// The SeqSystem seam the generated occupant implements
// ---------------------------------------------------------------------------

/// The bound occupant `abi::run_seq_*` threads: the owned future (the user ports moved
/// inside it), the wrapper-owned [`SequenceStatus`] / health / log tail (an [`Out`]),
/// and the cancel input the cycle reads. All over [`RawBacking`] — a dlopen'd
/// occupant's views into the host's regions.
///
/// TODO(E6, seq path): a sequence's *user* output ports move into the future, so
/// their [`publish`](crate::Output::publish) drop counters are unreachable from this
/// wrapper — unlike [`CyclicRunner::step`](crate::CyclicRunner), the per-poll driver
/// (`abi::run_seq_execute`, in-flight in the slot-addressing work) cannot fold them
/// into `health.error("publish_dropped")` yet. Folding needs either a shared counter
/// cell threaded into the ports at `build()` or a poll-side hook on this struct; wire
/// it up when the seq execute path is next touched.
pub struct SeqBound {
    pub future: Pin<Box<dyn Future<Output = Outcome>>>,
    pub status: Out<SeqStatusOut<RawBacking>, RawBacking>,
    pub control: Input<SlotControlIn, RawBacking>,
}

/// The seam the `#[sequence]` macro implements and `abi::run_seq_*` is generic over —
/// defined here (not in `abi`) so it stays ungated, and in `abi` reach so the helpers
/// can name it without `kdl`.
///
/// [`descriptor`](SeqSystem::descriptor) is the single source of truth for ring
/// sizing / `compatible()` validation / the prefixed `announce`: it lists inputs
/// `[user inputs…, SlotControlIn]` and outputs `[user outputs…, then the
/// `Out<SeqStatusOut>` tail = SequenceStatus, health, log]`, `kind = Cyclic`. It may
/// name ports at [`BoxBacking`] (a `PortDesc` is backing-independent). [`build`](SeqSystem::build)
/// binds those same ports — in the same order — at [`RawBacking`], moving the user
/// ports into the future.
pub trait SeqSystem {
    /// The params value `run_seq_create` postcard-decodes (`()` for a paramless sequence).
    type Params;

    /// The signature-derived descriptor (the order the host sizes/validates and
    /// `build` binds in).
    fn descriptor() -> SystemDescriptor;

    /// Bind the ports off the `RawBinder` in `descriptor()` order, move the user ports
    /// + control into the future, and hand back the [`SeqBound`].
    fn build(params: Self::Params, binder: &mut RawBinder, clock: &Rc<SeqClock>) -> SeqBound;
}

// ---------------------------------------------------------------------------
// The per-cycle status writer the ABI execute drives
// ---------------------------------------------------------------------------

/// Publish one [`SequenceStatus`] record from the wrapper-owned `Out` tail: stamp
/// `run_state`/`timestamp` and fill the `progress` [`FrameList`] from `lines` (each a
/// [`ProgressLine::new`]). The caller separately drives `out.health().end_cycle`.
pub fn publish_status<B: Backing>(
    out: &mut Out<SeqStatusOut<B>, B>,
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
