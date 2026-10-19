//! The [`Ctor`] trait, which lets a plain fn build a system's state.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::coordinator::{MakeError, Params};

/// Builds a system's state from its config params.
///
/// It is implemented for fns that take no params, fns that take a
/// deserializable params type, and fns that take params and may fail with a
/// [`MakeError`]. `Marker` keeps these impls from overlapping.
pub trait Ctor<S, Marker> {
    /// Builds the state.
    ///
    /// Decodes `params` first if the constructor takes them.
    fn make(&self, params: Params<'_>) -> Result<S, MakeError>;

    /// Returns the JSON Schema of the params type.
    ///
    /// Returns `None` if the constructor takes no params.
    fn schema() -> Option<Box<RawValue>>;
}

fn schema_of<P: JsonSchema>() -> Option<Box<RawValue>> {
    serde_json::value::to_raw_value(&schemars::schema_for!(P)).ok()
}

impl<S, F: Fn() -> S> Ctor<S, ()> for F {
    fn make(&self, _params: Params<'_>) -> Result<S, MakeError> {
        Ok(self())
    }

    fn schema() -> Option<Box<RawValue>> {
        None
    }
}

impl<S, P: DeserializeOwned + JsonSchema, F: Fn(P) -> S> Ctor<S, (P,)> for F {
    fn make(&self, params: Params<'_>) -> Result<S, MakeError> {
        Ok(self(params.decode()?))
    }

    fn schema() -> Option<Box<RawValue>> {
        schema_of::<P>()
    }
}

/// A constructor that can reject its params, such as one that binds a socket.
impl<S, P: DeserializeOwned + JsonSchema, F: Fn(P) -> Result<S, MakeError>> Ctor<S, (P, MakeError)>
    for F
{
    fn make(&self, params: Params<'_>) -> Result<S, MakeError> {
        self(params.decode()?)
    }

    fn schema() -> Option<Box<RawValue>> {
        schema_of::<P>()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;
    use crate::coordinator::ParamError;

    #[derive(Deserialize, JsonSchema)]
    struct Port {
        port: u16,
    }

    fn build(params: Port) -> Result<u16, MakeError> {
        match params.port {
            0 => Err(ParamError::Decode("port zero".into()).into()),
            port => Ok(port),
        }
    }

    /// Calls `build` as a fallible constructor.
    ///
    /// The turbofish is needed because `Fn(P) -> S` also matches `build`.
    fn ctor(params: &serde_json::Value) -> Result<u16, MakeError> {
        Ctor::<u16, (Port, MakeError)>::make(&build, Params(params))
    }

    #[test]
    fn test_fallible_constructor() {
        assert_eq!(ctor(&json!({ "port": 7 })), Ok(7));
    }

    #[test]
    fn test_constructor_error() {
        assert_eq!(
            ctor(&json!({ "port": 0 })),
            Err(ParamError::Decode("port zero".into()).into())
        );
    }

    #[test]
    fn test_invalid_params_skip_constructor() {
        assert!(matches!(
            ctor(&json!({ "port": "eight" })),
            Err(MakeError::Params(ParamError::Decode(_)))
        ));
        let schema = <fn(Port) -> Result<u16, MakeError> as Ctor<u16, (Port, MakeError)>>::schema();
        assert!(schema.expect("a params schema").get().contains("port"));
    }
}
