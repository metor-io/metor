//! Reads a frame back out of the port a pack declares for it.
//!
//! This is the inverse of [`schema::port_def`](crate::schema::port_def) for
//! frames of scalars and arrays, so the compiler can import a Rust pack's
//! frames and check them by layout.

use std::collections::HashMap;

use metor_fsw_3::metor_proto::types::ComponentId;
use metor_fsw_3::metor_proto::vtable::{Field, Op, OpRef, VTable};
use metor_fsw_3::{PortDef, RecordSchema};

use crate::desc::{FrameDesc, LeafDesc, Prim};
use crate::schema::frame_layout;

/// Why a port's frame cannot be imported.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ImportError {
    #[error("`{0}` is a message, not a frame")]
    NotFrame(String),
    #[error("`{0}` has a field this compiler cannot read: {1}")]
    Unreadable(String, &'static str),
    #[error("`{0}` has a layout metor-code cannot reproduce")]
    Layout(String),
}

/// Rebuilds the frame a port carries.
///
/// `root` is the path the port's schema names its fields under. The rebuilt
/// frame's layout must hash to the port's own, so a frame metor-code reads
/// differently from its producer is refused.
pub fn frame_of_port(port: &PortDef, root: &str) -> Result<FrameDesc, ImportError> {
    let name = port.record.to_string();
    let RecordSchema::Frame { vtable, metadata } = &port.schema else {
        return Err(ImportError::NotFrame(name));
    };
    let names: HashMap<ComponentId, &str> = metadata
        .iter()
        .map(|m| (m.component_id, m.name.as_str()))
        .collect();
    let prefix = format!("{root}.");
    let mut timestamp = None;
    let mut leaves = Vec::new();
    for field in &vtable.fields {
        let leaf = read_field(vtable, field).ok_or(ImportError::Unreadable(name.clone(), "op"))?;
        timestamp = timestamp.or(leaf.timestamp);
        let full = names
            .get(&leaf.component)
            .ok_or(ImportError::Unreadable(name.clone(), "name"))?;
        let path = full.strip_prefix(&prefix).unwrap_or(full);
        leaves.push(LeafDesc {
            path: path.to_string(),
            offset: leaf.offset,
            prim: leaf.prim,
        });
    }
    let frame = FrameDesc {
        name: name.clone(),
        size: port.max_len as u32,
        align: port.alignment as u32,
        timestamp: timestamp.ok_or(ImportError::Unreadable(name.clone(), "timestamp"))?,
        leaves,
    };
    match port.layout == Some(frame_layout(&frame)) {
        true => Ok(frame),
        false => Err(ImportError::Layout(name)),
    }
}

/// What one vtable field says about its leaf.
struct ReadLeaf {
    offset: u32,
    prim: Prim,
    component: ComponentId,
    timestamp: Option<u32>,
}

/// Follows a field's op chain down to its component.
fn read_field(vtable: &VTable, field: &Field) -> Option<ReadLeaf> {
    let mut op = vtable.get_op(field.arg).ok()?;
    let mut timestamp = None;
    loop {
        match op {
            Op::Frame { arg, .. } => op = vtable.get_op(*arg).ok()?,
            Op::Timestamp { source, arg } => {
                let Op::Table { offset, .. } = vtable.get_op(*source).ok()? else {
                    return None;
                };
                timestamp = Some(offset.to_index() as u32);
                op = vtable.get_op(*arg).ok()?;
            }
            Op::Schema { ty, dim, arg } => {
                if !data(vtable, *dim)?.is_empty() {
                    return None;
                }
                let prim = prim_of(u64::from_le_bytes(data(vtable, *ty)?.try_into().ok()?))?;
                let Op::Component { component_id } = vtable.get_op(*arg).ok()? else {
                    return None;
                };
                let id = u64::from_le_bytes(data(vtable, *component_id)?.try_into().ok()?);
                return Some(ReadLeaf {
                    offset: field.offset.to_index() as u32,
                    prim,
                    component: ComponentId(id),
                    timestamp,
                });
            }
            _ => return None,
        }
    }
}

/// Returns the bytes of a `Data` op.
fn data(vtable: &VTable, op: OpRef) -> Option<&[u8]> {
    let Op::Data { offset, len } = vtable.get_op(op).ok()? else {
        return None;
    };
    vtable
        .data
        .get(offset.to_index()..offset.to_index() + *len as usize)
}

/// Maps a vtable type tag to a scalar.
fn prim_of(tag: u64) -> Option<Prim> {
    const PRIMS: [Prim; 11] = [
        Prim::U8,
        Prim::U16,
        Prim::U32,
        Prim::U64,
        Prim::I8,
        Prim::I16,
        Prim::I32,
        Prim::I64,
        Prim::Bool,
        Prim::F32,
        Prim::F64,
    ];
    PRIMS.get(usize::try_from(tag).ok()?).copied()
}

#[cfg(test)]
mod tests {
    use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
    use metor_fsw_3::{DefCx, Frame, Output, Timestamp};

    use super::*;

    #[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
    #[repr(C)]
    struct Chamber {
        #[frame(timestamp)]
        timestamp: Timestamp,
        psi: f64,
        temps: [f32; 2],
        valve: u8,
        _pad: [u8; 7],
    }

    #[test]
    fn test_imports_a_derived_frame() {
        let port = Output::<Chamber>::def(&DefCx::empty(), "chamber");
        let frame = frame_of_port(&port, "chamber").expect("imports");
        let paths: Vec<_> = frame
            .leaves
            .iter()
            .map(|l| (l.path.as_str(), l.offset))
            .collect();
        assert_eq!(
            paths,
            [("psi", 8), ("temps.0", 16), ("temps.1", 20), ("valve", 24)]
        );
        assert_eq!((frame.size, frame.align, frame.timestamp), (32, 8, 0));
    }

    #[test]
    fn test_refuses_a_changed_layout() {
        let mut port = Output::<Chamber>::def(&DefCx::empty(), "chamber");
        port.layout = Some(1);
        assert_eq!(
            frame_of_port(&port, "chamber"),
            Err(ImportError::Layout("chamber".into()))
        );
    }

    #[test]
    fn test_refuses_a_message() {
        let port = Output::<metor_fsw_3::metor_proto_wkt::LogEvent>::def(&DefCx::empty(), "log");
        assert!(matches!(
            frame_of_port(&port, "log"),
            Err(ImportError::NotFrame(_))
        ));
    }
}
