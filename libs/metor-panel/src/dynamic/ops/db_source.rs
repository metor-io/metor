//! `from_db`: bridge a real db Component into the dynamic graph.
//!
//! Adopts the component's WAL as the node output. The WAL is already a
//! fan-out stream, so a second ring and forwarding task add no isolation.

use std::hash::Hash;
use std::sync::Arc;

use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::dynamic::node::{BuildError, DynamicNode, NodeImpl, ValueType, hash_id, op_tag};

/// Mirror `component_id` from the DB into a node. The component must be
/// registered before this is called — its schema is read upfront so the
/// node has a stable [`ValueType`]. To wait for a component that may not
/// be registered yet, await `db.vtable_gen.wait()` first.
pub fn from_db(db: &DB, component_id: ComponentId) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let component = db
        .with_state(|s| s.get_component(component_id).cloned())
        .ok_or(BuildError::ComponentNotFound(component_id))?;
    let schema = component.schema.clone();
    let id = hash_id(op_tag::FROM_DB, &[], |h| {
        component_id.0.hash(h);
    });
    Ok(NodeImpl::spawn_with_output(
        id,
        ValueType::Value(schema),
        Some(id),
        component.wal,
        |_| async {},
    ))
}
