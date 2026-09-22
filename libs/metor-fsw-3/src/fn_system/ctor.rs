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

/// A constructor that can refuse its params, such as one binding a socket.
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

    /// The turbofish a fallible registration needs, since `Fn(P) -> S` also matches.
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
