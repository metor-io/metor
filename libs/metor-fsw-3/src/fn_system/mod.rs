//! Systems authored as an `execute` method, lowered onto the [`System`] trait.
//!
//! `#[system]` on an impl block emits [`SystemFn`] for the type; [`FnSystem`]
//! is the [`System`] the table registers for it. Every parameter of `execute`
//! is a [`Param`], bound positionally in parameter order.

mod ctor;
mod param;
mod set;

use core::marker::PhantomData;

use metor_proto::types::Timestamp;

use crate::system::{System, SystemDef};

pub use ctor::Ctor;
pub use param::{Param, Views, Writers};
pub use set::{InSet, OutSet, ParamSet};

/// A `SystemFn` is a type with an `execute` method whose parameters are the ports.
pub trait SystemFn: Sized + 'static {
    type Params: ParamSet;
    const NAME: &'static str;
    /// One name per parameter, in parameter order.
    const NAMES: &'static [&'static str];

    /// Calls `execute` with this cycle's parameter values.
    fn call(&mut self, items: <Self::Params as ParamSet>::Items<'_>);
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

    fn execute(
        &self,
        now: Timestamp,
        state: &mut S,
        inputs: &mut InSet<S>,
        outputs: &mut OutSet<S>,
    ) {
        state.call(S::Params::get(&mut inputs.0, &mut outputs.0, now));
    }
}

#[cfg(test)]
mod tests;
