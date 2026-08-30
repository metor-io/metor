//! The driver behind a fn-authored system: owns the state and the port
//! states between cycles and generalizes [`CyclicRunner`]'s drop report and
//! log flush to the parameter-tuple world.
//!
//! [`CyclicRunner`]: crate::CyclicRunner

use core::marker::PhantomData;

use metor_proto::types::Timestamp;

use crate::binder::{AnySource, RingSource};
use crate::log::{LogEvent, LogPort};
use crate::message::MsgOut;
use crate::pack::{Driver, StepStatus};
use crate::system::report_dropped;

use super::param::{BindCx, CycleCx};
use super::tuples::{ExecParamSet, ExecuteFn};

/// How a fn-authored driver reaches its state: owned outright (the zero-cost
/// default), or the scoped borrow of a pack-shared instance.
pub(crate) enum StateAccess<S> {
    Owned(S),
    Shared(crate::Shared<S>),
}

impl<S> StateAccess<S> {
    fn with<R>(&mut self, f: impl FnOnce(&mut S) -> R) -> R {
        match self {
            StateAccess::Owned(state) => f(state),
            StateAccess::Shared(token) => f(&mut token.get()),
        }
    }
}

pub(crate) struct FnDriver<S, M, F>
where
    F: ExecuteFn<S, M>,
{
    state: StateAccess<S>,
    execute: F,
    ports: <F::Params as ExecParamSet>::State,
    log: LogPort,
    _m: PhantomData<fn() -> M>,
}

impl<S, M, F> FnDriver<S, M, F>
where
    S: 'static,
    F: ExecuteFn<S, M>,
{
    /// Bind the parameter states in declaration order, then the implicit
    /// log tail — the same order the entry's descriptor declares.
    pub(crate) fn bind(state: StateAccess<S>, execute: F, src: &mut AnySource) -> Self {
        let ports = {
            let mut cx = BindCx {
                src,
                params: None,
                drops: None,
            };
            <F::Params as ExecParamSet>::bind(&mut cx)
        };
        Self {
            state,
            execute,
            ports,
            log: bind_log_tail(src),
            _m: PhantomData,
        }
    }
}

/// Bind the implicit log output every entry's descriptor appends, after the
/// user ports.
pub(crate) fn bind_log_tail(src: &mut AnySource) -> LogPort {
    let log: MsgOut<LogEvent> = MsgOut::bind(src);
    let mut port = LogPort::new(log);
    port.set_instance(src.instance_name());
    port
}

impl<S, M, F> Driver for FnDriver<S, M, F>
where
    S: 'static,
    F: ExecuteFn<S, M>,
{
    fn init(&mut self) {}

    fn step(&mut self, now: Timestamp) -> StepStatus {
        {
            let Self {
                state,
                execute,
                ports,
                log,
                ..
            } = self;
            state.with(|state| {
                let mut cx = CycleCx {
                    now,
                    log: Some(log),
                };
                let items = <F::Params as ExecParamSet>::get(ports, &mut cx);
                execute.call(state, items);
            });
        }
        let dropped = <F::Params as ExecParamSet>::take_dropped(&mut self.ports);
        report_dropped(&mut self.log, dropped);
        self.log.flush(now);
        StepStatus::Running
    }

    fn shutdown(&mut self) {}
}

/// The driver behind an async-fn (task) entry: the future owns its ports and
/// its state (locals); the driver refreshes the clock and polls once per
/// cycle with a no-op waker, exactly the sequence execution model. Drops
/// from the future-owned outputs arrive through the shared cell and are
/// reported on the log, same as a runner-owned bundle's.
pub(crate) struct FutureDriver {
    future: core::pin::Pin<Box<dyn core::future::Future<Output = crate::sequence::Outcome>>>,
    clock: std::rc::Rc<crate::sequence::CycleClock>,
    log: LogPort,
    drops: std::sync::Arc<core::sync::atomic::AtomicU64>,
    /// A finished future is never polled again; the terminal is re-served.
    done: Option<crate::sequence::Outcome>,
}

impl FutureDriver {
    pub(crate) fn new(
        future: core::pin::Pin<Box<dyn core::future::Future<Output = crate::sequence::Outcome>>>,
        clock: std::rc::Rc<crate::sequence::CycleClock>,
        log: LogPort,
        drops: std::sync::Arc<core::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            future,
            clock,
            log,
            drops,
            done: None,
        }
    }

    /// Poll once under the ambient clock, reporting the shared drop count
    /// and flushing the log; the caller decides what happens to progress
    /// lines and the outcome.
    fn poll_once(&mut self, now: Timestamp) -> core::task::Poll<crate::sequence::Outcome> {
        use core::task::{Context, Waker};
        self.clock.now.set(now);
        let poll = crate::sequence::with_clock(&self.clock, || {
            self.future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        });
        let dropped = self.drops.swap(0, core::sync::atomic::Ordering::Relaxed);
        report_dropped(&mut self.log, dropped);
        self.log.flush(now);
        poll
    }
}

impl Driver for FutureDriver {
    fn init(&mut self) {}

    fn step(&mut self, now: Timestamp) -> StepStatus {
        use core::task::Poll;
        if let Some(outcome) = self.done {
            return StepStatus::Done(outcome);
        }
        let poll = self.poll_once(now);
        // A wired task has no SequenceStatus output; progress lines land on
        // the entry's ordinary log so they still flow off-board.
        for line in self.clock.drain_progress() {
            self.log.log(crate::log::LogLevel::Info, &line);
        }
        match poll {
            Poll::Ready(outcome) => {
                self.done = Some(outcome);
                StepStatus::Done(outcome)
            }
            Poll::Pending => StepStatus::Running,
        }
    }

    fn shutdown(&mut self) {}
}

/// The slot-occupant wrapper around a future-driven entry: the cancel latch
/// folded from the [`SlotControlIn`](crate::SlotControlIn) input before each
/// poll, and a [`SequenceStatus`](crate::sequence::SequenceStatus) record
/// published after it. Bound AFTER the inner entry's own ports (control
/// after the user inputs; status after the log tail), matching the
/// occupant tail the host appends around the entry's descriptor.
pub(crate) struct OccupantFuture {
    inner: FutureDriver,
    control: crate::port::Input<crate::sequence::SlotControlIn>,
    status: crate::port::Output<crate::sequence::SequenceStatus>,
}

impl OccupantFuture {
    pub(crate) fn new(
        inner: FutureDriver,
        control: crate::port::Input<crate::sequence::SlotControlIn>,
        status: crate::port::Output<crate::sequence::SequenceStatus>,
    ) -> Self {
        Self {
            inner,
            control,
            status,
        }
    }
}

impl Driver for OccupantFuture {
    fn init(&mut self) {}

    fn step(&mut self, now: Timestamp) -> StepStatus {
        use core::task::Poll;
        if let Some(outcome) = self.inner.done {
            return StepStatus::Done(outcome);
        }
        // A cancel stays latched once seen, even if later control frames
        // clear it.
        match self.control.latest() {
            Ok(Some(f)) if f.cancel != 0 => self.inner.clock.cancel.set(true),
            Err(_) => {
                let outcome = crate::sequence::Outcome::Failed;
                self.inner.done = Some(outcome);
                return StepStatus::Done(outcome);
            }
            _ => {}
        }
        let poll = self.inner.poll_once(now);
        let lines = self.inner.clock.drain_progress();
        let mut outcome = match poll {
            Poll::Ready(outcome) => Some(outcome),
            Poll::Pending => None,
        };
        let run_state = outcome.map_or(0, crate::sequence::Outcome::run_state);
        if crate::sequence::publish_status(&mut self.status, now, run_state, &lines).is_err() {
            outcome = Some(crate::sequence::Outcome::Failed);
        }
        if let Some(outcome) = outcome {
            self.inner.done = Some(outcome);
            StepStatus::Done(outcome)
        } else {
            StepStatus::Running
        }
    }

    fn shutdown(&mut self) {}
}

/// The slot-occupant wrapper around a cyclic entry (fn-authored or struct):
/// a latched cancel stops stepping the inner driver and reports a terminal
/// `Aborted` — the hard-but-clean stop; cooperative cancellation is the
/// async style's domain. A `SequenceStatus` record is published every step
/// so the slot runner's status tap works for any occupant.
pub(crate) struct OccupantCyclic {
    inner: Box<dyn Driver>,
    control: crate::port::Input<crate::sequence::SlotControlIn>,
    status: crate::port::Output<crate::sequence::SequenceStatus>,
    done: Option<crate::sequence::Outcome>,
}

impl Driver for OccupantCyclic {
    fn init(&mut self) {
        self.inner.init()
    }

    fn step(&mut self, now: Timestamp) -> StepStatus {
        use crate::sequence::Outcome;
        if let Some(outcome) = self.done {
            return StepStatus::Done(outcome);
        }
        let mut status = match self.control.latest() {
            Err(_) => {
                let outcome = Outcome::Failed;
                self.done = Some(outcome);
                return StepStatus::Done(outcome);
            }
            Ok(Some(f)) if f.cancel != 0 => StepStatus::Done(Outcome::Aborted),
            _ => self.inner.step(now),
        };
        let run_state = match &status {
            StepStatus::Running => 0,
            StepStatus::Done(outcome) => outcome.run_state(),
        };
        if crate::sequence::publish_status(&mut self.status, now, run_state, &[]).is_err() {
            status = StepStatus::Done(Outcome::Failed);
        }
        if let StepStatus::Done(outcome) = status {
            self.done = Some(outcome);
        }
        status
    }

    fn shutdown(&mut self) {
        self.inner.shutdown()
    }
}

/// Wrap `bind_inner`'s driver per the mount: wired entries run bare;
/// slot occupants gain the framework tail — the cancel input bound after the
/// inner inputs and the status output bound after the inner outputs, the
/// order the host appends the occupant tail in.
pub(crate) fn mount_driver(
    src: &mut AnySource,
    mount: crate::pack::Mount,
    bind_inner: impl FnOnce(&mut AnySource) -> Box<dyn Driver>,
) -> Box<dyn Driver> {
    match mount {
        crate::pack::Mount::Wired => bind_inner(src),
        crate::pack::Mount::SlotOccupant => {
            let inner = bind_inner(src);
            let control = crate::port::Input::bind(src);
            let status = crate::port::Output::bind(src);
            Box::new(OccupantCyclic {
                inner,
                control,
                status,
                done: None,
            })
        }
    }
}
