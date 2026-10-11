//! Systems written as a single `#[system]` fn.
//!
//! Each parameter of the fn is a port, or another value the system reads
//! each cycle, such as the cycle time. [`FnSystem`] turns the fn into a
//! [`System`] or an [`AsyncSystem`].

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

pub(crate) use ctor::make_state;
pub use ctor::{Ctor, WithIo};
pub use param::{Cycle, Param, ParamNames, Views, Writers};
pub(crate) use param::{Defs, param_name};
pub use set::{InSet, LOG_PORT, OutSet};

/// Describes the parameters of a system written as a fn.
pub trait FnSignature: Sized + 'static {
    /// The type of the `#[state]` parameter, or `()` if there is none.
    type State: 'static;
    type Params: Param;
    const NAME: &'static str;
    /// The name of each parameter, in order.
    const NAMES: &'static [&'static str];
    /// The fn's doc comment, with its lines joined by newlines.
    const DOC: &'static str = "";
}

/// A cyclic system written as a fn.
pub trait SystemFn: FnSignature {
    /// Runs one cycle.
    fn call(state: &mut Self::State, items: <Self::Params as Param>::Item<'_, NoWake>);
}

/// An async system written as a fn.
///
/// Besides its ports, the fn receives a [`Stop`] that tells it when to end.
#[allow(async_fn_in_trait)]
pub trait AsyncSystemFn: FnSignature {
    /// Runs the system until `stop` resolves.
    async fn call(
        state: &mut Self::State,
        items: <Self::Params as Param>::Item<'_, Notifier>,
        stop: Stop,
    );
}

/// Fails to compile unless `P` implements [`Param`].
///
/// `#[system]` calls this once per parameter.
#[doc(hidden)]
#[inline(always)]
pub fn assert_param<P: Param>() {}

/// Runs a fn system as a [`System`] or an [`AsyncSystem`].
///
/// A [`SystemFn`] becomes a `System`, and an [`AsyncSystemFn`] becomes an
/// `AsyncSystem`.
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

    /// Returns the system's definition.
    ///
    /// # Errors
    /// Refuses a [`Sample`](crate::port::Sample) parameter, because async
    /// systems have no step to take samples each cycle.
    ///
    /// Refuses a `Cap` parameter too: async systems keep
    /// stellarator's own IO.
    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        let defs = Defs::of::<S>(cx)?;
        let system = S::NAME.into();
        if defs.sampled {
            return Err(DefError::SampleOnAsync { system });
        }
        if !defs.grants.is_empty() {
            return Err(DefError::IoOnAsync { system });
        }
        if defs.rng {
            return Err(DefError::RngOnAsync { system });
        }
        set::system_def::<S, Notifier>(cx)
    }

    /// Runs the system, sending its log lines to its own `log` output.
    async fn run(
        &self,
        state: &mut S::State,
        inputs: &mut InSet<S, Notifier>,
        outputs: &mut OutSet<S>,
        stop: Stop,
    ) {
        let mut cx = Cycle::new(Timestamp::now());
        let items = S::Params::get::<Notifier>(&mut inputs.ins, &mut outputs.outs, &mut cx);
        let call = core::pin::pin!(S::call(state, items, stop));
        log::with_logs(&mut outputs.log, call).await;
    }
}

impl<S: SystemFn> System for FnSystem<S> {
    type State = S::State;
    type Inputs = InSet<S>;
    type Outputs = OutSet<S>;

    fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
        Ok(SystemDef {
            grants: Defs::of::<S>(cx)?.grants,
            ..set::system_def::<S, NoWake>(cx)?
        })
    }

    /// Runs one cycle, sending its log lines to the system's own `log` output.
    ///
    /// Samples are refreshed before the fn runs. Failed writes are reported as a
    /// fault afterward.
    fn execute(
        &self,
        now: Timestamp,
        state: &mut S::State,
        inputs: &mut InSet<S>,
        outputs: &mut OutSet<S>,
    ) {
        log::with_log_port(&mut outputs.log, now, || {
            S::Params::refresh(&mut inputs.ins);
            let mut cx = Cycle::new(now);
            S::call(
                state,
                S::Params::get(&mut inputs.ins, &mut outputs.outs, &mut cx),
            );
        });
        outputs.report_failed_writes(now);
    }

    /// Logs a panic as a fault on the system's own `log` output.
    fn fault(&self, now: Timestamp, outputs: &mut OutSet<S>, message: &str) {
        outputs.log.fault(now, "panic", message.to_string());
    }
}

#[cfg(all(test, feature = "host"))]
mod tests;
