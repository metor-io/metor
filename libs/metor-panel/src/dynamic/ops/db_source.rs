//! `from_db`: bridge a real db Component into the dynamic graph.
//!
//! Republishes every sample from the component's WAL onto a node-owned
//! [`Disruptor`]. Republishing (vs. handing out a reader on the existing
//! WAL) decouples the node's lifetime from the underlying Component and
//! lets us treat it like any other node (own clock id, own subscribers).

use std::hash::Hash;
use std::sync::Arc;

use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::dynamic::node::{
    BuildError, DynamicNode, NodeImpl, NodeReader, ValueType, default_ring_bytes, hash_id, op_tag,
    write_sample,
};

/// Mirror `component_id` from the DB into a node. The component must be
/// registered before this is called — its schema is read upfront so the
/// node has a stable [`ValueType`]. To wait for a component that may not
/// be registered yet, await `db.vtable_gen.wait()` first.
pub fn from_db(db: &DB, component_id: ComponentId) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let component = db
        .with_state(|s| s.get_component(component_id).cloned())
        .ok_or(BuildError::ComponentNotFound(component_id))?;
    let schema = component.schema.clone();
    let value_bytes = schema.size();

    let id = hash_id(op_tag::FROM_DB, &[], |h| {
        component_id.0.hash(h);
    });
    let mut reader = NodeReader::from_disruptor(&component.wal, value_bytes);
    Ok(NodeImpl::spawn(
        id,
        ValueType::Value(schema),
        Some(id),
        default_ring_bytes(value_bytes),
        move |output| async move {
            let _component = component;
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    write_sample(&output, ts, value);
                }
            }
        },
    ))
}
