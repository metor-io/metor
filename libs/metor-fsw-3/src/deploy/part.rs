//! A typed view of some of a system's params.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::DeployError;
use crate::coordinator::SystemConfig;

/// The fields of `T` in a system's params, and the rest untouched.
///
/// `rest` holds every key as read, and `write` lays `part` over it. A derived
/// `#[serde(flatten)]` pair would hand `rest` the fields of a `T` that itself
/// flattens, and write their stale values back.
pub(crate) struct Part<T> {
    pub(crate) part: T,
    rest: Map<String, Value>,
}

impl<T: Serialize + DeserializeOwned> Part<T> {
    /// Decodes `T` out of `system`'s params.
    pub(crate) fn read(system: &SystemConfig) -> Result<Self, DeployError> {
        let Value::Object(rest) = &system.params else {
            return Err(params_error(system, "params are not an object"));
        };
        let part = T::deserialize(&system.params).map_err(|e| params_error(system, e))?;
        Ok(Self {
            part,
            rest: rest.clone(),
        })
    }

    /// Writes `part`'s fields into `system`'s params, keeping every other key.
    pub(crate) fn write(self, system: &mut SystemConfig) -> Result<(), DeployError> {
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

fn params_error(system: &SystemConfig, message: impl ToString) -> DeployError {
    DeployError::Params {
        system: system.id.clone(),
        message: message.to_string(),
    }
}
