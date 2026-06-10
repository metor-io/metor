//! Background eviction of cold sealed spans.
//!
//! One task per DB walks every component's manifest and frees local bytes
//! under two policies: an age limit (data older than `max_age` moves out)
//! and a size budget (when resident bytes exceed `max_db_bytes`, the
//! oldest evictable spans go first). Eviction is always offload-then-purge
//! — a span leaves disk only once a store holds a durable, acked copy —
//! and data with no store copy and no configured store is never touched.

use std::{sync::Arc, time::Duration};

use metor_proto::types::{ComponentId, Timestamp};
use tracing::{debug, warn};

use crate::{
    Component, DB, Error,
    manifest::{SpanSource, SpanState},
    remote::offload_span,
    store::NodeStore,
};

pub struct TieringConfig {
    /// Purge oldest evictable spans once resident sealed bytes exceed this.
    pub max_db_bytes: Option<u64>,
    /// Evict spans whose newest sample is older than this.
    pub max_age: Option<Duration>,
    /// Never purge a span newer than this, whatever the policies say —
    /// views are almost certainly still reading the recent past.
    pub min_resident: Duration,
    pub check_interval: Duration,
}

impl Default for TieringConfig {
    fn default() -> Self {
        Self {
            max_db_bytes: None,
            max_age: None,
            min_resident: Duration::from_secs(600),
            check_interval: Duration::from_secs(30),
        }
    }
}

/// Spawn the tiering task on the current runtime. `store` is both the
/// offload target and what `acked` flags refer to; without one, only
/// already-acked spans (e.g. hydrated cache copies) are evictable.
pub fn spawn(db: Arc<DB>, store: Option<Arc<dyn NodeStore>>, config: TieringConfig) {
    stellarator::spawn(run(db, store, config));
}

pub async fn run(db: Arc<DB>, store: Option<Arc<dyn NodeStore>>, config: TieringConfig) {
    loop {
        if let Err(err) = tier_once(&db, store.as_deref(), &config, Timestamp::now()).await {
            warn!(?err, "tiering pass failed");
        }
        stellarator::sleep(config.check_interval).await;
    }
}

/// One evictable sealed span, with everything needed to offload it.
struct Victim<'a> {
    component_id: ComponentId,
    name: &'a str,
    component: &'a Component,
    start_ts: Timestamp,
    end_ts: Timestamp,
    bytes: u64,
    acked: bool,
    /// Hydrated from a store — purging is free, no upload needed.
    cache_copy: bool,
}

impl Victim<'_> {
    /// Budget-pass priority: free cache evictions, then spans a store
    /// already acked, then spans that still need an upload.
    fn class(&self) -> u8 {
        if self.cache_copy {
            0
        } else if self.acked {
            1
        } else {
            2
        }
    }
}

/// Run both policies once against the DB's current state.
pub async fn tier_once(
    db: &Arc<DB>,
    store: Option<&dyn NodeStore>,
    config: &TieringConfig,
    now: Timestamp,
) -> Result<(), Error> {
    if config.max_age.is_none() && config.max_db_bytes.is_none() {
        return Ok(());
    }
    let components: Vec<(ComponentId, String, Component)> = db.with_state(|state| {
        state
            .components
            .iter()
            .map(|(id, component)| {
                let name = state
                    .get_component_metadata(*id)
                    .map(|m| m.name.clone())
                    .unwrap_or_else(|| id.to_string());
                (*id, name, component.clone())
            })
            .collect()
    });
    let min_resident_cutoff =
        Timestamp(now.0.saturating_sub(config.min_resident.as_micros() as i64));
    let (mut victims, mut resident_bytes) = collect_victims(&components, min_resident_cutoff);

    if let Some(max_age) = config.max_age {
        let cutoff = Timestamp(now.0.saturating_sub(max_age.as_micros() as i64));
        victims.sort_unstable_by_key(|v| v.end_ts.0);
        let mut remaining = Vec::with_capacity(victims.len());
        for victim in victims {
            if victim.end_ts.0 < cutoff.0 && try_evict(&victim, store).await {
                resident_bytes = resident_bytes.saturating_sub(victim.bytes);
                continue;
            }
            remaining.push(victim);
        }
        victims = remaining;
    }

    if let Some(budget) = config.max_db_bytes {
        victims.sort_unstable_by_key(|v| (v.class(), v.end_ts.0));
        for victim in &victims {
            if resident_bytes <= budget {
                break;
            }
            if try_evict(victim, store).await {
                resident_bytes = resident_bytes.saturating_sub(victim.bytes);
            }
        }
        if resident_bytes > budget {
            debug!(
                resident_bytes,
                budget, "over byte budget with no evictable spans left"
            );
        }
    }
    Ok(())
}

/// Every resident sealed span older than `min_resident_cutoff`, plus the
/// total resident sealed bytes (including spans too fresh to evict). The
/// live head is unsealed and never appears in manifests, so it is
/// implicitly protected.
fn collect_victims<'a>(
    components: &'a [(ComponentId, String, Component)],
    min_resident_cutoff: Timestamp,
) -> (Vec<Victim<'a>>, u64) {
    let mut victims = Vec::new();
    let mut resident_bytes = 0u64;
    for (component_id, name, component) in components {
        let manifest = component.time_series.manifest();
        for span in manifest
            .spans
            .iter()
            .filter(|s| s.state == SpanState::Resident)
        {
            resident_bytes += span.bytes();
            if span.seal.end_ts.0 >= min_resident_cutoff.0 {
                continue;
            }
            victims.push(Victim {
                component_id: *component_id,
                name,
                component,
                start_ts: span.seal.start_ts,
                end_ts: span.seal.end_ts,
                bytes: span.bytes(),
                acked: span.acked,
                cache_copy: span.source == SpanSource::RemoteFetch,
            });
        }
    }
    (victims, resident_bytes)
}

/// [`evict`], but a failure only costs this victim: a down store must not
/// block the rest of the pass, including free evictions that need no
/// store contact at all.
async fn try_evict(victim: &Victim<'_>, store: Option<&dyn NodeStore>) -> bool {
    match evict(victim, store).await {
        Ok(purged) => purged,
        Err(err) => {
            warn!(
                ?err,
                component = %victim.name,
                start_ts = victim.start_ts.0,
                "failed to evict span"
            );
            false
        }
    }
}

/// Offload (when needed and possible) then purge. `Ok(false)` means the
/// span stayed resident — typically the sole copy with no store to send
/// it to, which is never an excuse to drop data.
async fn evict(victim: &Victim<'_>, store: Option<&dyn NodeStore>) -> Result<bool, Error> {
    if !victim.acked {
        let Some(store) = store else {
            warn!(
                component = %victim.name,
                start_ts = victim.start_ts.0,
                "span is past its retention policy but holds the only copy; not purging"
            );
            return Ok(false);
        };
        let schema = victim.component.schema.to_schema();
        offload_span(
            &victim.component.time_series,
            store,
            victim.component_id,
            victim.name,
            &schema,
            victim.start_ts,
        )
        .await?;
    }
    victim.component.time_series.purge_span(victim.start_ts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ComponentMetadata, ComponentSchema, MetadataExt, store::LocalDirStore,
        time_series_2::TimeSeriesNode,
    };
    use metor_proto::types::PrimType;
    use metor_proto_wkt::DbConfig;
    use std::path::Path;

    const COMPONENT: ComponentId = ComponentId(9001);

    /// Lay a component with sealed history directly on disk, then open it
    /// as a real DB so tiering sees the same state a long session leaves.
    fn build_db(dir: &Path, starts: &[i64]) -> Arc<DB> {
        let comp_dir = dir.join(COMPONENT.to_string());
        for start in starts {
            let node =
                TimeSeriesNode::create(comp_dir.join(start.to_string()), Timestamp(*start), 8)
                    .unwrap();
            for i in 0..4 {
                node.data.write(&((start + i) as u64).to_le_bytes()).unwrap();
                node.index
                    .write(&Timestamp(start + i).to_le_bytes())
                    .unwrap();
            }
        }
        ComponentSchema::new(PrimType::U64, &[1])
            .write(comp_dir.join("schema"))
            .unwrap();
        ComponentMetadata {
            component_id: COMPONENT,
            name: "tiering.test".into(),
            metadata: Default::default(),
        }
        .write(comp_dir.join("metadata"))
        .unwrap();
        DbConfig {
            recording: true,
            default_stream_time_step: Duration::from_millis(8),
            metadata: Default::default(),
        }
        .write(dir.join("db_state"))
        .unwrap();
        let db = Arc::new(DB::open(dir.to_path_buf()).unwrap());
        let component = db
            .with_state(|s| s.components.get(&COMPONENT).cloned())
            .unwrap();
        component.time_series.seal_rolled_nodes().unwrap();
        db
    }

    fn resident_starts(db: &Arc<DB>) -> Vec<i64> {
        let component = db
            .with_state(|s| s.components.get(&COMPONENT).cloned())
            .unwrap();
        component
            .time_series
            .manifest()
            .spans
            .iter()
            .filter(|s| s.state == SpanState::Resident)
            .map(|s| s.seal.start_ts.0)
            .collect()
    }

    #[stellarator::test]
    async fn age_policy_offloads_then_purges() {
        let dir = tempfile::tempdir().unwrap();
        let db = build_db(&dir.path().join("db"), &[100, 200, 300]);
        let store = LocalDirStore::new(dir.path().join("store"));
        let config = TieringConfig {
            max_age: Some(Duration::from_micros(750)),
            min_resident: Duration::ZERO,
            ..Default::default()
        };
        // now=1000: the sealed spans end at 103 and 203, both past
        // max_age. 300 is the unsealed live head — never a candidate.
        tier_once(&db, Some(&store), &config, Timestamp(1000))
            .await
            .unwrap();
        assert_eq!(resident_starts(&db), Vec::<i64>::new());
        let component = db
            .with_state(|s| s.components.get(&COMPONENT).cloned())
            .unwrap();
        assert_eq!(component.time_series.latest().unwrap().timestamp().0, 303);
        // The purged spans are remote-only, not forgotten.
        let component = db
            .with_state(|s| s.components.get(&COMPONENT).cloned())
            .unwrap();
        let manifest = component.time_series.manifest();
        assert_eq!(manifest.span(Timestamp(100)).unwrap().state, SpanState::RemoteOnly);
        assert!(manifest.span(Timestamp(100)).unwrap().acked);
    }

    #[stellarator::test]
    async fn budget_policy_evicts_oldest_until_under() {
        let dir = tempfile::tempdir().unwrap();
        let db = build_db(&dir.path().join("db"), &[100, 200, 300]);
        let store = LocalDirStore::new(dir.path().join("store"));
        let span_bytes = {
            let component = db
                .with_state(|s| s.components.get(&COMPONENT).cloned())
                .unwrap();
            component.time_series.manifest().spans[0].bytes()
        };
        let config = TieringConfig {
            // Room for one sealed span: the two oldest must go.
            max_db_bytes: Some(span_bytes + span_bytes / 2),
            min_resident: Duration::ZERO,
            ..Default::default()
        };
        tier_once(&db, Some(&store), &config, Timestamp(1000))
            .await
            .unwrap();
        assert_eq!(resident_starts(&db), vec![200]);
    }

    #[stellarator::test]
    async fn sole_copies_survive_without_a_store() {
        let dir = tempfile::tempdir().unwrap();
        let db = build_db(&dir.path().join("db"), &[100, 200]);
        let config = TieringConfig {
            max_age: Some(Duration::from_micros(1)),
            max_db_bytes: Some(0),
            min_resident: Duration::ZERO,
            ..Default::default()
        };
        tier_once(&db, None, &config, Timestamp(1000)).await.unwrap();
        // The policies wanted everything gone, but the sealed span has no
        // store copy and there is no store to make one.
        assert_eq!(resident_starts(&db), vec![100]);
    }

    #[stellarator::test]
    async fn min_resident_protects_fresh_spans() {
        let dir = tempfile::tempdir().unwrap();
        let db = build_db(&dir.path().join("db"), &[100, 200]);
        let store = LocalDirStore::new(dir.path().join("store"));
        let config = TieringConfig {
            max_db_bytes: Some(0),
            min_resident: Duration::from_micros(2000),
            ..Default::default()
        };
        // Everything is newer than now - min_resident; nothing may move.
        tier_once(&db, Some(&store), &config, Timestamp(1000))
            .await
            .unwrap();
        assert_eq!(resident_starts(&db), vec![100]);
    }
}
