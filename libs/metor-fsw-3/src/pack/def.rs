//! The descriptor a pack exports: a projection of its [`SystemTable`].

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::coordinator::{OutputConfig, SystemTable, TableEntry};
use crate::def::{DefCx, Records};
use crate::system::{PortDef, SystemDef};

/// An `Instance` is what the host hands `create`: the definition build settled
/// on, including the ports a config added, and where the system is placed.
#[derive(Serialize, Deserialize, Debug)]
pub struct Instance {
    pub id: String,
    pub def: SystemDef,
    pub thread: String,
}

/// A `PackDef` is every system a pack exports, in registration order.
#[derive(Serialize, Deserialize, Debug)]
pub struct PackDef {
    pub systems: Vec<PackSystemDef>,
}

/// One exported system: its table key, its ports, its doc, and its params schema.
#[derive(Serialize, Deserialize, Debug)]
pub struct PackSystemDef {
    /// The table key: `register("nav", ..)`.
    pub ty: String,
    /// The ports the type declares whatever a config says.
    pub def: SystemDef,
    /// Whether the type takes the input ports a config lists.
    #[serde(default)]
    pub takes_inputs: bool,
    /// Whether the type takes the output ports a config lists.
    #[serde(default)]
    pub takes_outputs: bool,
    /// The doc comment on `execute`, or empty.
    pub doc: Cow<'static, str>,
    /// JSON Schema of the params struct; `None` for a `Fn() -> S` ctor.
    pub params: Option<Box<RawValue>>,
}

impl PackDef {
    /// Projects the registered entries into an owned descriptor.
    pub fn from_table(table: &SystemTable) -> Self {
        let systems = table
            .entries()
            .map(|(ty, entry)| {
                let (takes_inputs, takes_outputs) = probe(entry);
                PackSystemDef {
                    ty: ty.to_string(),
                    def: entry.def.clone(),
                    takes_inputs,
                    takes_outputs,
                    doc: entry.doc.clone(),
                    params: entry.schema.clone(),
                }
            })
            .collect();
        Self { systems }
    }
}

/// The port name a probe offers a type, which no config would spell.
const PROBE: &str = "__probe";

/// Whether a type takes the ports a config lists, by offering it one of each.
fn probe(entry: &TableEntry) -> (bool, bool) {
    let record = crate::Output::<crate::SystemStatus>::def(PROBE);
    let records = Records::of(core::iter::once(&record));
    let inputs = [(PROBE, &record)];
    let outputs = [OutputConfig {
        port: PROBE.to_string(),
        record: record.record.to_string(),
    }];
    let cx = DefCx {
        inputs: &inputs,
        outputs: &outputs,
        records: &records,
    };
    match entry.def_for(&cx) {
        Ok(def) => (took(&def.inputs), took(&def.outputs)),
        Err(_) => (false, false),
    }
}

fn took(ports: &[PortDef]) -> bool {
    ports.iter().any(|port| port.name == PROBE)
}

/// A [`DefCx`] as the ABI carries it: the borrowed context, owned.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct DefCxOwned {
    pub inputs: Vec<(String, PortDef)>,
    pub outputs: Vec<OutputConfig>,
    pub records: Records,
}

impl DefCxOwned {
    /// Copies a context for the crossing.
    pub fn of(cx: &DefCx<'_>) -> Self {
        Self {
            inputs: cx
                .inputs
                .iter()
                .map(|(port, def)| (port.to_string(), (*def).clone()))
                .collect(),
            outputs: cx.outputs.to_vec(),
            records: cx.records.clone(),
        }
    }

    /// The edges this context carries, in the shape [`DefCx`] borrows.
    pub fn edges(&self) -> Vec<(&str, &PortDef)> {
        self.inputs
            .iter()
            .map(|(port, def)| (port.as_str(), def))
            .collect()
    }

    /// Borrows this context back, over `edges` from [`DefCxOwned::edges`].
    pub fn borrow<'a>(&'a self, edges: &'a [(&'a str, &'a PortDef)]) -> DefCx<'a> {
        DefCx {
            inputs: edges,
            outputs: &self.outputs,
            records: &self.records,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::utils::{Recorder, table};

    fn decoded(table: &SystemTable) -> PackDef {
        let bytes = serde_json::to_vec(&PackDef::from_table(table)).expect("encodes");
        serde_json::from_slice(&bytes).expect("decodes")
    }

    #[test]
    fn from_table_lists_systems_in_registration_order() {
        let table = table(&Recorder::default());
        let def = decoded(&table);
        let types: Vec<_> = def.systems.iter().map(|s| s.ty.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "imu",
                "imu_offset",
                "nav",
                "control",
                "status_watch",
                "reserved",
                "boom",
                "trap",
                "log_sink",
                "tap",
                "emit",
                "relay",
                "sleeper",
                "async_boom",
                "ctor_boom",
                "who_am_i"
            ]
        );
        assert_eq!(def.systems[0].def.outputs[0].name, "imu");
    }

    #[test]
    fn decoded_names_outlive_the_source_buffer() {
        let def = decoded(&table(&Recorder::default()));
        let system = &def.systems[0];
        assert_eq!(system.ty, "imu");
        assert_eq!(system.def.outputs[0].name, "imu");
        assert!(matches!(system.def.name, Cow::Owned(_)));
        assert!(matches!(system.def.outputs[0].record, Cow::Owned(_)));
        assert!(system.params.is_none());
    }

    #[test]
    fn a_doc_with_an_escape_decodes_as_owned() {
        let bytes: &'static [u8] = br#"{"systems":[{"ty":"nav",
            "def":{"name":"nav","inputs":[],"outputs":[]},
            "doc":"one\ntwo","params":{"type":"object"}}]}"#;
        let def: PackDef = serde_json::from_slice(bytes).expect("decodes");
        assert!(matches!(def.systems[0].doc, Cow::Owned(_)));
        assert_eq!(def.systems[0].doc, "one\ntwo");
        assert_eq!(
            def.systems[0].params.as_deref().map(RawValue::get),
            Some(r#"{"type":"object"}"#)
        );
    }

    /// Every port announces, so the ground needs nothing but the descriptor.
    #[test]
    fn every_port_carries_its_schema_across_the_descriptor() {
        use crate::Record;
        use crate::record::{MsgCodec, RecordSchema};
        use crate::tests::utils::Imu;

        let def = decoded(&table(&Recorder::default()));
        assert_eq!(def.systems[0].def.outputs[0].schema, Imu::schema());
        let ports = def
            .systems
            .iter()
            .flat_map(|s| s.def.inputs.iter().chain(&s.def.outputs));
        for port in ports {
            match &port.schema {
                RecordSchema::Frame { metadata, .. } => assert!(!metadata.is_empty()),
                RecordSchema::Msg { codec, .. } => {
                    assert!(matches!(codec, MsgCodec::Postcard(_)))
                }
            }
        }
    }

    /// The descriptor says which types take the ports a config lists.
    #[test]
    fn the_probe_finds_the_types_that_take_config_ports() {
        let def = decoded(&table(&Recorder::default()));
        let takes = |ty: &str| {
            let system = def.systems.iter().find(|s| s.ty == ty).expect("registered");
            (system.takes_inputs, system.takes_outputs)
        };
        assert_eq!(takes("tap"), (true, false));
        assert_eq!(takes("emit"), (false, true));
        assert_eq!(takes("nav"), (false, false));
    }

    #[test]
    fn an_empty_table_has_no_systems() {
        let def = PackDef::from_table(&SystemTable::new());
        assert_eq!(
            serde_json::to_vec(&def).expect("encodes"),
            br#"{"systems":[]}"#
        );
    }

    #[test]
    fn truncated_bytes_do_not_decode() {
        assert!(serde_json::from_slice::<PackDef>(b"{\"systems\":").is_err());
    }
}
