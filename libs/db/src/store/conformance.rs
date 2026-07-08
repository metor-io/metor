//! Black-box contract tests for any [`NodeStore`] implementation.
//!
//! Backend authors (S3, ClickHouse, custom HTTP, …) should call
//! [`run`] from their own test suite; it panics with a description on the
//! first violated contract. `scratch` is a caller-owned empty directory
//! used for fixture nodes and staging — pass a tempdir.

use std::path::Path;

use metor_proto::types::{ComponentId, Timestamp};

use crate::{
    seal::{SealRecord, seal_node},
    store::{NodeKey, NodeStaging, NodeStore, SealedNode},
    time_series_2::TimeSeriesNode,
};

struct Fixture {
    node: TimeSeriesNode,
    seal: SealRecord,
}

fn fixture(scratch: &Path, start: i64, samples: i64) -> Fixture {
    let dir = scratch.join(format!("fixture-{start}"));
    let node = TimeSeriesNode::create(&dir, Timestamp(start), 8).expect("create fixture node");
    for i in 0..samples {
        node.data.write(&(start + i).to_le_bytes()).expect("write");
        node.index
            .write(&Timestamp(start + i).to_le_bytes())
            .expect("write");
    }
    let seal = seal_node(&node, &dir).expect("seal").expect("non-empty");
    Fixture { node, seal }
}

fn key<'a>(
    seal: &SealRecord,
    component_id: ComponentId,
    schema: &'a metor_proto::schema::Schema<Vec<u64>>,
) -> NodeKey<'a> {
    NodeKey {
        component_id,
        component_name: "conformance.test",
        schema,
        start_ts: seal.start_ts,
        checksum: seal.checksum,
    }
}

/// Run every contract check against `store`.
pub async fn run(store: &dyn NodeStore, scratch: &Path) {
    let component_id = ComponentId(7777);
    let schema = metor_proto::schema::Schema::new(metor_proto::types::PrimType::U64, [1usize])
        .expect("schema");
    let a = fixture(scratch, 1_000, 64);
    let b = fixture(scratch, 2_000, 32);

    // Empty store: nothing found, nothing listed.
    assert!(
        !store
            .contains(key(&a.seal, component_id, &schema))
            .await
            .expect("contains on empty store"),
        "empty store claims to contain a node"
    );
    assert!(
        store
            .list(component_id, "conformance.test")
            .await
            .expect("list on empty store")
            .is_empty(),
        "empty store lists nodes"
    );
    let staging_dir = scratch.join("staging-miss");
    std::fs::create_dir_all(&staging_dir).expect("staging dir");
    let staging = NodeStaging::create(&staging_dir, &a.seal).expect("staging");
    assert!(
        store.get(key(&a.seal, component_id, &schema), &staging).await.is_err(),
        "get of a missing node must fail"
    );
    drop(staging);

    // Put, then read everything back.
    let sealed_a = SealedNode::from_node(&a.node, a.seal).expect("sealed view");
    store
        .put(key(&a.seal, component_id, &schema), sealed_a)
        .await
        .expect("put");
    assert!(
        store
            .contains(key(&a.seal, component_id, &schema))
            .await
            .expect("contains after put"),
        "store does not contain a node it acked"
    );

    let staging_dir = scratch.join("staging-get");
    std::fs::create_dir_all(&staging_dir).expect("staging dir");
    let staging = NodeStaging::create(&staging_dir, &a.seal).expect("staging");
    let fetched_seal = store
        .get(key(&a.seal, component_id, &schema), &staging)
        .await
        .expect("get after put");
    assert_eq!(
        fetched_seal, a.seal,
        "store returned a different seal than it accepted"
    );
    // Byte-identical round trip: commit verifies the checksum.
    staging
        .commit(&a.seal)
        .expect("fetched bytes failed checksum verification");

    // Idempotent re-put.
    store
        .put(key(&a.seal, component_id, &schema), sealed_a)
        .await
        .expect("re-put of an existing node must succeed");

    // A second node; list returns both sorted by start timestamp.
    let sealed_b = SealedNode::from_node(&b.node, b.seal).expect("sealed view");
    store
        .put(key(&b.seal, component_id, &schema), sealed_b)
        .await
        .expect("put second node");
    let listed = store
        .list(component_id, "conformance.test")
        .await
        .expect("list");
    assert_eq!(
        listed,
        vec![a.seal, b.seal],
        "list must return every stored seal sorted by start_ts"
    );

    // Unknown component lists empty rather than erroring.
    assert!(
        store
            .list(ComponentId(8888), "conformance.other")
            .await
            .expect("list unknown component")
            .is_empty()
    );
}

#[cfg(test)]
mod tests {
    use crate::store::LocalDirStore;

    #[stellarator::test]
    async fn local_dir_store_conforms() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalDirStore::new(dir.path().join("store"));
        super::run(&store, dir.path()).await;
    }
}
