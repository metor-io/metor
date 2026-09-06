//! Drivers for function and future-based pack entries.
//!
//! Drivers own ports, flush logs, and report dropped output records. Future
//! drivers poll once per cycle under the sequence clock.

use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use core::task::{Context, Poll, Waker};
use std::rc::Rc;
use std::sync::Arc;

use metor_proto::types::Timestamp;

use crate::Shared;
use crate::binder::{AnySource, RingSource};
use crate::log::{LogEvent, LogLevel, LogPort};
use crate::message::MsgOut;
use crate::pack::{Driver, Mount, StepStatus};
use crate::port::{Input, Output};
use crate::sequence::{self, CycleClock, Outcome, SequenceStatus, SlotControlIn};
use crate::system::report_dropped;

use super::param::{BindCx, CycleCx};
use super::tuples::{ExecParamSet, ExecuteFn};

/// How a fn-authored driver reaches its state: owned outright (the zero-cost
/// default), or the scoped borrow of a pack-shared instance.
pub(crate) enum StateAccess<S> {
    Owned(S),
    Shared(Shared<S>),
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
    _marker: PhantomData<fn() -> M>,
}

impl<S, M, F> FnDriver<S, M, F>
where
    S: 'static,
    F: ExecuteFn<S, M>,
{
    /// Bind the parameter states in declaration order, then the implicit
    /// log tail, the same order the entry's descriptor declares.
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
            _marker: PhantomData,
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

/// Poll a task once per cycle, tracking its outcome and output drops.
pub(crate) struct FutureDriver {
    future: Pin<Box<dyn Future<Output = Outcome>>>,
    clock: Rc<CycleClock>,
    log: LogPort,
    drops: Arc<AtomicU64>,
    /// A finished future is never polled again; the terminal is re-served.
    done: Option<Outcome>,
}

impl FutureDriver {
    pub(crate) fn new(
        future: Pin<Box<dyn Future<Output = Outcome>>>,
        clock: Rc<CycleClock>,
        log: LogPort,
        drops: Arc<AtomicU64>,
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
    fn poll_once(&mut self, now: Timestamp) -> Poll<Outcome> {
        self.clock.now.set(now);
        let poll = sequence::with_clock(&self.clock, || {
            self.future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        });
        let dropped = self.drops.swap(0, Relaxed);
        report_dropped(&mut self.log, dropped);
        self.log.flush(now);
        poll
    }
}

impl Driver for FutureDriver {
    fn init(&mut self) {}

    fn step(&mut self, now: Timestamp) -> StepStatus {
        if let Some(outcome) = self.done {
            return StepStatus::Done(outcome);
        }
        let poll = self.poll_once(now);
        // A wired task has no SequenceStatus output; progress lines land on
        // the entry's ordinary log so they still flow off-board.
        for line in self.clock.drain_progress() {
            self.log.log(LogLevel::Info, &line);
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

/// Add cancellation and sequence status to a future-driven entry.
///
/// Control and status bind after the entry's own ports, matching the
/// occupant descriptor's appended ports.
pub(crate) struct OccupantFuture {
    inner: FutureDriver,
    control: Input<SlotControlIn>,
    status: Output<SequenceStatus>,
}

impl OccupantFuture {
    pub(crate) fn new(
        inner: FutureDriver,
        control: Input<SlotControlIn>,
        status: Output<SequenceStatus>,
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
        if let Some(outcome) = self.inner.done {
            return StepStatus::Done(outcome);
        }
        // A cancel stays latched once seen, even if later control frames
        // clear it.
        match self.control.latest() {
            Ok(Some(f)) if f.cancel != 0 => self.inner.clock.cancel.set(true),
            Err(_) => {
                let outcome = Outcome::Failed;
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
        let run_state = outcome.map_or(0, Outcome::run_state);
        if sequence::publish_status(&mut self.status, now, run_state, &lines).is_err() {
            outcome = Some(Outcome::Failed);
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
/// `Aborted`, the hard-but-clean stop; cooperative cancellation is the
/// async style's domain. A `SequenceStatus` record is published every step
/// so the slot runner's status tap works for any occupant.
pub(crate) struct OccupantCyclic {
    inner: Box<dyn Driver>,
    control: Input<SlotControlIn>,
    status: Output<SequenceStatus>,
    done: Option<Outcome>,
}

impl Driver for OccupantCyclic {
    fn init(&mut self) {
        self.inner.init()
    }

    fn step(&mut self, now: Timestamp) -> StepStatus {
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
        if sequence::publish_status(&mut self.status, now, run_state, &[]).is_err() {
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
/// slot occupants gain the framework tail, the cancel input bound after the
/// inner inputs and the status output bound after the inner outputs, the
/// order the host appends the occupant tail in.
pub(crate) fn mount_driver(
    src: &mut AnySource,
    mount: Mount,
    bind_inner: impl FnOnce(&mut AnySource) -> Box<dyn Driver>,
) -> Box<dyn Driver> {
    match mount {
        Mount::Wired => bind_inner(src),
        Mount::SlotOccupant => {
            let inner = bind_inner(src);
            let control = Input::bind(src);
            let status = Output::bind(src);
            Box::new(OccupantCyclic {
                inner,
                control,
                status,
                done: None,
            })
        }
    }
}
