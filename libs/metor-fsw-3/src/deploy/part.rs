//! Typed access to part of a system's params.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::DeployError;
use crate::coordinator::SystemConfig;

/// A system's params, decoded as `T`, with every other key kept as is.
///
/// This lets a pass edit a few typed fields and write them back without
/// dropping keys `T` does not know about. It does not use a
/// `#[serde(flatten)]` pair, because when `T` itself flattens, serde would put
/// `T`'s fields in the leftover map too, and writing it back would restore
/// their old values.
pub(crate) struct Part<T> {
    pub(crate) part: T,
    rest: Map<String, Value>,
}

impl<T: Serialize + DeserializeOwned> Part<T> {
    /// Decodes `T` from `system`'s params.
    pub(crate) fn read<O>(system: &SystemConfig<O>) -> Result<Self, DeployError> {
        let Value::Object(rest) = &system.params else {
            return Err(params_error(system, "params are not an object"));
        };
        let part = T::deserialize(&system.params).map_err(|e| params_error(system, e))?;
        Ok(Self {
            part,
            rest: rest.clone(),
        })
    }

    /// Writes `part`'s fields back into `system`'s params. Every other key is
    /// kept.
    pub(crate) fn write<O>(self, system: &mut SystemConfig<O>) -> Result<(), DeployError> {
        let Value::Object(fields) =
            serde_json::to_value(&self.part).map_err(|e| params_error(system, e))?
        else {
            return Err(params_error(system, "params encode to a non-object"));
        };
        let mut params = self.rest;
        params.extend(fields);
        system.params = Value::Object(params);
        Ok(())
    }
}

fn params_error<O>(system: &SystemConfig<O>, message: impl ToString) -> DeployError {
    DeployError::Params {
        system: system.id.clone(),
        message: message.to_string(),
    }
}
