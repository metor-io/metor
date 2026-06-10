use metor_proto::{
    schema::Schema,
    types::{ComponentId, Timestamp},
};

use crate::{
    Error,
    seal::SealRecordExt,
    store::{NodeKey, NodeStore, SealedNode, StoreError},
    time_series_2::TimeSeries,
};

/// Push one sealed resident span to `store` and record the ack. `Ok`
/// means the store holds a durable, checksum-verified copy and the local
/// span is marked purgable. Idempotent: an already-acked span re-puts
/// (cheap — stores short-circuit by checksum) and re-acks.
pub async fn offload_span(
    time_series: &TimeSeries,
    store: &dyn NodeStore,
    component_id: ComponentId,
    component_name: &str,
    schema: &Schema<Vec<u64>>,
    start_ts: Timestamp,
) -> Result<(), Error> {
    let manifest = time_series.manifest();
    let span = manifest
        .span(start_ts)
        .filter(|s| s.state == crate::manifest::SpanState::Resident)
        .ok_or_else(|| StoreError::Other("span is not resident".to_string()))?;
    let node = time_series
        .list
        .iter()
        .find(|n| n.timestamps().first() == Some(&start_ts))
        .ok_or_else(|| StoreError::Other("resident span has no node".to_string()))?;
    if !span.seal.verify(&node) {
        return Err(StoreError::ChecksumMismatch.into());
    }
    let sealed = SealedNode::from_node(&node, span.seal)
        .ok_or_else(|| StoreError::Other("node no longer matches its seal".to_string()))?;
    let key = NodeKey {
        component_id,
        component_name,
        schema,
        start_ts,
        checksum: span.seal.checksum,
    };
    store.put(key, sealed).await?;
    time_series.mark_acked(start_ts)
}
