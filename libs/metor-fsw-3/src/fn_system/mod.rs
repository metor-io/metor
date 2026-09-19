//! `FnSystem` allows you to author a system using just the execute function

mod ctor;
mod param;
mod set;

use core::marker::PhantomData;

use metor_proto::types::Timestamp;

use metor_fsw_3_ring::{NoWake, Notifier};

use crate::async_system::{AsyncSystem, Stop};
use crate::{
    log,
    system::{System, SystemDef},
};

pub use ctor::Ctor;
pub use param::{Cycle, Names, Param, Views, Writers};
pub use set::{InSet, LOG_PORT, OutSet};

/// A `Ports` names one authored system's parameters, which are its ports.
pub trait Ports: Sized + 'static {
    type Params: Param;
    const NAME: &'static str;
    /// One name per parameter, in parameter order.
    const NAMES: &'static [&'static str];
    /// The doc comment on the method, lines joined with newlines.
    const DOC: &'static str = "";
}

/// A `SystemFn` is a type with an `execute` method whose parameters are the ports.
pub trait SystemFn: Ports {
    /// Calls `execute` with this cycle's parameter values.
    fn call(&mut self, items: <Self::Params as Param>::Item<'_, NoWake>);
}

/// An `AsyncSystemFn` is a type with an `async run` method whose parameters are
/// the ports, plus the [`Stop`] that ends it.
#[allow(async_fn_in_trait)]
pub trait AsyncSystemFn: Ports {
    /// Calls `run`, which returns once `stop` resolves.
    async fn call(&mut self, items: <Self::Params as Param>::Item<'_, Notifier>, stop: Stop);
}

/// Compiles only when `P` is a `Param`; `#[system]` calls it once per parameter.
#[doc(hidden)]
#[inline(always)]
pub fn assert_param<P: Param>() {}

/// A `FnSystem` is the [`System`] a [`SystemFn`] registers as.
pub struct FnSystem<S>(PhantomData<S>);

impl<S> Default for FnSystem<S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

/// A `FnAsyncSystem` is the [`AsyncSystem`] an [`AsyncSystemFn`] registers as.
pub struct FnAsyncSystem<S>(PhantomData<S>);

impl<S> Default for FnAsyncSystem<S> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<S: AsyncSystemFn> AsyncSystem for FnAsyncSystem<S> {
    type State = S;
    type Inputs = InSet<S, Notifier>;
    type Outputs = OutSet<S>;

    fn def() -> SystemDef {
        SystemDef::new_async::<InSet<S, Notifier>, OutSet<S>>(S::NAME)
    }

    /// Points each poll's log lines at the system's own `log` output, then runs `run`.
    async fn run(
        &self,
        state: &mut S,
        inputs: &mut InSet<S, Notifier>,
        outputs: &mut OutSet<S>,
        stop: Stop,
    ) {
        let mut log = log::Log;
        let mut cx = Cycle::new(Timestamp::now(), &mut log);
        let items = S::Params::get::<Notifier>(&mut inputs.0, &mut outputs.outs, &mut cx);
        let call = core::pin::pin!(state.call(items, stop));
        log::log_scope(&mut outputs.log, call).await;
    }
}

impl<S: SystemFn> System for FnSystem<S> {
    type State = S;
    type Inputs = InSet<S>;
    type Outputs = OutSet<S>;

    fn def() -> SystemDef {
        SystemDef::new::<InSet<S>, OutSet<S>>(S::NAME)
    }

    /// Points this thread's log lines at the system's own `log` output, then runs `execute`.
    fn execute(
        &self,
        now: Timestamp,
        state: &mut S,
        inputs: &mut InSet<S>,
        outputs: &mut OutSet<S>,
    ) {
        log::with_log_port(&mut outputs.log, now, || {
            let mut log = log::Log;
            let mut cx = Cycle::new(now, &mut log);
            state.call(S::Params::get(&mut inputs.0, &mut outputs.outs, &mut cx));
        });
    }

    /// Writes the panic as a fault line on the system's own `log` output.
    fn fault(&self, now: Timestamp, outputs: &mut OutSet<S>, message: &str) {
        outputs.log.fault(now, "panic", message.to_string());
    }
}

#[cfg(test)]
mod tests;
