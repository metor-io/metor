//! The descriptor a pack exports so the host knows what systems it has.
//!
//! The descriptor is built from the pack's [`SystemTable`] and sent to the host
//! as JSON.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::coordinator::{Adapter, OutputConfig, SystemKind, SystemTable, TableEntry};
use crate::def::{DefCx, Records};
use crate::system::{PortDef, SystemDef};

/// Describes the instance the host asks a pack's `create` to build.
///
/// `def` is the definition the host resolved, including any ports the config
/// added.
#[derive(Serialize, Deserialize, Debug)]
pub struct Instance {
    pub id: String,
    pub def: SystemDef,
    pub adapter: Option<Adapter>,
}

/// Lists the systems a pack exports, in registration order.
#[derive(Serialize, Deserialize, Debug)]
pub struct PackDef {
    pub systems: Vec<PackSystemDef>,
}

/// Describes one system type a pack exports.
#[derive(Serialize, Deserialize, Debug)]
pub struct PackSystemDef {
    /// The name the type was registered under, as in `register("nav", ..)`.
    pub ty: String,
    #[serde(default)]
    pub kind: SystemKind,
    /// The ports the type always has, whatever the config says.
    pub def: SystemDef,
    /// Whether the type accepts extra input ports listed in a config.
    #[serde(default)]
    pub takes_inputs: bool,
    /// Whether the type accepts extra output ports listed in a config.
    #[serde(default)]
    pub takes_outputs: bool,
    /// The doc comment on `execute`, or an empty string.
    pub doc: Cow<'static, str>,
    /// The JSON Schema of the params struct, or `None` for a `Fn() -> S` ctor.
    pub params: Option<Box<RawValue>>,
}

impl PackDef {
    /// Builds a descriptor from the types registered in `table`.
    pub fn from_table(table: &SystemTable) -> Self {
        let systems = table
            .entries()
            .map(|(ty, entry)| {
                let (takes_inputs, takes_outputs) = probe(entry);
                PackSystemDef {
                    ty: ty.to_string(),
                    kind: entry.kind,
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

/// The port name [`probe`] offers a type. No real config uses this name.
const PROBE: &str = "__probe";

/// Returns whether a type accepts config-listed input and output ports.
///
/// It offers the type one extra port of each kind and checks which ones the
/// type's definition keeps.
fn probe(entry: &TableEntry) -> (bool, bool) {
    let record = crate::Output::<crate::SystemStatus>::def(&DefCx::empty(), PROBE);
    let records = Records::of(core::iter::once(&record));
    let inputs = [(PROBE, &record)];
    let outputs = [OutputConfig::record(PROBE, record.record.as_ref())];
    let cx = DefCx {
        inputs: &inputs,
        outputs: &outputs,
        records: &records,
        ..DefCx::empty()
    };
    match (entry.def_fn)(&cx) {
        Ok(def) => (took(&def.inputs), took(&def.outputs)),
        Err(_) => (false, false),
    }
}

fn took(ports: &[PortDef]) -> bool {
    ports.iter().any(|port| port.name == PROBE)
}

/// An owned copy of a [`DefCx`], used to send it across the ABI.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct DefCxOwned {
    pub id: String,
    pub namespace: Option<String>,
    pub inputs: Vec<(String, PortDef)>,
    pub outputs: Vec<OutputConfig>,
    pub records: Records,
    #[serde(default)]
    pub params: serde_json::Value,
}

impl DefCxOwned {
    /// Copies `cx` into an owned value.
    pub fn of(cx: &DefCx<'_>) -> Self {
        Self {
            id: cx.id.to_string(),
            namespace: cx.namespace.map(str::to_string),
            inputs: cx
                .inputs
                .iter()
                .map(|(port, def)| (port.to_string(), (*def).clone()))
                .collect(),
            outputs: cx.outputs.to_vec(),
            records: cx.records.clone(),
            params: cx.params.clone(),
        }
    }

    /// Returns this context's input edges in the form [`DefCx`] borrows.
    pub fn edges(&self) -> Vec<(&str, &PortDef)> {
        self.inputs
            .iter()
            .map(|(port, def)| (port.as_str(), def))
            .collect()
    }

    /// Returns a borrowed [`DefCx`] that uses `edges` from [`DefCxOwned::edges`].
    pub fn borrow<'a>(&'a self, edges: &'a [(&'a str, &'a PortDef)]) -> DefCx<'a> {
        DefCx {
            id: &self.id,
            namespace: self.namespace.as_deref(),
            inputs: edges,
            outputs: &self.outputs,
            records: &self.records,
            params: &self.params,
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
    fn test_system_registration_order() {
        let table = table(&Recorder::default());
        let def = decoded(&table);
        let types: Vec<_> = def.systems.iter().map(|s| s.ty.as_str()).collect();
        let registered: Vec<_> = table.entries().map(|(ty, _)| ty).collect();
        assert_eq!(types, registered);
    }

    #[test]
    fn test_decoded_names_are_owned() {
        let def = decoded(&table(&Recorder::default()));
        let system = &def.systems[0];
        assert_eq!(system.ty, "imu");
        assert_eq!(system.def.outputs[0].name, "imu");
        assert!(matches!(system.def.name, Cow::Owned(_)));
        assert!(matches!(system.def.outputs[0].record, Cow::Owned(_)));
        assert!(system.params.is_none());
    }

    #[test]
    fn test_owned_context_carries_params() {
        let params = serde_json::json!({ "allow": ["a"] });
        let cx = DefCx {
            id: "slot",
            params: &params,
            ..DefCx::empty()
        };
        let bytes = serde_json::to_vec(&DefCxOwned::of(&cx)).expect("encodes");
        let owned: DefCxOwned = serde_json::from_slice(&bytes).expect("decodes");
        let edges = owned.edges();
        assert_eq!(owned.borrow(&edges).params, &params);
        let bare: DefCxOwned = serde_json::from_str(
            r#"{"id":"","namespace":null,"inputs":[],"outputs":[],"records":{}}"#,
        )
        .expect("params default");
        assert!(bare.params.is_null());
    }

    #[test]
    fn test_escaped_doc_decoding() {
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

    /// Every port in the descriptor carries its full schema.
    ///
    /// The ground station needs nothing but the descriptor to decode them.
    #[test]
    fn test_descriptor_port_schemas() {
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

    /// The descriptor records which types accept config-listed ports.
    #[test]
    fn test_configurable_port_flags() {
        let def = decoded(&table(&Recorder::default()));
        let takes = |ty: &str| {
            let system = def.systems.iter().find(|s| s.ty == ty).expect("registered");
            (system.takes_inputs, system.takes_outputs)
        };
        assert_eq!(takes("tap"), (true, false));
        assert_eq!(takes("emit"), (false, true));
        assert_eq!(takes("nav"), (false, false));
    }
}
