//! `FnSystem` allows you to author a system as one `#[system]` fn.

mod ctor;
mod param;
mod set;

use core::marker::PhantomData;

use metor_proto::types::Timestamp;

use metor_fsw_3_ring::{NoWake, Notifier};

use crate::async_system::{AsyncSystem, Stop};
use crate::def::{DefCx, DefError};
use crate::{
    log,
    system::{System, SystemDef},
};

pub use ctor::Ctor;
use param::Defs;
pub use param::{Cycle, Param, ParamNames, Views, Writers};
pub use set::{InSet, LOG_PORT, OutSet};

/// A `Ports` names one authored system's parameters, which are its ports.
pub trait Ports: Sized + 'static {
    /// The `#[state]` parameter's type, or `()`.
    type State: 'static;
    type Params: Param;
    const NAME: &'static str;
    /// One name per parameter, in parameter order.
    const NAMES: &'static [&'static str];
    /// The doc comment on the fn, lines joined with newlines.
    const DOC: &'static str = "";
}

/// A `SystemFn` is a cyclic system whose parameters are the ports.
pub trait SystemFn: Ports {
    /// Runs one cycle with this cycle's parameter values.
    fn call(state: &mut Self::State, items: <Self::Params as Param>::Item<'_, NoWake>);
}

/// An `AsyncSystemFn` is an async system whose parameters are the ports, plus
/// the [`Stop`] that ends it.
#[allow(async_fn_in_trait)]
pub trait AsyncSystemFn: Ports {
    /// Runs the system, returning once `stop` resolves.
    async fn call(
        state: &mut Self::State,
        items: <Self::Params as Param>::Item<'_, Notifier>,
        stop: Stop,
    );
}

/// Compiles only when `P` is a `Param`; `#[system]` calls it once per parameter.
#[doc(hidden)]
#[inline(always)]
pub fn assert_param<P: Param>() {}

/// A `FnSystem` is the [`System`] a [`SystemFn`] registers as, or the
/// [`AsyncSystem`] an [`AsyncSystemFn`] does.
pub struct FnSystem<S>(PhantomData<S>);

impl<S> Default for FnSystem<S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<S: AsyncSystemFn> AsyncSystem for FnSystem<S> {
    type State = S::State;
    type Inputs = InSet<S, Notifier>;
    type Outputs = OutSet<S>;

    /// Refuses a [`Sample`](crate::port::Sample) parameter, since no step takes it.
    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        if Defs::of::<S>(cx)?.sampled {
            return Err(DefError::SampleOnAsync {
                system: S::NAME.into(),
            });
        }
        SystemDef::new::<InSet<S, Notifier>, OutSet<S>, _>(S::NAME, cx)
    }

    /// Points each poll's log lines at the system's own `log` output, then runs the system.
    async fn run(
        &self,
        state: &mut S::State,
        inputs: &mut InSet<S, Notifier>,
        outputs: &mut OutSet<S>,
        stop: Stop,
    ) {
        let mut log = log::Log;
        let mut cx = Cycle::new(Timestamp::now(), &mut log);
        let items = S::Params::get::<Notifier>(&mut inputs.ins, &mut outputs.outs, &mut cx);
        let call = core::pin::pin!(S::call(state, items, stop));
        log::log_scope(&mut outputs.log, call).await;
    }
}

impl<S: SystemFn> System for FnSystem<S> {
    type State = S::State;
    type Inputs = InSet<S>;
    type Outputs = OutSet<S>;

    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        SystemDef::new::<InSet<S>, OutSet<S>, _>(S::NAME, cx)
    }

    /// Points this thread's log lines at the system's own `log` output, then runs one cycle.
    fn execute(
        &self,
        now: Timestamp,
        state: &mut S::State,
        inputs: &mut InSet<S>,
        outputs: &mut OutSet<S>,
    ) {
        log::with_log_port(&mut outputs.log, now, || {
            inputs.samplers.take_all();
            let mut log = log::Log;
            let mut cx = Cycle::new(now, &mut log);
            S::call(
                state,
                S::Params::get(&mut inputs.ins, &mut outputs.outs, &mut cx),
            );
        });
    }

    /// Writes the panic as a fault line on the system's own `log` output.
    fn fault(&self, now: Timestamp, outputs: &mut OutSet<S>, message: &str) {
        outputs.log.fault(now, "panic", message.to_string());
    }
}

#[cfg(test)]
mod tests;
