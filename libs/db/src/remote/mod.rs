//! Backend-agnostic movement of sealed nodes between a local DB and a
//! [`NodeStore`](crate::store::NodeStore): on-demand hydration of
//! remote-only spans and offload of local spans for archival. Which spans
//! move, and when, is decided by callers (the panel's gap requests, the
//! tiering engine); this module owns the how.

mod db;
mod fsw;
mod hydrate;
mod offload;

pub use db::{MirrorEvent, RemoteDb};
pub use fsw::{Peer, fsw_stream, identify};
pub use hydrate::Hydrator;
pub use hydrate::hydrate_span;
pub use offload::offload_span;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        manifest::{SpanSource, SpanState},
        seal::SealRecord,
        store::{LocalDirStore, NodeKey, NodeStore},
        time_series_2::{TimeSeries, TimeSeriesNode},
    };
    use metor_proto::{
        schema::Schema,
        types::{ComponentId, PrimType, Timestamp},
    };
    use std::path::Path;

    const COMPONENT: ComponentId = ComponentId(4242);

    fn schema() -> Schema<Vec<u64>> {
        Schema::new(PrimType::U64, [1usize]).unwrap()
    }

    fn component_schema() -> crate::ComponentSchema {
        crate::ComponentSchema::new(PrimType::U64, &[1][..])
    }

    /// Three nodes at starts 10/20/30 with 4 samples each; 10 and 20 end
    /// up sealed, 30 stays the live head.
    fn series_with_history(dir: &Path) -> TimeSeries {
        for start in [10i64, 20, 30] {
            let node =
                TimeSeriesNode::create(dir.join(start.to_string()), Timestamp(start), 8).unwrap();
            for i in 0..4 {
                node.data.write(&((start + i) as u64).to_le_bytes()).unwrap();
                node.index
                    .write(&Timestamp(start + i).to_le_bytes())
                    .unwrap();
            }
        }
        let series = TimeSeries::open(dir).unwrap();
        series.seal_rolled_nodes().unwrap();
        series
    }

    fn key<'a>(schema: &'a Schema<Vec<u64>>, seal: &SealRecord) -> NodeKey<'a> {
        NodeKey {
            component_id: COMPONENT,
            component_name: "round.trip",
            schema,
            start_ts: seal.start_ts,
            checksum: seal.checksum,
        }
    }

    #[stellarator::test]
    async fn offload_purge_rehydrate_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let series_dir = dir.path().join("component");
        std::fs::create_dir_all(&series_dir).unwrap();
        let series = series_with_history(&series_dir);
        let store = LocalDirStore::new(dir.path().join("store"));
        let schema = schema();

        // Offload both sealed spans.
        for start in [10i64, 20] {
            offload_span(
                &series,
                &store,
                COMPONENT,
                "round.trip",
                &schema,
                Timestamp(start),
            )
            .await
            .unwrap();
        }
        let manifest = series.manifest();
        assert!(manifest.spans.iter().all(|s| s.acked));
        for span in manifest.spans.iter() {
            assert!(store.contains(key(&schema, &span.seal)).await.unwrap());
        }

        // Purge one; its bytes leave disk but the manifest remembers it.
        assert!(series.purge_span(Timestamp(10)).unwrap());
        assert!(!series_dir.join("10").exists());
        assert!(series.get(Timestamp(10)).is_none());
        assert_eq!(
            series.manifest().span(Timestamp(10)).unwrap().state,
            SpanState::RemoteOnly
        );

        // Purging an unacked or live span is refused.
        assert!(!series.purge_span(Timestamp(30)).unwrap());

        // Hydrate it back and verify the bytes survived the round trip.
        let hydrated = hydrate_span(
            &series,
            &store,
            COMPONENT,
            "round.trip",
            &component_schema(),
            Timestamp(10),
        )
        .await
        .unwrap();
        assert!(hydrated);
        let span = *series.manifest().span(Timestamp(10)).unwrap();
        assert_eq!(span.state, SpanState::Resident);
        assert_eq!(span.source, SpanSource::RemoteFetch);
        assert!(span.acked);
        let sample = series.get(Timestamp(12)).expect("hydrated sample");
        assert_eq!(sample.data(), 12u64.to_le_bytes());

        // A second hydrate finds nothing to do (already resident).
        assert!(
            !hydrate_span(
                &series,
                &store,
                COMPONENT,
                "round.trip",
                &component_schema(),
                Timestamp(10),
            )
            .await
            .unwrap()
        );
    }

    #[stellarator::test]
    async fn purged_span_hydrates_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let series_dir = dir.path().join("component");
        std::fs::create_dir_all(&series_dir).unwrap();
        let store = LocalDirStore::new(dir.path().join("store"));
        let schema = schema();

        {
            let series = series_with_history(&series_dir);
            offload_span(
                &series,
                &store,
                COMPONENT,
                "round.trip",
                &schema,
                Timestamp(20),
            )
            .await
            .unwrap();
            assert!(series.purge_span(Timestamp(20)).unwrap());
        }

        let series = TimeSeries::open(&series_dir).unwrap();
        assert_eq!(
            series.manifest().span(Timestamp(20)).unwrap().state,
            SpanState::RemoteOnly
        );
        assert!(
            hydrate_span(
                &series,
                &store,
                COMPONENT,
                "round.trip",
                &component_schema(),
                Timestamp(20),
            )
            .await
            .unwrap()
        );
        let sample = series.get(Timestamp(21)).expect("hydrated sample");
        assert_eq!(sample.data(), 21u64.to_le_bytes());
        // The list is correctly ordered: 30 (head), 20, 10.
        let starts: Vec<i64> = series
            .list
            .iter()
            .filter_map(|n| n.timestamps().first().map(|t| t.0))
            .collect();
        assert_eq!(starts, vec![30, 20, 10]);
    }
}
