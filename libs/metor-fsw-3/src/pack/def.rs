//! The descriptor a pack exports: a projection of its [`SystemTable`].

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::coordinator::SystemTable;
use crate::system::SystemDef;

/// A `PackDef` is every system a pack exports, in registration order.
///
/// `SystemDef`'s names are `&'static str`, so a descriptor decodes only from
/// bytes the host has leaked.
#[derive(Serialize, Deserialize, Debug)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct PackDef<'a> {
    #[serde(borrow)]
    pub systems: Vec<PackSystemDef<'a>>,
}

/// One exported system: its table key, its ports, its doc, and its params schema.
#[derive(Serialize, Deserialize, Debug)]
#[serde(bound(deserialize = "'de: 'static"))]
pub struct PackSystemDef<'a> {
    /// The table key: `register("nav", ..)`.
    pub ty: &'a str,
    pub def: SystemDef,
    /// The doc comment on `execute`, or empty.
    #[serde(borrow)]
    pub doc: Cow<'a, str>,
    /// JSON Schema of the params struct; `None` for a `Fn() -> S` ctor.
    #[serde(borrow)]
    pub params: Option<Cow<'a, RawValue>>,
}

impl PackDef<'_> {
    /// Serializes `table`'s entries as the descriptor `metor_fsw_pack_def` returns.
    pub fn from_table(table: &SystemTable) -> Vec<u8> {
        let systems = table
            .entries()
            .map(|(ty, entry)| PackSystemDef {
                ty,
                def: entry.def.clone(),
                doc: Cow::Borrowed(entry.doc),
                params: entry.schema.as_deref().map(Cow::Borrowed),
            })
            .collect();
        // Every field is already valid JSON, so the writer cannot fail; empty
        // bytes leave the host with a decode error either way.
        serde_json::to_vec(&PackDef { systems }).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::utils::{Recorder, table};

    /// Leaks the descriptor so it decodes with the lifetime a host gives it.
    fn leaked(table: &SystemTable) -> &'static [u8] {
        Box::leak(PackDef::from_table(table).into_boxed_slice())
    }

    fn within(bytes: &[u8], text: &str) -> bool {
        let (start, end) = (
            bytes.as_ptr() as usize,
            bytes.as_ptr() as usize + bytes.len(),
        );
        let at = text.as_ptr() as usize;
        at >= start && at + text.len() <= end
    }

    #[test]
    fn from_table_lists_systems_in_registration_order() {
        let table = table(&Recorder::default());
        let bytes = leaked(&table);
        let def: PackDef<'static> = serde_json::from_slice(bytes).expect("decodes");
        let types: Vec<_> = def.systems.iter().map(|s| s.ty).collect();
        assert_eq!(
            types,
            vec![
                "imu",
                "imu_offset",
                "nav",
                "control",
                "status_watch",
                "reserved"
            ]
        );
        assert_eq!(def.systems[0].def.outputs[0].name, "imu");
    }

    #[test]
    fn names_borrow_from_the_decoded_bytes() {
        let table = table(&Recorder::default());
        let bytes = leaked(&table);
        let def: PackDef<'static> = serde_json::from_slice(bytes).expect("decodes");
        let system = &def.systems[0];
        assert!(within(bytes, system.ty));
        assert!(within(bytes, system.def.name));
        assert!(within(bytes, system.def.outputs[0].name));
        assert!(within(bytes, system.def.outputs[0].record));
        assert!(matches!(system.doc, Cow::Borrowed(_)));
        assert!(system.params.is_none());
    }

    #[test]
    fn a_doc_with_an_escape_decodes_as_owned() {
        let bytes: &'static [u8] = br#"{"systems":[{"ty":"nav",
            "def":{"name":"nav","inputs":[],"outputs":[]},
            "doc":"one\ntwo","params":{"type":"object"}}]}"#;
        let def: PackDef<'static> = serde_json::from_slice(bytes).expect("decodes");
        assert!(matches!(def.systems[0].doc, Cow::Owned(_)));
        assert_eq!(def.systems[0].doc, "one\ntwo");
        assert_eq!(
            def.systems[0].params.as_deref().map(RawValue::get),
            Some(r#"{"type":"object"}"#)
        );
    }

    #[test]
    fn an_empty_table_has_no_systems() {
        let def = PackDef::from_table(&SystemTable::new());
        assert_eq!(def, br#"{"systems":[]}"#);
    }

    #[test]
    fn truncated_bytes_do_not_decode() {
        assert!(serde_json::from_slice::<PackDef<'static>>(b"{\"systems\":").is_err());
    }
}
