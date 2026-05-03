//! `from_db`: bridge a real db Component into the dynamic graph.
//!
//! Republishes every sample from the component's WAL onto a node-owned
//! [`Disruptor`]. Republishing (vs. handing out a reader on the existing
//! WAL) decouples the node's lifetime from the underlying Component and
//! lets us treat it like any other node (own clock id, own subscribers).

use std::hash::Hash;
use std::mem::size_of;
use std::sync::Arc;

use metor_db::DB;
use metor_proto::types::{ComponentId, Timestamp};

use crate::dynamic::node::{
    BuildError, DynamicNode, NodeImpl, ValueType, default_ring_bytes, hash_id, op_tag, write_sample,
};

/// Mirror `component_id` from the DB into a node. The component must be
/// registered before this is called — its schema is read upfront so the
/// node has a stable [`ValueType`]. To wait for a component that may not
/// be registered yet, await `db.vtable_gen.wait()` first.
pub fn from_db(db: &DB, component_id: ComponentId) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let component = db
        .with_state(|s| s.get_component(component_id).cloned())
        .ok_or(BuildError::SchemaMismatch {
            a: metor_db::ComponentSchema::new(metor_proto::types::PrimType::F64, &[]),
            b: metor_db::ComponentSchema::new(metor_proto::types::PrimType::F64, &[]),
        })?;
    let schema = component.schema.clone();
    let value_bytes = schema.size();

    let id = hash_id(op_tag::FROM_DB, &[], |h| {
        component_id.0.hash(h);
    });
    let mut reader = component.wal.reader();
    let msg_size = size_of::<Timestamp>() + value_bytes;
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema),
        Some(id),
        default_ring_bytes(value_bytes),
        move |output| async move {
            let _component = component;
            loop {
                let grant = reader.next().await;
                for chunk in grant.chunks_exact(msg_size) {
                    let ts_bytes: [u8; 8] = chunk[..size_of::<Timestamp>()]
                        .try_into()
                        .expect("ts slice");
                    let ts = Timestamp::from_le_bytes(ts_bytes);
                    let value = &chunk[size_of::<Timestamp>()..];
                    write_sample(&output, ts, value);
                }
            }
        },
    ))
}
