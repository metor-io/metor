//! The driver behind a fn-authored system: owns the state and the port
//! states between cycles and generalizes [`CyclicRunner`]'s step timing and
//! health fold to the parameter-tuple world.
//!
//! [`CyclicRunner`]: crate::CyclicRunner

use core::marker::PhantomData;

use metor_proto::types::Timestamp;

use crate::binder::AnySource;
use crate::health::{HealthPort, SystemHealth, SystemLog};
use crate::pack::{Driver, StepStatus};
use crate::port::Output;

use super::param::{BindCx, CycleCx};
use super::tuples::{ExecParamSet, ExecuteFn};

pub(crate) struct FnDriver<S, M, F>
where
    F: ExecuteFn<S, M>,
{
    state: S,
    execute: F,
    ports: <F::Params as ExecParamSet>::State,
    health: HealthPort,
    _m: PhantomData<fn() -> M>,
}

impl<S, M, F> FnDriver<S, M, F>
where
    S: 'static,
    F: ExecuteFn<S, M>,
{
    /// Bind the parameter states in declaration order, then the implicit
    /// health/log tail — the same order the entry's descriptor declares.
    pub(crate) fn bind(state: S, execute: F, src: &mut AnySource) -> Self {
        let ports = {
            let mut cx = BindCx {
                src,
                params: None,
                clock: None,
                drops: None,
            };
            <F::Params as ExecParamSet>::bind(&mut cx)
        };
        Self {
            state,
            execute,
            ports,
            health: bind_health_tail(src),
            _m: PhantomData,
        }
    }
}

/// Bind the implicit health/log output pair every entry's descriptor
/// appends, after the user ports.
pub(crate) fn bind_health_tail(src: &mut AnySource) -> HealthPort {
    let health: Output<SystemHealth> = Output::bind(src);
    let log: Output<SystemLog> = Output::bind(src);
    HealthPort::new(health, log)
}

impl<S, M, F> Driver for FnDriver<S, M, F>
where
    S: 'static,
    F: ExecuteFn<S, M>,
{
    fn init(&mut self) {}

    fn step(&mut self, now: Timestamp) -> StepStatus {
        let start = std::time::Instant::now();
        {
            let mut cx = CycleCx {
                now,
                health: Some(&mut self.health),
            };
            let items = <F::Params as ExecParamSet>::get(&mut self.ports, &mut cx);
            self.execute.call(&mut self.state, items);
        }
        let micros = start.elapsed().as_micros() as u64;
        if <F::Params as ExecParamSet>::take_dropped(&mut self.ports) > 0 {
            self.health.error("publish_dropped");
        }
        self.health.end_cycle(now, micros);
        StepStatus::Running
    }

    fn shutdown(&mut self) {}
}

/// The driver behind an async-fn (task) entry: the future owns its ports and
/// its state (locals); the driver refreshes the clock and polls once per
/// cycle with a no-op waker, exactly the sequence execution model. Drops
/// from the future-owned outputs arrive through the shared cell and fold
/// into health, same as a runner-owned bundle's.
pub(crate) struct FutureDriver {
    future: core::pin::Pin<Box<dyn core::future::Future<Output = crate::sequence::Outcome>>>,
    clock: std::rc::Rc<crate::sequence::CycleClock>,
    health: HealthPort,
    drops: std::sync::Arc<core::sync::atomic::AtomicU64>,
    /// A finished future is never polled again; the terminal is re-served.
    done: Option<crate::sequence::Outcome>,
}

impl FutureDriver {
    pub(crate) fn new(
        future: core::pin::Pin<Box<dyn core::future::Future<Output = crate::sequence::Outcome>>>,
        clock: std::rc::Rc<crate::sequence::CycleClock>,
        health: HealthPort,
        drops: std::sync::Arc<core::sync::atomic::AtomicU64>,
    ) -> Self {
        Self {
            future,
            clock,
            health,
            drops,
            done: None,
        }
    }

    /// Poll once under the ambient clock, timing the poll and folding the
    /// shared drop count; the caller decides what happens to progress lines
    /// and the outcome.
    fn poll_once(&mut self, now: Timestamp) -> core::task::Poll<crate::sequence::Outcome> {
        use core::task::{Context, Waker};
        self.clock.now.set(now);
        let start = std::time::Instant::now();
        let poll = crate::sequence::with_clock(&self.clock, || {
            self.future.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        });
        let micros = start.elapsed().as_micros() as u64;
        if self.drops.swap(0, core::sync::atomic::Ordering::Relaxed) > 0 {
            self.health.error("publish_dropped");
        }
        self.health.end_cycle(now, micros);
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
            self.health.log(crate::health::Level::Info, &line);
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
/// after the user inputs; status after the health/log tail), matching the
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
        if let Some(f) = self.control.latest()
            && f.cancel != 0
        {
            self.inner.clock.cancel.set(true);
        }
        let poll = self.inner.poll_once(now);
        let lines = self.inner.clock.drain_progress();
        match poll {
            Poll::Ready(outcome) => {
                crate::sequence::publish_status(&mut self.status, now, outcome.run_state(), &lines);
                self.inner.done = Some(outcome);
                StepStatus::Done(outcome)
            }
            Poll::Pending => {
                crate::sequence::publish_status(&mut self.status, now, 0, &lines);
                StepStatus::Running
            }
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
        if let Some(f) = self.control.latest()
            && f.cancel != 0
        {
            let outcome = Outcome::Aborted;
            crate::sequence::publish_status(&mut self.status, now, outcome.run_state(), &[]);
            self.done = Some(outcome);
            return StepStatus::Done(outcome);
        }
        let status = self.inner.step(now);
        match status {
            StepStatus::Done(outcome) => {
                crate::sequence::publish_status(&mut self.status, now, outcome.run_state(), &[]);
                self.done = Some(outcome);
            }
            StepStatus::Running => {
                crate::sequence::publish_status(&mut self.status, now, 0, &[]);
            }
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
