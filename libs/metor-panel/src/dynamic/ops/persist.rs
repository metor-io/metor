//! `persist`: register the input stream as a real db Component, granting it
//! the on-disk TimeSeries plus all existing view integration (browser,
//! plot, monitor, etc.) for free.
//!
//! The persist node is a transparent passthrough — its output is the
//! Component's own WAL, so subscribing to the persist node and subscribing
//! to the underlying component yield the same stream. The Component's
//! built-in persist task (spawned by `Component::create`) handles disk.

use std::hash::Hash;
use std::sync::Arc;

use metor_db::DB;
use metor_proto::types::ComponentId;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeImpl, ValueType, hash_id, op_tag,
};

/// Promote `input` to a real db Component named `name`. Returns a
/// passthrough node whose output mirrors the Component's WAL.
///
/// The Component's `ComponentId` is derived from `name` alone, so the
/// component identity is stable across graph edits and sessions: editing
/// upstream args (or restarting the app) re-resolves to the same on-disk
/// Component, preserving its history. If a component with that id already
/// exists with a different schema, returns `BuildError::SchemaMismatch`.
///
/// The persist *node*'s `NodeId` still hashes the input chain, so the graph
/// rebuilder can dedup persist nodes correctly.
pub fn persist(
    db: &DB,
    name: String,
    input: Arc<dyn DynamicNode>,
) -> Result<Arc<dyn DynamicNode>, BuildError> {
    let schema = input
        .value_type()
        .schema()
        .cloned()
        .ok_or(BuildError::ExpectedValue)?;
    let id = hash_id(op_tag::PERSIST, &[input.id()], |h| {
        name.hash(h);
    });
    // ComponentId is name-derived (not node-id-derived) so the on-disk
    // Component survives upstream edits.
    let component_id = ComponentId(component_id_for_name(&name));

    db.with_state_mut(|s| s.insert_component(component_id, schema.clone(), &db.path))
        .map_err(|err| match err {
            metor_db::Error::SchemaMismatch => {
                // The existing on-disk component has a different schema
                // than what we're trying to register. We don't have its
                // schema readily available here, so we report both sides
                // as the *new* schema; the inspector still surfaces a
                // clear "schema mismatch" label.
                BuildError::SchemaMismatch {
                    a: schema.clone(),
                    b: schema.clone(),
                }
            }
            other => {
                tracing::error!(?other, "persist: insert_component failed");
                BuildError::DbError(other.to_string())
            }
        })?;

    let mut metadata = metor_proto_wkt::ComponentMetadata {
        component_id,
        name,
        metadata: Default::default(),
    };
    use metor_proto_wkt::MetadataExt;
    metadata.set("source", "dynamic");
    if let Err(err) = db.with_state_mut(|s| s.set_component_metadata(metadata, &db.path)) {
        tracing::warn!(?err, "persist: failed to set metadata");
    }

    let component = db
        .with_state(|s| s.get_component(component_id).cloned())
        .expect("just inserted");
    let parent_clock_id = input.parent_clock_id();
    let mut reader = input.subscribe();
    let component_for_task = component.clone();
    Ok(NodeImpl::spawn_with_output(
        id,
        ValueType::Value(schema),
        parent_clock_id,
        component.wal.clone(),
        move |_output| async move {
            let _input = input;
            let component = component_for_task;
            loop {
                let grant = reader.next().await;
                for (ts, value) in grant.samples() {
                    if let Err(err) = component.push_buf(ts, value) {
                        tracing::warn!(?err, ?ts, "persist: push_buf failed");
                    }
                }
            }
        },
    ))
}

/// Hash a component name into a stable `ComponentId` raw value. Distinct
/// from any node id hash so persist names don't collide with NodeId-derived
/// stream component ids.
pub fn component_id_for_name(name: &str) -> u64 {
    let id = hash_id(b"persist.component_name", &[], |h| name.hash(h));
    id.0
}
