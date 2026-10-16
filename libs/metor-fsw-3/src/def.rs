//! The config context a system uses to compute its [`SystemDef`](crate::SystemDef).
//!
//! Most systems declare fixed ports and ignore it. Systems whose ports come
//! from config, like links, read their ports from it.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::coordinator::OutputConfig;
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
        [self.namespace.unwrap_or(""), self.id, port]
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

    #[error("record `{record}` is declared differently by two registered ports")]
    RecordConflict { record: String },

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

    #[error("ports `{port}` and `{other}` carry tables with the same id")]
    TableIdCollision { port: String, other: String },

    #[error("`{system}` takes a `Cap`; register it with `register` or `register_sequence`")]
    IoOnTraitPath { system: String },
}

/// Port definitions by record name.
///
/// A record that two ports declare differently maps to `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Records(HashMap<String, Option<PortDef>>);

impl Records {
    /// Collects the records that `ports` carry.
    ///
    /// If two ports carry different records under the same name, that name is
    /// marked as a conflict.
    pub fn from_port_defs<'a>(ports: impl Iterator<Item = &'a PortDef>) -> Self {
        let mut records = HashMap::new();
        for port in ports {
            match records.get(port.record.as_ref()) {
                Some(Some(known)) if !port.same_record(known) => {
                    records.insert(port.record.to_string(), None);
                }
                Some(_) => {}
                None => {
                    records.insert(port.record.to_string(), Some(port.clone()));
                }
            }
        }
        Self(records)
    }

    /// Returns a definition for a port called `port` that carries `record`.
    ///
    /// Fails if the record is unknown, or if two ports declared it differently.
    pub fn port_def(&self, port: &str, record: &str) -> Result<PortDef, DefError> {
        match self.0.get(record) {
            Some(Some(def)) => Ok(PortDef {
                name: port.to_string().into(),
                ..def.clone()
            }),
            Some(None) => Err(DefError::RecordConflict {
                record: record.to_string(),
            }),
            None => Err(DefError::UnknownRecord {
                port: port.to_string(),
                record: record.to_string(),
            }),
        }
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

    #[test]
    fn test_conflicting_record_definitions() {
        let mut wide = Output::<Imu>::def(&DefCx::empty(), "imu");
        wide.max_len += 8;
        let records =
            Records::from_port_defs([Output::<Imu>::def(&DefCx::empty(), "imu"), wide].iter());
        assert_eq!(
            records.port_def("out", Imu::NAME),
            Err(DefError::RecordConflict {
                record: Imu::NAME.into(),
            })
        );
    }

    /// Two frames of one name whose fields differ dispute the record.
    #[test]
    fn test_conflicting_frame_layouts() {
        let cx = DefCx::empty();
        let records = Records::from_port_defs(
            [
                Output::<Imu>::def(&cx, "imu"),
                Output::<ImuPair>::def(&cx, "imu"),
            ]
            .iter(),
        );
        assert_eq!(
            records.port_def("out", Imu::NAME),
            Err(DefError::RecordConflict {
                record: Imu::NAME.into(),
            })
        );
    }

    #[test]
    fn test_empty_context() {
        let cx = DefCx::empty();
        assert!(cx.inputs.is_empty() && cx.outputs.is_empty());
        assert!(cx.records.port_def("out", "imu").is_err());
    }
}
