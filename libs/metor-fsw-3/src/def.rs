//! The instance config a type computes its [`SystemDef`](crate::SystemDef) from.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

use crate::channel::{ABORT_PORT, COMMANDS_PORT};
use crate::coordinator::{Adapter, BuildError, OutputConfig, PortRef, SystemConfig, TableEntry};
use crate::system::{PortDef, SystemDef};

/// What a type sees of one instance's config when it computes its def.
pub struct DefCx<'a> {
    /// The instance this def is for.
    pub id: &'a str,
    /// The target's namespace, which announced names hang under.
    pub namespace: Option<&'a str>,
    /// One entry per config edge: the input port it names and the def of the
    /// output feeding it.
    pub inputs: &'a [(&'a str, &'a PortDef)],
    /// Each config output port and the record it names.
    pub outputs: &'a [OutputConfig],
    /// Every record the table knows.
    pub records: &'a Records,
    /// The instance's params.
    pub params: &'a serde_json::Value,
}

static NO_RECORDS: LazyLock<Records> = LazyLock::new(Records::default);
static NO_PARAMS: serde_json::Value = serde_json::Value::Null;

impl DefCx<'static> {
    /// The context with nothing in it: the def the descriptor carries.
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

impl<'a> DefCx<'a> {
    /// The context of one instance's input edges.
    pub fn of_inputs(inputs: &'a [(&'a str, &'a PortDef)]) -> Self {
        Self {
            id: "",
            namespace: None,
            inputs,
            outputs: &[],
            records: &NO_RECORDS,
            params: &NO_PARAMS,
        }
    }

    /// The context of one instance's output ports.
    pub fn of_outputs(outputs: &'a [OutputConfig], records: &'a Records) -> Self {
        Self {
            id: "",
            namespace: None,
            inputs: &[],
            outputs,
            records,
            params: &NO_PARAMS,
        }
    }

    /// Where port `port`'s components hang: `{namespace}.{id}.{port}`,
    /// skipping the parts this context lacks.
    pub fn root(&self, port: &str) -> String {
        [self.namespace.unwrap_or(""), self.id, port]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// A `DefError` is why a type refused its instance config.
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

    #[error("output `{port}` names a peer's port that is not resolved; run `metor build`")]
    Unresolved { port: String },

    #[error("slot: {message}")]
    Slot { message: String },
}

/// The definition `entry`'s type computes for `system`, over the producers
/// `producer` finds. A producer it returns `None` for is not resolved yet,
/// which only a port the type declares may read.
pub(crate) fn instance_def(
    system: &SystemConfig,
    entry: &TableEntry,
    namespace: Option<&str>,
    records: &Records,
    producer: impl Fn(&PortRef) -> Result<Option<PortDef>, BuildError>,
) -> Result<SystemDef, BuildError> {
    let channel = matches!(system.adapter, Some(Adapter::Channel(_)));
    let mut edges: Vec<(&str, PortDef)> = Vec::new();
    for input in &system.inputs {
        if channel && input.port == ABORT_PORT {
            return Err(reserved(system, ABORT_PORT));
        }
        // The channel reads its commands; the system never sees them.
        if channel && input.port == COMMANDS_PORT {
            continue;
        }
        for from in &input.from {
            match producer(from)? {
                Some(def) => edges.push((input.port.as_str(), def)),
                None if entry.def.inputs.iter().any(|p| p.name == input.port) => {}
                None => {
                    return Err(BuildError::DynamicFromLater {
                        system: system.id.clone(),
                        port: input.port.clone(),
                    });
                }
            }
        }
    }
    let inputs: Vec<(&str, &PortDef)> = edges.iter().map(|(port, def)| (*port, def)).collect();
    let cx = DefCx {
        id: &system.id,
        namespace,
        inputs: &inputs,
        outputs: &system.outputs,
        records,
        params: &system.params,
    };
    let def = entry.def_for(&cx).map_err(|source| BuildError::Def {
        id: system.id.clone(),
        source,
    })?;
    if !channel {
        return Ok(def);
    }
    crate::channel::def(def, &cx).map_err(|port| reserved(system, &port))
}

fn reserved(system: &SystemConfig, port: &str) -> BuildError {
    BuildError::ReservedChannelPort {
        id: system.id.clone(),
        port: port.to_string(),
    }
}

/// Every record a table knows, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Records(HashMap<String, Option<PortDef>>);

impl Records {
    /// Collects ports by record name, keeping `None` where two disagree.
    pub fn of<'a>(ports: impl Iterator<Item = &'a PortDef>) -> Self {
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

    /// The port declaring `record`, absent when it is unknown or disputed.
    pub fn get(&self, record: &str) -> Option<&PortDef> {
        self.0.get(record)?.as_ref()
    }

    /// The record `record` names, as a port called `port`.
    pub fn port(&self, port: &str, record: &str) -> Result<PortDef, DefError> {
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
    fn test_root_skips_missing_parts() {
        assert_eq!(DefCx::empty().root("imu"), "imu");
        let named = DefCx {
            id: "plant",
            ..DefCx::empty()
        };
        assert_eq!(named.root("imu"), "plant.imu");
        let namespaced = DefCx {
            id: "plant",
            namespace: Some("cube_sat"),
            ..DefCx::empty()
        };
        assert_eq!(namespaced.root("imu"), "cube_sat.plant.imu");
    }

    #[test]
    fn test_record_port_lookup() {
        let records = Records::of([Output::<Imu>::def(&DefCx::empty(), "imu")].iter());
        let port = records.port("plant.imu", Imu::NAME).expect("known record");
        assert_eq!(port.name, "plant.imu");
        assert_eq!(port.id, Output::<Imu>::def(&DefCx::empty(), "imu").id);
    }

    #[test]
    fn test_unknown_record_error() {
        let records = Records::of([Output::<Imu>::def(&DefCx::empty(), "imu")].iter());
        assert_eq!(
            records.port("out", Nav::NAME),
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
        let records = Records::of([Output::<Imu>::def(&DefCx::empty(), "imu"), wide].iter());
        assert_eq!(
            records.port("out", Imu::NAME),
            Err(DefError::RecordConflict {
                record: Imu::NAME.into(),
            })
        );
    }

    /// Two frames of one name whose fields differ dispute the record.
    #[test]
    fn test_conflicting_frame_layouts() {
        let cx = DefCx::empty();
        let records = Records::of(
            [
                Output::<Imu>::def(&cx, "imu"),
                Output::<ImuPair>::def(&cx, "imu"),
            ]
            .iter(),
        );
        assert_eq!(
            records.port("out", Imu::NAME),
            Err(DefError::RecordConflict {
                record: Imu::NAME.into(),
            })
        );
    }

    #[test]
    fn test_empty_context() {
        let cx = DefCx::empty();
        assert!(cx.inputs.is_empty() && cx.outputs.is_empty());
        assert!(cx.records.port("out", "imu").is_err());
    }
}
