//! `from_db`: bridge a real db Component into the dynamic graph.
//!
//! Adopts the component's WAL as the node output. The WAL is already a
//! fan-out stream, so a second ring and forwarding task add no isolation.

use std::hash::Hash;
use std::sync::Arc;

use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::dynamic::node::{BuildError, DynamicNode, NodeId, NodeImpl, ValueType, hash_id, op_tag};

/// The node id [`from_db`] will give this component.
///
/// Derived from the component id alone, so a caller that needs to know what a
/// graph *will* hash to — a dedup check, a rebuild asking "did this change?" —
/// can ask without building it. Building spawns a task, and tasks may only be
/// spawned on the worker thread.
pub fn from_db_id(component_id: ComponentId) -> NodeId {
    hash_id(op_tag::FROM_DB, &[], |h| {
        component_id.0.hash(h);
    })
}

/// Mirror `component_id` from the DB into a node. The component must be
/// registered before this is called — its schema is read upfront so the
/// node has a stable [`ValueType`]. To wait for a component that may not
/// be registered yet, await `db.vtable_gen.wait()` first.
pub fn from_db(db: &DB, component_id: ComponentId) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let component = db
        .with_state(|s| s.get_component(component_id).cloned())
        .ok_or(BuildError::ComponentNotFound(component_id))?;
    let schema = component.schema.clone();
    let id = from_db_id(component_id);
    Ok(NodeImpl::spawn_with_output(
        id,
        ValueType::Value(schema),
        Some(id),
        component.wal,
        |_| async {},
    ))
}
