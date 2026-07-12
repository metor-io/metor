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
/// cycle with a no-op waker, exactly the sequence execution model.
pub(crate) struct FutureDriver {
    future: core::pin::Pin<Box<dyn core::future::Future<Output = crate::sequence::Outcome>>>,
    clock: std::rc::Rc<crate::sequence::SeqClock>,
    health: HealthPort,
    /// A finished future is never polled again; the terminal is re-served.
    done: Option<crate::sequence::Outcome>,
}

impl FutureDriver {
    pub(crate) fn new(
        future: core::pin::Pin<Box<dyn core::future::Future<Output = crate::sequence::Outcome>>>,
        clock: std::rc::Rc<crate::sequence::SeqClock>,
        health: HealthPort,
    ) -> Self {
        Self {
            future,
            clock,
            health,
            done: None,
        }
    }
}

impl Driver for FutureDriver {
    fn init(&mut self) {}

    fn step(&mut self, now: Timestamp) -> StepStatus {
        use core::task::{Context, Poll, Waker};
        if let Some(outcome) = self.done {
            return StepStatus::Done(outcome);
        }
        self.clock.now.set(now);
        let start = std::time::Instant::now();
        let poll = crate::sequence::with_clock(&self.clock, || {
            self.future.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        });
        let micros = start.elapsed().as_micros() as u64;
        // A wired task has no SequenceStatus output; progress lines land on
        // the entry's ordinary log so they still flow off-board.
        for line in self.clock.drain_progress() {
            self.health.log(crate::health::Level::Info, &line);
        }
        self.health.end_cycle(now, micros);
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

/// The driver behind a sequence entry: [`FutureDriver`]'s poll-once model
/// plus the slot-occupant contract — the cancel latch folded from the
/// [`SlotControlIn`](crate::SlotControlIn) input before each poll, and a
/// [`SequenceStatus`](crate::sequence::SequenceStatus) record published after
/// it. This is the sequence execution model behind the pack seam.
pub(crate) struct SeqDriver {
    future: core::pin::Pin<Box<dyn core::future::Future<Output = crate::sequence::Outcome>>>,
    clock: std::rc::Rc<crate::sequence::SeqClock>,
    control: crate::port::Input<crate::sequence::SlotControlIn>,
    status: crate::Out<crate::sequence::SeqStatusOut>,
    done: Option<crate::sequence::Outcome>,
}

impl SeqDriver {
    pub(crate) fn new(
        future: core::pin::Pin<Box<dyn core::future::Future<Output = crate::sequence::Outcome>>>,
        clock: std::rc::Rc<crate::sequence::SeqClock>,
        control: crate::port::Input<crate::sequence::SlotControlIn>,
        status: crate::Out<crate::sequence::SeqStatusOut>,
    ) -> Self {
        Self {
            future,
            clock,
            control,
            status,
            done: None,
        }
    }
}

impl Driver for SeqDriver {
    fn init(&mut self) {}

    fn step(&mut self, now: Timestamp) -> StepStatus {
        use core::task::{Context, Poll, Waker};
        if let Some(outcome) = self.done {
            return StepStatus::Done(outcome);
        }
        self.clock.now.set(now);
        // A cancel stays latched once seen, even if later control frames
        // clear it.
        if let Some(f) = self.control.latest()
            && f.cancel != 0
        {
            self.clock.cancel.set(true);
        }
        let start = std::time::Instant::now();
        let poll = crate::sequence::with_clock(&self.clock, || {
            self.future.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        });
        let micros = start.elapsed().as_micros() as u64;
        let lines = self.clock.drain_progress();
        match poll {
            Poll::Ready(outcome) => {
                crate::sequence::publish_status(&mut self.status, now, outcome.run_state(), &lines);
                self.status.health().end_cycle(now, micros);
                self.done = Some(outcome);
                StepStatus::Done(outcome)
            }
            Poll::Pending => {
                crate::sequence::publish_status(&mut self.status, now, 0, &lines);
                self.status.health().end_cycle(now, micros);
                StepStatus::Running
            }
        }
    }

    fn shutdown(&mut self) {}
}
