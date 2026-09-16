//! The [`Ctor`] trait, which builds a state from a plain constructor fn.

use serde::de::DeserializeOwned;

use crate::coordinator::{ParamError, Params};

/// A `Ctor` builds a system's state from its params; `M` tells a `Fn() -> S` from a `Fn(P) -> S`.
pub trait Ctor<S, M> {
    /// Builds the state, decoding `params` when the constructor takes them.
    fn make(&self, params: Params<'_>) -> Result<S, ParamError>;
}

impl<S, F: Fn() -> S> Ctor<S, ()> for F {
    fn make(&self, _params: Params<'_>) -> Result<S, ParamError> {
        Ok(self())
    }
}

impl<S, P: DeserializeOwned, F: Fn(P) -> S> Ctor<S, (P,)> for F {
    fn make(&self, params: Params<'_>) -> Result<S, ParamError> {
        Ok(self(params.decode()?))
    }
}
