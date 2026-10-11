//! The [`Ctor`] trait, which lets a plain fn build a system's state.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::coordinator::{ParamError, Params};

/// Builds a system's state from its config params.
///
/// It is implemented for fns that take no params, fns that take a
/// deserializable params type, and fns that take params and may fail with a
/// [`ParamError`]. `Marker` keeps these impls from overlapping.
pub trait Ctor<S, Marker> {
    /// Builds the state.
    ///
    /// Decodes `params` first if the constructor takes them.
    fn make(&self, params: Params<'_>) -> Result<S, ParamError>;

    /// Returns the JSON Schema of the params type.
    ///
    /// Returns `None` if the constructor takes no params.
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

/// A constructor that can reject its params, such as one that binds a socket.
impl<S, P: DeserializeOwned + JsonSchema, F: Fn(P) -> Result<S, ParamError>>
    Ctor<S, (P, ParamError)> for F
{
    fn make(&self, params: Params<'_>) -> Result<S, ParamError> {
        self(params.decode()?)
    }

    fn schema() -> Option<Box<RawValue>> {
        serde_json::value::to_raw_value(&schemars::schema_for!(P)).ok()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[derive(Deserialize, JsonSchema)]
    struct Port {
        port: u16,
    }

    fn build(params: Port) -> Result<u16, ParamError> {
        match params.port {
            0 => Err(ParamError::Decode("port zero".into())),
            port => Ok(port),
        }
    }

    /// Calls `build` as a fallible constructor.
    ///
    /// The turbofish is needed because `Fn(P) -> S` also matches `build`.
    fn ctor(params: &serde_json::Value) -> Result<u16, ParamError> {
        Ctor::<u16, (Port, ParamError)>::make(&build, Params(params))
    }

    #[test]
    fn test_fallible_constructor() {
        assert_eq!(ctor(&json!({ "port": 7 })), Ok(7));
    }

    #[test]
    fn test_constructor_error() {
        assert_eq!(
            ctor(&json!({ "port": 0 })),
            Err(ParamError::Decode("port zero".into()))
        );
    }

    #[test]
    fn test_invalid_params_skip_constructor() {
        assert!(matches!(
            ctor(&json!({ "port": "eight" })),
            Err(ParamError::Decode(_))
        ));
        let schema =
            <fn(Port) -> Result<u16, ParamError> as Ctor<u16, (Port, ParamError)>>::schema();
        assert!(schema.expect("a params schema").get().contains("port"));
    }
}
