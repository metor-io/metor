//! The [`Ctor`] trait, which builds a state from a plain constructor fn.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::coordinator::{ParamError, Params};

/// A `Ctor` builds a system's state from its params
pub trait Ctor<S, Marker> {
    /// Builds the state, decoding `params` when the constructor takes them.
    fn make(&self, params: Params<'_>) -> Result<S, ParamError>;

    /// The JSON Schema of the params type, or `None` when the constructor takes none.
    fn schema() -> Option<Box<RawValue>>;
}

impl<S, F: Fn() -> S> Ctor<S, ()> for F {
    fn make(&self, _params: Params<'_>) -> Result<S, ParamError> {
        Ok(self())
    }

    fn schema() -> Option<Box<RawValue>> {
        None
    }
}

impl<S, P: DeserializeOwned + JsonSchema, F: Fn(P) -> S> Ctor<S, (P,)> for F {
    fn make(&self, params: Params<'_>) -> Result<S, ParamError> {
        Ok(self(params.decode()?))
    }

    fn schema() -> Option<Box<RawValue>> {
        serde_json::value::to_raw_value(&schemars::schema_for!(P)).ok()
    }
}
