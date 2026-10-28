//! Port definitions and record schemas for compiled frames.
//!
//! A compiled frame announces the same vtable `#[derive(Frame)]` builds for
//! the equivalent `#[repr(C)]` struct, so its layout fingerprint matches a
//! Rust frame of the same shape.

use std::sync::Arc;

use metor_component::path::ComponentPath;
use metor_fsw_3::metor_proto::types::{ComponentId, PrimType};
use metor_fsw_3::metor_proto::vtable::VTable;
use metor_fsw_3::metor_proto::vtable::builder::{
    FieldBuilder, OpBuilder, component, raw_field, raw_table, schema, vtable,
};
use metor_fsw_3::{DefCx, PortDef, RecordSchema};

use crate::desc::{FrameDesc, Prim};

/// The ring depth every compiled port asks for.
const DEPTH: usize = 1;

impl Prim {
    /// The vtable type of this scalar.
    pub const fn prim_type(self) -> PrimType {
        match self {
            Prim::Bool => PrimType::Bool,
            Prim::U8 => PrimType::U8,
            Prim::U16 => PrimType::U16,
            Prim::U32 => PrimType::U32,
            Prim::U64 => PrimType::U64,
            Prim::I8 => PrimType::I8,
            Prim::I16 => PrimType::I16,
            Prim::I32 => PrimType::I32,
            Prim::I64 => PrimType::I64,
            Prim::F32 => PrimType::F32,
            Prim::F64 => PrimType::F64,
        }
    }
}

/// Returns the schema a port rooted at `root` announces for `frame`.
pub fn frame_schema(frame: &FrameDesc, root: &str) -> RecordSchema {
    let fields = vtable_fields(frame, root);
    let metadata = frame
        .leaves
        .iter()
        .map(|leaf| leaf_path(root, &leaf.path).to_metadata())
        .collect();
    RecordSchema::Frame {
        vtable: vtable(fields),
        metadata,
    }
}

/// Returns the fingerprint of `frame`'s field layout.
///
/// It hashes the vtable rooted at the record's own name, as
/// `Record::layout` does.
pub fn frame_layout(frame: &FrameDesc) -> u64 {
    let table = vtable(vtable_fields(frame, &frame.name));
    layout_of(&table)
}

/// Hashes an encoded vtable.
pub fn layout_of(table: &VTable) -> u64 {
    let bytes = postcard::to_allocvec(table).unwrap_or_default();
    const_fnv1a_hash::fnv1a_hash_64(&bytes, None)
}

/// Returns the definition of a port called `port` that carries `frame`.
pub fn port_def(frame: &FrameDesc, cx: &DefCx<'_>, port: &str) -> PortDef {
    PortDef {
        name: port.to_string().into(),
        record: frame.name.clone().into(),
        id: ComponentId::new(&frame.name),
        max_len: frame.size as usize,
        alignment: frame.align as usize,
        depth: DEPTH,
        layout: Some(frame_layout(frame)),
        schema: frame_schema(frame, &cx.port_path(port)),
    }
}

/// The vtable fields of `frame`, with each leaf under `root`.
fn vtable_fields(frame: &FrameDesc, root: &str) -> Vec<FieldBuilder> {
    let stamp: Arc<OpBuilder> = raw_table(frame.timestamp, 8);
    let frame_id = ComponentId::new(&frame.name);
    frame
        .leaves
        .iter()
        .map(|leaf| {
            let id = leaf_path(root, &leaf.path).to_component_id();
            let ty = schema(leaf.prim.prim_type(), &[], component(id));
            raw_field(leaf.offset, leaf.prim.size(), ty)
                .with_timestamp(stamp.clone())
                .with_frame(frame_id)
        })
        .collect()
}

/// Joins a root and a leaf's path, leaving out an empty root.
fn leaf_path(root: &str, path: &str) -> String {
    match root.is_empty() {
        true => path.to_string(),
        false => format!("{root}.{path}"),
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
    use metor_fsw_3::{Frame, Output, Record, Timestamp};

    use super::*;
    use crate::desc::LeafDesc;

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    struct HeaterCmd {
        #[frame(timestamp)]
        timestamp: Timestamp,
        duty: f64,
        level: u32,
        mode: u8,
        _pad: [u8; 3],
        gains: [f32; 2],
    }

    /// `_pad` is not a leaf, since `#[derive(Frame)]` skips fields whose
    /// names start with `_`.
    fn heater_cmd() -> FrameDesc {
        let leaf = |path: &str, offset, prim| LeafDesc {
            path: path.into(),
            offset,
            prim,
        };
        FrameDesc {
            name: "heater_cmd".into(),
            size: 32,
            align: 8,
            timestamp: 0,
            leaves: vec![
                leaf("duty", 8, Prim::F64),
                leaf("level", 16, Prim::U32),
                leaf("mode", 20, Prim::U8),
                leaf("gains.0", 24, Prim::F32),
                leaf("gains.1", 28, Prim::F32),
            ],
        }
    }

    #[test]
    fn test_layout_matches_derived_frame() {
        let frame = heater_cmd();
        assert_eq!(size_of::<HeaterCmd>(), frame.size as usize);
        assert_eq!(Some(frame_layout(&frame)), HeaterCmd::layout());
    }

    #[test]
    fn test_port_def_matches_derived_output() {
        let cx = DefCx {
            id: "heater",
            namespace: Some("stand"),
            ..DefCx::empty()
        };
        let ours = port_def(&heater_cmd(), &cx, "cmd");
        let theirs = Output::<HeaterCmd>::def(&cx, "cmd");
        assert_eq!(ours, theirs);
    }

    #[test]
    fn test_changed_leaf_changes_layout() {
        let mut frame = heater_cmd();
        frame.leaves[0].prim = Prim::F32;
        assert_ne!(Some(frame_layout(&frame)), HeaterCmd::layout());
        let mut renamed = heater_cmd();
        renamed.leaves[1].path = "lvl".into();
        assert_ne!(frame_layout(&renamed), frame_layout(&heater_cmd()));
    }

    #[test]
    fn test_empty_root_paths() {
        assert_eq!(leaf_path("", "a.b"), "a.b");
        assert_eq!(leaf_path("x", "a"), "x.a");
    }
}
