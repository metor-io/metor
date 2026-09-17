//! `FnSystem` allows you to author a system using just the execute function

mod ctor;
mod param;
mod set;

use core::marker::PhantomData;

use metor_proto::types::Timestamp;

use crate::{
    log,
    system::{System, SystemDef},
};

pub use ctor::Ctor;
pub use param::{Cycle, Names, Param, Views, Writers};
pub use set::{InSet, LOG_PORT, OutSet};

/// A `SystemFn` is a type with an `execute` method whose parameters are the ports.
pub trait SystemFn: Sized + 'static {
    type Params: Param;
    const NAME: &'static str;
    /// One name per parameter, in parameter order.
    const NAMES: &'static [&'static str];
    /// The doc comment on `execute`, lines joined with newlines.
    const DOC: &'static str = "";

    /// Calls `execute` with this cycle's parameter values.
    fn call(&mut self, items: <Self::Params as Param>::Item<'_>);
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
        let _guard = log::enter(&mut outputs.log, now);
        let mut log = log::Log;
        let mut cx = Cycle::new(now, &mut log);
        state.call(S::Params::get(&mut inputs.0, &mut outputs.outs, &mut cx));
    }

    /// Writes the panic as a fault line on the system's own `log` output.
    fn fault(&self, now: Timestamp, outputs: &mut OutSet<S>, message: &str) {
        outputs.log.fault(now, "panic", message.to_string());
    }
}

#[cfg(test)]
mod tests;
