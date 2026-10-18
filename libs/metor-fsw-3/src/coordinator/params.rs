//! The config value a system is built from.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// One system's raw config value.
///
/// A system decodes it once, at build time, with [`decode`](Params::decode).
#[derive(Clone, Copy, Debug)]
pub struct Params<'a>(pub &'a Value);

impl Params<'_> {
    /// Decodes the value as `P`.
    ///
    /// A null value is treated as an empty object. Keys that `P` does not know
    /// about are an error.
    pub fn decode<P: DeserializeOwned>(self) -> Result<P, ParamError> {
        let empty = Value::Object(serde_json::Map::new());
        let value = if self.0.is_null() { &empty } else { self.0 };
        let mut unknown = None;
        let decoded = serde_ignored::deserialize(value, |path| {
            unknown.get_or_insert_with(|| path.to_string());
        })
        .map_err(|e: serde_json::Error| ParamError::Decode(e.to_string()))?;
        match unknown {
            Some(key) => Err(ParamError::UnknownKey(key)),
            None => Ok(decoded),
        }
    }
}

/// A system's config params that do not decode.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum ParamError {
    #[error("unknown key `{0}`")]
    UnknownKey(String),
    #[error("{0}")]
    Decode(String),
}

/// An error from making a system.
///
/// Packs send this error back to the host as JSON, so it derives serde.
#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum MakeError {
    #[error(transparent)]
    Params(#[from] ParamError),
    #[error("thread `{thread}` is not running")]
    ThreadStart { thread: String },
    #[error("the constructor panicked: {0}")]
    Panicked(String),
    #[error("a mirror of a {capacity}-byte ring is too large")]
    MirrorTooLarge { capacity: usize },
    #[error("`{system}`: an async system needs OS threads, which this build has none of")]
    ThreadsUnsupported { system: String },
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[derive(Deserialize, Debug, PartialEq)]
    struct Gains {
        #[serde(default = "one")]
        kp: f64,
        #[serde(default)]
        ki: f64,
    }

    fn one() -> f64 {
        1.0
    }

    #[derive(Deserialize, Debug, PartialEq)]
    struct Required {
        seed: u64,
    }

    #[test]
    fn test_null_params_use_defaults() {
        let gains: Gains = Params(&Value::Null).decode().expect("all defaulted");
        assert_eq!(gains, Gains { kp: 1.0, ki: 0.0 });
    }

    #[test]
    fn test_params_override_defaults() {
        let value = json!({ "ki": 0.5 });
        let gains: Gains = Params(&value).decode().expect("valid");
        assert_eq!(gains, Gains { kp: 1.0, ki: 0.5 });
    }

    #[test]
    fn test_reject_missing_required_field() {
        let err = Params(&Value::Null).decode::<Required>().unwrap_err();
        assert!(matches!(err, ParamError::Decode(ref m) if m.contains("seed")));
    }

    #[test]
    fn test_unknown_key_error() {
        let value = json!({ "seed": 1, "sead": 2 });
        assert_eq!(
            Params(&value).decode::<Required>(),
            Err(ParamError::UnknownKey("sead".into()))
        );
    }

    #[test]
    fn test_nested_unknown_key_path() {
        #[derive(Deserialize)]
        struct Outer {
            #[allow(dead_code)]
            inner: Required,
        }
        let value = json!({ "inner": { "seed": 1, "typo": 2 } });
        assert_eq!(
            Params(&value).decode::<Outer>().err(),
            Some(ParamError::UnknownKey("inner.typo".into()))
        );
    }
}
