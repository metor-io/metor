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
use metor_db::disruptor::Disruptor;
use metor_proto::types::ComponentId;
use stellarator::JoinHandleDropGuard;

use crate::dynamic::node::{
    BuildError, DynamicNode, DynamicNodeExt, NodeId, ValueType, hash_id, op_tag,
};

struct PersistNode {
    id: NodeId,
    value_type: ValueType,
    parent_clock_id: Option<NodeId>,
    component_wal: Disruptor,
    _task: JoinHandleDropGuard<()>,
}

impl DynamicNode for PersistNode {
    fn id(&self) -> NodeId {
        self.id
    }
    fn value_type(&self) -> &ValueType {
        &self.value_type
    }
    fn parent_clock_id(&self) -> Option<NodeId> {
        self.parent_clock_id
    }
    fn output(&self) -> &Disruptor {
        &self.component_wal
    }
}

/// Promote `input` to a real db Component named `name`. Returns a
/// passthrough node whose output mirrors the Component's WAL.
///
/// The Component's `ComponentId` is derived from this node's id. If a
/// component with that id already exists (e.g. from a prior session), it is
/// reused — historical samples on disk are preserved. If the existing
/// schema doesn't match, returns `BuildError::SchemaMismatch`.
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
    let component_id = ComponentId(id.0);

    db.with_state_mut(|s| s.insert_component(component_id, schema.clone(), &db.path))
        .map_err(|err| match err {
            metor_db::Error::SchemaMismatch => BuildError::SchemaMismatch {
                a: schema.clone(),
                b: schema.clone(),
            },
            other => {
                tracing::error!(?other, "persist: insert_component failed");
                BuildError::SchemaMismatch {
                    a: schema.clone(),
                    b: schema.clone(),
                }
            }
        })?;

    // Set human-readable metadata so the component browser shows `name`.
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
    let component_wal = component.wal.clone();
    let parent_clock_id = input.parent_clock_id();

    let mut reader = input.subscribe();
    let task = stellarator::spawn(async move {
        let _input = input;
        loop {
            let grant = reader.next().await;
            for (ts, value) in grant.samples() {
                if let Err(err) = component.push_buf(ts, value) {
                    tracing::warn!(?err, ?ts, "persist: push_buf failed");
                }
            }
        }
    });

    Ok(Arc::new(PersistNode {
        id,
        value_type: ValueType::Value(schema),
        parent_clock_id,
        component_wal,
        _task: task.drop_guard(),
    }))
}
