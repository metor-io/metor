//! The descriptor a pack exports: a projection of its [`SystemTable`].

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::coordinator::SystemTable;
use crate::system::SystemDef;

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
    pub def: SystemDef,
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
            .map(|(ty, entry)| PackSystemDef {
                ty: ty.to_string(),
                def: entry.def.clone(),
                doc: entry.doc.clone(),
                params: entry.schema.clone(),
            })
            .collect();
        Self { systems }
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
                "log_sink"
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
