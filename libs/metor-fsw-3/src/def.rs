//! The config context a system uses to compute its [`SystemDef`](crate::SystemDef).
//!
//! Most systems declare fixed ports and ignore it. Systems whose ports come
//! from config, like links, read their ports from it.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::coordinator::{OutputConfig, OutputSource};
use crate::system::PortDef;

/// The part of one instance's config a system sees when computing its ports.
pub struct DefCx<'a> {
    /// The ID of the instance being defined.
    pub id: &'a str,
    /// The target's namespace.
    ///
    /// Every name the instance announces is prefixed with it.
    pub namespace: Option<&'a str>,
    /// The instance's connected inputs.
    ///
    /// Each entry pairs an input port name with the definition of the output
    /// that feeds it.
    pub inputs: &'a [(&'a str, &'a PortDef)],
    /// The output ports listed in the config, with the record each one carries.
    pub outputs: &'a [OutputConfig],
    /// Every record the system table knows about.
    pub records: &'a Records,
    /// The instance's params from the config.
    pub params: &'a serde_json::Value,
}

static NO_RECORDS: LazyLock<Records> = LazyLock::new(Records::default);
static NO_PARAMS: serde_json::Value = serde_json::Value::Null;

impl DefCx<'static> {
    /// Returns a context with nothing in it.
    ///
    /// A system's definition under this context is the one its descriptor
    /// carries.
    pub fn empty() -> Self {
        Self {
            id: "",
            namespace: None,
            inputs: &[],
            outputs: &[],
            records: &NO_RECORDS,
            params: &NO_PARAMS,
        }
    }
}

impl DefCx<'_> {
    /// Returns the path that port `port` announces its components under.
    ///
    /// The path is `{namespace}.{id}.{port}`, leaving out any part that is
    /// empty.
    pub fn port_path(&self, port: &str) -> String {
        Self::path(self.namespace, self.id, port)
    }

    /// Returns definitions for config inputs that no declared port claimed.
    ///
    /// Each port takes its name from the config and its record from its producer.
    pub fn dyn_input_defs(&self, declared: &[Cow<'static, str>]) -> Result<Vec<PortDef>, DefError> {
        let mut ports: Vec<PortDef> = Vec::new();
        for (name, producer) in self.inputs {
            if declared.iter().any(|taken| taken == name) {
                continue;
            }
            if ports.iter().any(|port| port.name == *name) {
                return Err(DefError::FanIn {
                    port: name.to_string(),
                });
            }
            ports.push(PortDef {
                name: name.to_string().into(),
                ..(*producer).clone()
            });
        }
        Ok(ports)
    }

    /// Returns definitions for config outputs that no declared port claimed.
    ///
    /// Each port's record comes from the record the config names, or from the
    /// definition `metor build` resolved for it.
    pub fn dyn_output_defs(
        &self,
        declared: &[Cow<'static, str>],
    ) -> Result<Vec<PortDef>, DefError> {
        self.outputs
            .iter()
            .filter(|output| !declared.iter().any(|taken| *taken == output.port))
            .map(|output| match &output.source {
                OutputSource::Record(record) => self.records.port_def(&output.port, record),
                OutputSource::Def(def) => Ok(PortDef {
                    name: output.port.clone().into(),
                    ..def.clone()
                }),
            })
            .collect()
    }

    /// Returns `{namespace}.{id}.{port}`, leaving out any part that is empty.
    pub(crate) fn path(namespace: Option<&str>, id: &str, port: &str) -> String {
        [namespace.unwrap_or(""), id, port]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// An error from computing a system's ports from its config.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum DefError {
    #[error("input `{port}` takes one producer")]
    FanIn { port: String },

    #[error("output `{port}` names record `{record}`, which no registered port declares")]
    UnknownRecord { port: String, record: String },

    #[error("the pack refused the definition: {message}")]
    Pack { message: String },

    #[error("slot: {message}")]
    Slot { message: String },

    #[error("async system `{system}` takes a `Sample`, which only a stepped system takes")]
    SampleOnAsync { system: String },

    #[error("async system `{system}` takes a `Cap`, which only a stepped system takes")]
    IoOnAsync { system: String },

    #[error("async system `{system}` takes an `Rng`, which only a stepped system takes")]
    RngOnAsync { system: String },
}

/// Port definitions by record name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Records(HashMap<String, PortDef>);

impl Records {
    /// Collects the records that `ports` carry, keeping the first port of each
    /// record name.
    ///
    /// The system table refuses two ports that declare one record differently.
    pub fn from_port_defs<'a>(ports: impl Iterator<Item = &'a PortDef>) -> Self {
        let mut records = HashMap::new();
        for port in ports {
            records
                .entry(port.record.to_string())
                .or_insert_with(|| port.clone());
        }
        Self(records)
    }

    /// Returns a definition for a port called `port` that carries `record`.
    ///
    /// Fails if the record is unknown.
    pub fn port_def(&self, port: &str, record: &str) -> Result<PortDef, DefError> {
        let def = self.0.get(record).ok_or_else(|| DefError::UnknownRecord {
            port: port.to_string(),
            record: record.to_string(),
        })?;
        Ok(PortDef {
            name: port.to_string().into(),
            ..def.clone()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::utils::{Imu, ImuPair, Nav};
    use crate::{Output, Record};

    #[test]
    fn test_port_path_skips_missing_parts() {
        assert_eq!(DefCx::empty().port_path("imu"), "imu");
        let named = DefCx {
            id: "plant",
            ..DefCx::empty()
        };
        assert_eq!(named.port_path("imu"), "plant.imu");
        let namespaced = DefCx {
            id: "plant",
            namespace: Some("cube_sat"),
            ..DefCx::empty()
        };
        assert_eq!(namespaced.port_path("imu"), "cube_sat.plant.imu");
    }

    #[test]
    fn test_record_port_lookup() {
        let records = Records::from_port_defs([Output::<Imu>::def(&DefCx::empty(), "imu")].iter());
        let port = records
            .port_def("plant.imu", Imu::NAME)
            .expect("known record");
        assert_eq!(port.name, "plant.imu");
        assert_eq!(port.id, Output::<Imu>::def(&DefCx::empty(), "imu").id);
    }

    #[test]
    fn test_unknown_record_error() {
        let records = Records::from_port_defs([Output::<Imu>::def(&DefCx::empty(), "imu")].iter());
        assert_eq!(
            records.port_def("out", Nav::NAME),
            Err(DefError::UnknownRecord {
                port: "out".into(),
                record: Nav::NAME.into(),
            })
        );
    }

    /// The first port of a record name wins.
    #[test]
    fn test_first_record_definition_wins() {
        let cx = DefCx::empty();
        let records = Records::from_port_defs(
            [
                Output::<Imu>::def(&cx, "imu"),
                Output::<ImuPair>::def(&cx, "imu"),
            ]
            .iter(),
        );
        let port = records.port_def("out", Imu::NAME).expect("known record");
        assert!(port.same_record(&Output::<Imu>::def(&cx, "imu")));
    }

    #[test]
    fn test_empty_context() {
        let cx = DefCx::empty();
        assert!(cx.inputs.is_empty() && cx.outputs.is_empty());
        assert!(cx.records.port_def("out", "imu").is_err());
    }
}
