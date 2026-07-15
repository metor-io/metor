//! `persist`: register the input stream as a real db Component, granting it
//! the on-disk TimeSeries plus all existing view integration (browser,
//! plot, monitor, etc.) for free.
//!
//! The persist node is a transparent passthrough — its output is the
//! Component's own WAL, so subscribing to the persist node and subscribing
//! to the underlying component yield the same stream. The Component's
//! built-in persist task (spawned by `Component::create`) handles disk.

use std::hash::Hash;
use std::sync::{Arc, atomic};

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
    if name.trim().is_empty() {
        return Err(BuildError::InvalidArg {
            op: "persist",
            reason: "name must not be empty",
        });
    }
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

    // The existing on-disk schema (if any). Doubles as the "was this a new
    // insert?" signal: we only bump `vtable_gen` (which wakes view watchers)
    // on actual additions, and we report it as the incumbent side of a
    // schema mismatch.
    let existing_schema = db.with_state(|s| s.get_component(component_id).map(|c| c.schema.clone()));
    let was_new = existing_schema.is_none();
    db.with_state_mut(|s| s.insert_component(component_id, schema.clone(), &db.path))
        .map_err(|err| match err {
            metor_db::Error::SchemaMismatch => BuildError::SchemaMismatch {
                a: existing_schema.clone().unwrap_or_else(|| schema.clone()),
                b: schema.clone(),
            },
            other => {
                tracing::error!(?other, "persist: insert_component failed");
                BuildError::DbError(other.to_string())
            }
        })?;
    if was_new {
        // Notify any view that's parked on `db.vtable_gen.wait()`
        // (component browser, monitor, table, ...). Without this, a freshly
        // persisted node only shows up after something else bumps the gen.
        // `DB::insert_vtable` does this same fetch_add on the realize_fields
        // path; we mirror it here for the direct insert.
        db.vtable_gen.fetch_add(1, atomic::Ordering::SeqCst);
    }

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

/// Hash a component name into a stable `ComponentId` raw value.
///
/// Uses FNV-1a with pinned constants rather than [`hash_id`]'s
/// `DefaultHasher` (SipHash): this id is written to disk as the durable
/// identity of a persisted component, and `DefaultHasher`'s algorithm is
/// unspecified across Rust releases, so a toolchain bump could orphan every
/// persisted component. FNV-1a is fixed forever.
pub fn component_id_for_name(name: &str) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in name.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
