//! Derived min/max LoD components — the data behind wide plot views.
//!
//! Every eligible source component gets a cascade of hidden companions
//! (`{src}.__metor_internal__.lod_{ratio}`) holding per-bucket minima and
//! maxima as ordinary time-series samples. They store, seal, manifest,
//! tier, offload, and hydrate exactly like user components, so a year-wide
//! plot is just a `GetTimeSeries` against a component ~`2/ratio` the size
//! of the raw one — no bespoke summary store or protocol.
//!
//! Buckets are epoch-aligned windows of a fixed duration derived once from
//! the source's observed rate and persisted in the LoD component's
//! metadata, so boundaries never move across restarts. The engine only
//! folds sealed history: a bucket is emitted when the sealed frontier
//! passes its end, which makes emission deterministic, idempotent, and
//! strictly monotonic (the live raw tail renders on top of the last
//! bucket). Only the system of record runs the engine — mirrors receive
//! LoD components through manifest seeding + hydration like any other
//! component, because their raw copy is lossy and locally-computed buckets
//! would disagree with the origin's.

use std::{
    collections::HashSet,
    sync::{Arc, atomic},
    time::Duration,
};

use metor_proto::types::{ComponentId, PrimType, Timestamp};
use metor_proto_wkt::ComponentMetadata;
use smallvec::SmallVec;
use tracing::{debug, info, warn};

use crate::{
    Component, ComponentSchema, DB, Error,
    manifest::SpanState,
    time_series_2::{TimeSeries, TimeSeriesNodeSlice, TimerSeriesWriter},
};

/// Bucket-size cascade, as multiples of the source's mean sample period.
/// Adjacent levels are far enough apart that the panel's level selection
/// never flaps, and three levels span "1M raw samples on screen" to
/// year-scale archives at multi-kHz rates.
pub const LOD_RATIOS: [u64; 3] = [64, 4096, 262144];
pub const LOD_INFIX: &str = ".__metor_internal__.lod_";
/// Data-time age at which LoD head nodes roll. Coarse levels write so
/// slowly that a 32 MB node would never fill, and mirrors only learn
/// about sealed spans — this bounds how far mirror LoD lags the origin.
pub const LOD_NODE_MAX_AGE: Duration = Duration::from_secs(3600);

/// [`LOD_NODE_MAX_AGE`], unless the `METOR_MAX_NODE_AGE_SECS` dev knob
/// asks every component to seal faster.
pub fn lod_node_max_age() -> Duration {
    crate::max_node_age_override().unwrap_or(LOD_NODE_MAX_AGE)
}

/// Metadata keys on LoD components. `lod_source_id` is what the panel and
/// the engine resolve levels by — name-derived ids would dangle when the
/// source is renamed. `lod_bucket_us` makes bucket boundaries restart-stable.
pub const LOD_SOURCE_ID_KEY: &str = "lod_source_id";
pub const LOD_RATIO_KEY: &str = "lod_ratio";
pub const LOD_BUCKET_US_KEY: &str = "lod_bucket_us";

/// Levels whose buckets would span more than this are skipped: a slow
/// source's raw history is small enough to render directly long before a
/// multi-day bucket could help.
const MAX_BUCKET_US: i64 = 86_400_000_000;
/// Buckets emitted per level per pass; bounds the mmap scan so a year of
/// backfill never pins the executor between sleeps.
const MAX_BUCKETS_PER_PASS: i64 = 16_384;
const PASS_PAUSE: Duration = Duration::from_millis(10);
/// Backstop for the lost-wakeup race between a pass and `seal_waiter`.
const IDLE_RECHECK: Duration = Duration::from_secs(60);

pub fn lod_name(source: &str, ratio: u64) -> String {
    format!("{source}{LOD_INFIX}{ratio}")
}

pub fn is_lod_name(name: &str) -> bool {
    name.contains(LOD_INFIX)
}

/// The derived schema: `[2, ..source shape]` f32, one min and one max per
/// source element, laid out `[min_e0..min_eN, max_e0..max_eN]` per sample.
pub fn lod_schema(source: &ComponentSchema) -> ComponentSchema {
    let mut dim = SmallVec::with_capacity(source.dim.len() + 1);
    dim.push(2);
    dim.extend(source.dim.iter().copied());
    ComponentSchema {
        prim_type: PrimType::F32,
        dim,
    }
}

/// Spawn the engine: watches for new eligible components and runs one
/// downsampling task per source. Call this only on the DB that ingests
/// the raw data (the system of record), never on mirrors.
pub fn spawn(db: Arc<DB>) {
    stellarator::spawn(run(db));
}

async fn run(db: Arc<DB>) {
    let mut tracked: HashSet<ComponentId> = HashSet::new();
    loop {
        let new: Vec<Component> = db.with_state(|state| {
            state
                .component_metadata_iter()
                .filter(|(id, meta)| !tracked.contains(id) && eligible(meta))
                .filter_map(|(id, _)| state.get_component(*id).cloned())
                .collect()
        });
        for src in new {
            tracked.insert(src.component_id);
            stellarator::spawn(downsample_source(db.clone(), src));
        }
        // Raced against a periodic recheck: a registration between the scan
        // above and the wait registering would otherwise be lost (wake_all
        // doesn't store wakeups), orphaning the component until some
        // unrelated vtable change.
        futures_lite::future::race(
            async {
                db.vtable_gen.wait().await;
            },
            stellarator::sleep(IDLE_RECHECK),
        )
        .await;
    }
}

fn eligible(meta: &ComponentMetadata) -> bool {
    !meta.is_hidden() && !meta.is_string() && !is_lod_name(&meta.name)
}

async fn downsample_source(db: Arc<DB>, src: Component) {
    let seal_waiter = src.time_series.seal_waiter();
    // The first sealed history is the rate estimate `bucket_us` derives
    // from; nothing can be emitted before a seal exists anyway.
    let period_us = loop {
        if let Some(period) = estimate_period_us(&src.time_series) {
            break period;
        }
        // Same lost-wakeup backstop as the pass loop below: a seal firing
        // between the estimate and the wait registering would otherwise
        // stall level creation until the *next* seal — days for a slow
        // component.
        futures_lite::future::race(
            async {
                let _ = seal_waiter.wait().await;
            },
            stellarator::sleep(IDLE_RECHECK),
        )
        .await;
    };
    // Re-read metadata: eligibility flags (`is_string`) and the real name
    // often arrive after the vtable that created the component.
    let Some(meta) = db.with_state(|s| s.get_component_metadata(src.component_id).cloned()) else {
        return;
    };
    if !eligible(&meta) {
        return;
    }
    let mut levels = setup_levels(&db, &src, &meta.name, period_us);
    if levels.is_empty() {
        return;
    }
    info!(src = %meta.name, levels = levels.len(), "lod engine tracking component");
    loop {
        let mut pending = false;
        for level in levels.iter_mut() {
            match level.run_pass(&src) {
                Ok(more) => pending |= more,
                Err(err) => warn!(?err, src = %meta.name, "lod pass failed"),
            }
        }
        if pending {
            stellarator::sleep(PASS_PAUSE).await;
        } else {
            futures_lite::future::race(
                async {
                    let _ = seal_waiter.wait().await;
                },
                stellarator::sleep(IDLE_RECHECK),
            )
            .await;
        }
    }
}

/// Mean sample period across all sealed history, in microseconds. `None`
/// until at least two samples are sealed.
fn estimate_period_us(series: &TimeSeries) -> Option<i64> {
    let manifest = series.manifest();
    let first = manifest.spans.first()?.seal.start_ts.0;
    let last = manifest.spans.last()?.seal.end_ts.0;
    let count: u64 = manifest.spans.iter().map(|s| s.seal.count).sum();
    if count < 2 || last <= first {
        return None;
    }
    Some((((last - first) as i128) / (count as i128 - 1)).max(1) as i64)
}

/// The lowest bucket frontier across `source_id`'s LoD levels: the
/// timestamp up to which *every* level has folded raw history into buckets.
/// Each level persists its frontier as its own latest sample, so this is
/// derived from state with no sidecar bookkeeping. Tiering gates purges on
/// it — a raw span newer than the frontier is still needed to compute its
/// buckets, so it is not cold yet regardless of age.
///
/// `None` when the source has no LoD levels (not eligible, engine not
/// running, or not yet set up); callers then keep their pre-LoD behavior.
/// A level that has emitted nothing reports [`i64::MIN`], pinning the min
/// so no raw history is purged until summarization has actually started.
pub fn summarized_frontier(db: &DB, source_id: ComponentId) -> Option<i64> {
    let source_key = source_id.0.to_string();
    let mut frontier: Option<i64> = None;
    db.with_state(|state| {
        for (id, meta) in state.component_metadata_iter() {
            if meta.metadata.get(LOD_SOURCE_ID_KEY).map(String::as_str) != Some(source_key.as_str())
            {
                continue;
            }
            let Some(bucket_us) = meta
                .metadata
                .get(LOD_BUCKET_US_KEY)
                .and_then(|v| v.parse::<i64>().ok())
            else {
                continue;
            };
            let Some(lod) = state.get_component(*id) else {
                continue;
            };
            let level_frontier = match lod.time_series.latest() {
                Some(latest) => latest.timestamp().0.div_euclid(bucket_us) * bucket_us + bucket_us,
                None => i64::MIN,
            };
            frontier = Some(frontier.map_or(level_frontier, |f| f.min(level_frontier)));
        }
    });
    frontier
}

struct Level {
    writer: TimerSeriesWriter,
    bucket_us: i64,
    /// End of the last scanned bucket window. Seeded from the LoD
    /// component's own latest sample (restart-safe, no sidecar state) and
    /// advanced past scanned-but-empty regions in memory so holes in the
    /// raw history are not rescanned every pass.
    cursor: i64,
    /// Per-pass scratch, owned by the level so a pass — which runs every
    /// [`PASS_PAUSE`] during backfill — reuses these buffers instead of
    /// allocating. All are `clear()`ed/`fill()`ed at the top of each pass;
    /// capacity warms on the first pass and is held for the task's life.
    /// `mins`/`maxs` are sized once to the source element count.
    mins: Vec<f64>,
    maxs: Vec<f64>,
    /// Raw spans that are not locally resident, intersected with the pass
    /// window; buckets touching one are skipped.
    holes: Vec<(i64, i64)>,
    /// The pass window's node slices, collected newest-first then walked in
    /// reverse. `TimeSeriesNodeSlice` owns its `Arc<TimeSeriesNode>`, so it
    /// lives here across passes with no borrow of the transient slice.
    node_slices: Vec<TimeSeriesNodeSlice>,
    /// Folded buckets buffered until the manifest generation re-check, as a
    /// flat byte arena plus an index of `(midpoint ts, byte length)` — one
    /// `Vec<u8>` per bucket would otherwise allocate up to
    /// [`MAX_BUCKETS_PER_PASS`] times per pass.
    emit_buf: Vec<u8>,
    emit_index: Vec<(Timestamp, u32)>,
}

/// Resolve or create the LoD companions for `src`. Existing levels are
/// found by `lod_source_id` so a renamed source keeps its history; new
/// ones derive their bucket duration from `period_us` and persist it.
fn setup_levels(db: &Arc<DB>, src: &Component, src_name: &str, period_us: i64) -> Vec<Level> {
    let mut levels = Vec::new();
    for ratio in LOD_RATIOS {
        let existing = db.with_state(|state| {
            state
                .component_metadata_iter()
                .find(|(_, m)| {
                    m.metadata.get(LOD_SOURCE_ID_KEY).map(String::as_str)
                        == Some(src.component_id.0.to_string().as_str())
                        && m.metadata.get(LOD_RATIO_KEY).map(String::as_str)
                            == Some(ratio.to_string().as_str())
                })
                .map(|(id, m)| {
                    (
                        *id,
                        m.metadata
                            .get(LOD_BUCKET_US_KEY)
                            .and_then(|v| v.parse::<i64>().ok()),
                    )
                })
        });
        let (component_id, bucket_us) = match existing {
            Some((id, Some(bucket_us))) => (id, bucket_us),
            Some((id, None)) => {
                // Metadata survived a crash without its bucket size; data
                // written under the lost size cannot be realigned.
                warn!(?id, src = %src_name, "lod component missing bucket size; skipping level");
                continue;
            }
            None => {
                let bucket_us =
                    (period_us as i128 * ratio as i128).min(i64::MAX as i128) as i64;
                if bucket_us > MAX_BUCKET_US {
                    continue;
                }
                let bucket_us = bucket_us.max(1);
                let name = lod_name(src_name, ratio);
                let id = ComponentId::new(&name);
                match create_level(db, src, name, id, ratio, bucket_us) {
                    Ok(()) => (id, bucket_us),
                    Err(err) => {
                        warn!(?err, src = %src_name, ratio, "failed to create lod component");
                        continue;
                    }
                }
            }
        };
        let Some(lod) = db.with_state(|s| s.get_component(component_id).cloned()) else {
            continue;
        };
        lod.time_series.set_max_node_age(lod_node_max_age());
        let Some(writer) = lod.time_series.writer() else {
            warn!(src = %src_name, ratio, "lod writer already owned; skipping level");
            continue;
        };
        let cursor = match lod.time_series.latest() {
            Some(latest) => latest.timestamp().0.div_euclid(bucket_us) * bucket_us + bucket_us,
            None => src
                .time_series
                .manifest()
                .spans
                .first()
                .map(|s| s.seal.start_ts.0.div_euclid(bucket_us) * bucket_us)
                .unwrap_or(0),
        };
        levels.push(Level::new(writer, bucket_us, cursor, &src.schema));
    }
    levels
}

fn create_level(
    db: &Arc<DB>,
    src: &Component,
    name: String,
    component_id: ComponentId,
    ratio: u64,
    bucket_us: i64,
) -> Result<(), Error> {
    let schema = lod_schema(&src.schema);
    let was_new = db.with_state(|s| s.get_component(component_id).is_none());
    db.with_state_mut(|state| {
        state.insert_engine_component(component_id, schema, &db.path)?;
        let mut meta = ComponentMetadata {
            component_id,
            name,
            metadata: Default::default(),
        }
        .with_hidden();
        meta.metadata
            .insert(LOD_SOURCE_ID_KEY.into(), src.component_id.0.to_string());
        meta.metadata
            .insert(LOD_RATIO_KEY.into(), ratio.to_string());
        meta.metadata
            .insert(LOD_BUCKET_US_KEY.into(), bucket_us.to_string());
        state.set_component_metadata(meta, &db.path)
    })?;
    if was_new {
        // Wake watchers (`wait_for_component`, browser refresh) the same
        // way the dynamic persist op does for direct inserts.
        db.vtable_gen.fetch_add(1, atomic::Ordering::SeqCst);
    }
    Ok(())
}

impl Level {
    /// Build a level with its per-pass scratch sized to the source element
    /// count (`mins`/`maxs` hold one f64 per element). The remaining buffers
    /// start empty and warm their capacity on the first pass.
    fn new(
        writer: TimerSeriesWriter,
        bucket_us: i64,
        cursor: i64,
        src_schema: &ComponentSchema,
    ) -> Self {
        let n = src_schema.dim.iter().product::<usize>().max(1);
        Level {
            writer,
            bucket_us,
            cursor,
            mins: vec![f64::INFINITY; n],
            maxs: vec![f64::NEG_INFINITY; n],
            holes: Vec::new(),
            node_slices: Vec::new(),
            emit_buf: Vec::new(),
            emit_index: Vec::new(),
        }
    }

    /// Emit every complete bucket between the cursor and the source's
    /// sealed frontier, bounded to [`MAX_BUCKETS_PER_PASS`]. Returns true
    /// when more sealed work remains (the caller should run another pass
    /// soon rather than wait for the next seal).
    fn run_pass(&mut self, src: &Component) -> Result<bool, Error> {
        let d = self.bucket_us;
        let manifest = src.time_series.manifest();
        let gen_before = manifest.generation;
        let Some(last_span) = manifest.spans.last() else {
            return Ok(false);
        };
        // Nodes seal strictly behind the live head, so everything before
        // the newest span's end is final.
        let frontier = last_span.cover_end.0.saturating_add(1);
        let emit_end = frontier.div_euclid(d) * d;
        if self.cursor >= emit_end {
            return Ok(false);
        }
        let pass_end =
            emit_end.min(self.cursor.saturating_add(d.saturating_mul(MAX_BUCKETS_PER_PASS)));

        // Disjoint borrows of the per-level scratch: the node walk reads
        // `node_slices` while the fold writes `mins`/`maxs`/`emit_*`, which a
        // single `&mut self` would forbid.
        let Level {
            mins,
            maxs,
            holes,
            node_slices,
            emit_buf,
            emit_index,
            writer,
            cursor,
            ..
        } = self;

        // Buckets touching raw history that is not locally resident are
        // skipped: pre-existing RemoteOnly history (e.g. a seeded mirror's
        // un-hydrated past) has no local bytes to fold. Tiering no longer
        // purges in-window resident spans (it gates on
        // [`summarized_frontier`]), so this only fires for history that was
        // never local to begin with.
        holes.clear();
        holes.extend(
            manifest
                .spans
                .iter()
                .filter(|s| s.state != SpanState::Resident)
                .map(|s| (s.seal.start_ts.0, s.cover_end.0))
                .filter(|(start, end)| *start < pass_end && *end >= *cursor),
        );

        let n = src.schema.dim.iter().product::<usize>().max(1);
        let prim = src.schema.prim_type;
        let prim_size = prim.size();
        let element_size = src.schema.size();

        let mut bucket_start = i64::MIN;
        mins.fill(f64::INFINITY);
        maxs.fill(f64::NEG_INFINITY);
        // Folded buckets are buffered (flat arena + index) and only written
        // after the manifest generation re-check below, so a snapshot that
        // shifted under us emits nothing rather than a bucket built from
        // partial data.
        emit_buf.clear();
        emit_index.clear();

        if let Some(slice) = src
            .time_series
            .get_range(Timestamp(*cursor)..Timestamp(pass_end))
        {
            // Newest-first node order, reversed so buckets emit in time
            // order; nearest-match boundaries can hand back samples just
            // outside the window, so every sample is range-checked.
            node_slices.clear();
            node_slices.extend(slice.as_iter());
            for node_slice in node_slices.iter().rev() {
                let timestamps = node_slice.timestamps();
                let data = node_slice.data();
                for (i, ts) in timestamps.iter().enumerate() {
                    if ts.0 < *cursor || ts.0 >= pass_end {
                        continue;
                    }
                    let bucket = ts.0.div_euclid(d) * d;
                    if bucket != bucket_start {
                        if bucket_start != i64::MIN {
                            Self::fold_bucket(
                                d,
                                bucket_start,
                                mins,
                                maxs,
                                holes,
                                emit_buf,
                                emit_index,
                            );
                        }
                        bucket_start = bucket;
                        mins.fill(f64::INFINITY);
                        maxs.fill(f64::NEG_INFINITY);
                    }
                    let base = i * element_size;
                    for e in 0..n {
                        let offset = base + e * prim_size;
                        let Some(bytes) = data.get(offset..offset + prim_size) else {
                            continue;
                        };
                        let v = read_f64(prim, bytes);
                        if v.is_finite() {
                            mins[e] = mins[e].min(v);
                            maxs[e] = maxs[e].max(v);
                        }
                    }
                }
            }
            if bucket_start != i64::MIN {
                Self::fold_bucket(d, bucket_start, mins, maxs, holes, emit_buf, emit_index);
            }
        }

        // If history shifted under us mid-fold (a seal, install, or purge
        // bumped the manifest), the buffered buckets may be built from a
        // window that no longer reflects the data. Drop them and re-run
        // next pass over a consistent snapshot rather than emit or advance.
        if src.time_series.manifest().generation != gen_before {
            return Ok(true);
        }
        let mut offset = 0usize;
        for (ts, len) in emit_index.iter() {
            let len = *len as usize;
            writer.push_buf(*ts, &emit_buf[offset..offset + len])?;
            offset += len;
        }
        *cursor = pass_end;
        Ok(pass_end < emit_end)
    }

    /// Fold one bucket into the emit arena as a sample at the bucket's
    /// midpoint: the min/max f32 bytes are appended to `emit_buf` and the
    /// midpoint timestamp plus byte length recorded in `emit_index`, so no
    /// per-bucket `Vec` is allocated. Elements with no finite sample store
    /// `(NaN, NaN)` so renderers keep treating holes as holes; buckets over
    /// non-resident raw history are dropped entirely (nothing local to
    /// summarize).
    fn fold_bucket(
        bucket_us: i64,
        bucket_start: i64,
        mins: &[f64],
        maxs: &[f64],
        holes: &[(i64, i64)],
        emit_buf: &mut Vec<u8>,
        emit_index: &mut Vec<(Timestamp, u32)>,
    ) {
        let bucket_end = bucket_start + bucket_us;
        if holes
            .iter()
            .any(|(start, end)| *start < bucket_end && *end >= bucket_start)
        {
            debug!(
                bucket_start,
                "lod skipping bucket over non-resident raw history"
            );
            return;
        }
        let start = emit_buf.len();
        for v in mins {
            let v = if v.is_finite() { *v as f32 } else { f32::NAN };
            emit_buf.extend_from_slice(&v.to_le_bytes());
        }
        for v in maxs {
            let v = if v.is_finite() { *v as f32 } else { f32::NAN };
            emit_buf.extend_from_slice(&v.to_le_bytes());
        }
        let len = (emit_buf.len() - start) as u32;
        emit_index.push((Timestamp(bucket_start + bucket_us / 2), len));
    }
}

pub(crate) fn read_f64(prim: PrimType, bytes: &[u8]) -> f64 {
    fn arr<const N: usize>(bytes: &[u8]) -> [u8; N] {
        bytes.try_into().expect("caller sliced to prim size")
    }
    match prim {
        PrimType::U8 => bytes[0] as f64,
        PrimType::U16 => u16::from_le_bytes(arr(bytes)) as f64,
        PrimType::U32 => u32::from_le_bytes(arr(bytes)) as f64,
        PrimType::U64 => u64::from_le_bytes(arr(bytes)) as f64,
        PrimType::I8 => bytes[0] as i8 as f64,
        PrimType::I16 => i16::from_le_bytes(arr(bytes)) as f64,
        PrimType::I32 => i32::from_le_bytes(arr(bytes)) as f64,
        PrimType::I64 => i64::from_le_bytes(arr(bytes)) as f64,
        PrimType::Bool => (bytes[0] != 0) as u8 as f64,
        PrimType::F32 => f32::from_le_bytes(arr(bytes)) as f64,
        PrimType::F64 => f64::from_le_bytes(arr(bytes)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time_series_2::TimeSeriesNode;
    use std::path::Path;

    fn f64_schema(shape: &[usize]) -> ComponentSchema {
        ComponentSchema::new(PrimType::F64, shape)
    }

    /// A source component with `values` written 10 µs apart from `start`,
    /// sealed except for one trailing head sample so the series has a
    /// live head like real ingest.
    fn sealed_source(dir: &Path, start: i64, values: &[f64]) -> Component {
        let component_id = ComponentId::new("src");
        let schema = f64_schema(&[1]);
        let component_path = dir.join(component_id.to_string());
        let node =
            TimeSeriesNode::create(component_path.join(start.to_string()), Timestamp(start), 8)
                .unwrap();
        for (i, v) in values.iter().enumerate() {
            node.data.write(&v.to_le_bytes()).unwrap();
            node.index
                .write(&Timestamp(start + i as i64 * 10).to_le_bytes())
                .unwrap();
        }
        let head_start = start + values.len() as i64 * 10;
        let head = TimeSeriesNode::create(
            component_path.join(head_start.to_string()),
            Timestamp(head_start),
            8,
        )
        .unwrap();
        head.data.write(&0f64.to_le_bytes()).unwrap();
        head.index.write(&Timestamp(head_start).to_le_bytes()).unwrap();

        let time_series = TimeSeries::open(&component_path).unwrap();
        time_series.seal_rolled_nodes().unwrap();
        Component {
            component_id,
            time_series,
            wal: crate::disruptor::Disruptor::new(1024),
            schema,
            last_timestamp: Arc::new(stellarator::util::AtomicCell::new(Timestamp(i64::MIN))),
        }
    }

    fn level_for(dir: &Path, src: &Component, bucket_us: i64) -> (Component, Level) {
        let component_id = ComponentId::new("src.lod");
        let lod = Component::create(dir, component_id, lod_schema(&src.schema), true).unwrap();
        let writer = lod.time_series.writer().unwrap();
        let cursor = src
            .time_series
            .manifest()
            .spans
            .first()
            .map(|s| s.seal.start_ts.0.div_euclid(bucket_us) * bucket_us)
            .unwrap_or(0);
        (lod, Level::new(writer, bucket_us, cursor, &src.schema))
    }

    fn lod_samples(lod: &Component) -> Vec<(i64, Vec<f32>)> {
        let mut out = Vec::new();
        for node_slice in lod
            .time_series
            .iter_node_slices()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            let element_size = lod.schema.size();
            let data = node_slice.data();
            for (i, ts) in node_slice.timestamps().iter().enumerate() {
                let values = data[i * element_size..(i + 1) * element_size]
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                    .collect();
                out.push((ts.0, values));
            }
        }
        out
    }

    #[stellarator::test]
    async fn buckets_align_to_epoch_and_cover_extremes() {
        let dir = tempfile::tempdir().unwrap();
        // 40 samples at 10 µs spacing starting at t=105: bucket size 100
        // gives epoch-aligned buckets [100, 200), [200, 300), ...
        let mut values = vec![1.0f64; 40];
        values[20] = 100.0; // t = 305, third bucket's max
        values[3] = -7.0; // t = 135, first bucket's min
        let src = sealed_source(dir.path(), 105, &values);
        let (lod, mut level) = level_for(dir.path(), &src, 100);

        assert!(!level.run_pass(&src).unwrap());
        let samples = lod_samples(&lod);
        // Sealed data covers [105, 495]; frontier 496 → emit_end 400, so
        // buckets starting at 100, 200, 300 emit with midpoint stamps.
        assert_eq!(
            samples.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
            vec![150, 250, 350]
        );
        assert_eq!(samples[0].1, vec![-7.0, 1.0]);
        assert_eq!(samples[1].1, vec![1.0, 1.0]);
        assert_eq!(samples[2].1, vec![1.0, 100.0]);
    }

    #[stellarator::test]
    async fn pass_is_idempotent_and_resumes_from_latest() {
        let dir = tempfile::tempdir().unwrap();
        let src = sealed_source(dir.path(), 100, &vec![2.0f64; 50]);
        let (lod, mut level) = level_for(dir.path(), &src, 100);
        assert!(!level.run_pass(&src).unwrap());
        let first = lod_samples(&lod);
        assert!(!first.is_empty());

        // A fresh Level (as after a restart) derives its cursor from the
        // LoD data and re-emits nothing.
        let cursor = lod.time_series.latest().unwrap().timestamp().0;
        let resumed_cursor = cursor.div_euclid(100) * 100 + 100;
        let mut resumed = Level {
            cursor: resumed_cursor,
            ..level
        };
        assert!(!resumed.run_pass(&src).unwrap());
        assert_eq!(lod_samples(&lod), first);
    }

    /// The passive LoD pass must not allocate once warm: a second pass over
    /// the same window reuses the level's scratch buffers rather than growing
    /// their capacity. (`run_pass` runs every `PASS_PAUSE` during backfill.)
    #[stellarator::test]
    async fn warm_pass_reuses_scratch_without_growth() {
        let dir = tempfile::tempdir().unwrap();
        let src = sealed_source(dir.path(), 100, &vec![2.0f64; 50]);
        let (_lod, mut level) = level_for(dir.path(), &src, 100);

        // First pass warms capacity; it must actually populate the buffers.
        let start_cursor = level.cursor;
        assert!(!level.run_pass(&src).unwrap());
        assert!(!level.emit_index.is_empty(), "pass emitted no buckets");
        let caps = (
            level.mins.capacity(),
            level.maxs.capacity(),
            level.holes.capacity(),
            level.node_slices.capacity(),
            level.emit_buf.capacity(),
            level.emit_index.capacity(),
        );

        // Re-scan the identical window: clear()+extend() back to the same
        // lengths must not reallocate any buffer.
        level.cursor = start_cursor;
        assert!(!level.run_pass(&src).unwrap());
        assert_eq!(
            caps,
            (
                level.mins.capacity(),
                level.maxs.capacity(),
                level.holes.capacity(),
                level.node_slices.capacity(),
                level.emit_buf.capacity(),
                level.emit_index.capacity(),
            ),
            "warm pass grew scratch capacity"
        );
    }

    #[stellarator::test]
    async fn non_finite_samples_fold_to_nan_buckets() {
        let dir = tempfile::tempdir().unwrap();
        // First bucket all-NaN, second mixes NaN/Inf with finite values.
        let mut values = vec![f64::NAN; 10];
        values.extend([f64::NAN, 5.0, f64::INFINITY, 3.0]);
        values.extend(vec![f64::NAN; 6]);
        values.extend(vec![1.0f64; 20]);
        let src = sealed_source(dir.path(), 100, &values);
        let (lod, mut level) = level_for(dir.path(), &src, 100);
        assert!(!level.run_pass(&src).unwrap());
        let samples = lod_samples(&lod);
        assert!(samples[0].1[0].is_nan() && samples[0].1[1].is_nan());
        assert_eq!(samples[1].1, vec![3.0, 5.0]);
    }

    #[stellarator::test]
    async fn vector_sources_fold_element_major() {
        let dir = tempfile::tempdir().unwrap();
        let component_id = ComponentId::new("vec_src");
        let schema = f64_schema(&[2]);
        let component_path = dir.path().join(component_id.to_string());
        let node = TimeSeriesNode::create(component_path.join("100"), Timestamp(100), 16).unwrap();
        // 30 samples reach t=390, so the sealed frontier completes both
        // [100, 200) and [200, 300).
        for i in 0..30i64 {
            for v in [i as f64, -(i as f64)] {
                node.data.write(&v.to_le_bytes()).unwrap();
            }
            node.index
                .write(&Timestamp(100 + i * 10).to_le_bytes())
                .unwrap();
        }
        let head =
            TimeSeriesNode::create(component_path.join("400"), Timestamp(400), 16).unwrap();
        for v in [0f64, 0f64] {
            head.data.write(&v.to_le_bytes()).unwrap();
        }
        head.index.write(&Timestamp(400).to_le_bytes()).unwrap();
        let time_series = TimeSeries::open(&component_path).unwrap();
        time_series.seal_rolled_nodes().unwrap();
        let src = Component {
            component_id,
            time_series,
            wal: crate::disruptor::Disruptor::new(1024),
            schema,
            last_timestamp: Arc::new(stellarator::util::AtomicCell::new(Timestamp(i64::MIN))),
        };

        let (lod, mut level) = level_for(dir.path(), &src, 100);
        assert!(!level.run_pass(&src).unwrap());
        let samples = lod_samples(&lod);
        // Layout per bucket: [min_e0, min_e1, max_e0, max_e1].
        // First bucket [100, 200) holds samples 0..10.
        assert_eq!(samples[0].1, vec![0.0, -9.0, 9.0, 0.0]);
        assert_eq!(samples[1].1, vec![10.0, -19.0, 19.0, -10.0]);
    }

    /// A bucket straddling the seam between two sealed nodes must fold
    /// samples from both. Regression: `get_range` used to drop every node
    /// newer than the one holding the range start, so the newer node's
    /// extremes silently vanished from seam buckets.
    #[stellarator::test]
    async fn buckets_straddling_node_seams_fold_both_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let component_id = ComponentId::new("seam_src");
        let schema = f64_schema(&[1]);
        let component_path = dir.path().join(component_id.to_string());
        // Node 1 holds [100, 190] with the bucket's min, node 2 holds
        // [200, 390] with its max; bucket size 300 puts the [0, 300)
        // bucket across the seam.
        for (start, count, spike, spike_at) in
            [(100i64, 10usize, -5.0f64, 5usize), (200, 20, 9.0, 5)]
        {
            let node = TimeSeriesNode::create(
                component_path.join(start.to_string()),
                Timestamp(start),
                8,
            )
            .unwrap();
            for i in 0..count {
                let v = if i == spike_at { spike } else { 1.0 };
                node.data.write(&v.to_le_bytes()).unwrap();
                node.index
                    .write(&Timestamp(start + i as i64 * 10).to_le_bytes())
                    .unwrap();
            }
        }
        let head =
            TimeSeriesNode::create(component_path.join("400"), Timestamp(400), 8).unwrap();
        head.data.write(&0f64.to_le_bytes()).unwrap();
        head.index.write(&Timestamp(400).to_le_bytes()).unwrap();
        let time_series = TimeSeries::open(&component_path).unwrap();
        time_series.seal_rolled_nodes().unwrap();
        let src = Component {
            component_id,
            time_series,
            wal: crate::disruptor::Disruptor::new(1024),
            schema,
            last_timestamp: Arc::new(stellarator::util::AtomicCell::new(Timestamp(i64::MIN))),
        };

        let (lod, mut level) = level_for(dir.path(), &src, 300);
        assert!(!level.run_pass(&src).unwrap());
        let samples = lod_samples(&lod);
        // One complete bucket ([0, 300)); [300, 600) waits on the head.
        assert_eq!(
            samples.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
            vec![150]
        );
        assert_eq!(samples[0].1, vec![-5.0, 9.0]);
    }

    #[stellarator::test]
    async fn buckets_overlapping_purged_spans_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let component_id = ComponentId::new("src");
        let schema = f64_schema(&[1]);
        let component_path = dir.path().join(component_id.to_string());
        // Two sealed nodes plus a head; the older node is then purged.
        for (start, count) in [(100i64, 10usize), (200, 20)] {
            let node = TimeSeriesNode::create(
                component_path.join(start.to_string()),
                Timestamp(start),
                8,
            )
            .unwrap();
            for i in 0..count {
                node.data.write(&4.0f64.to_le_bytes()).unwrap();
                node.index
                    .write(&Timestamp(start + i as i64 * 10).to_le_bytes())
                    .unwrap();
            }
        }
        let head =
            TimeSeriesNode::create(component_path.join("400"), Timestamp(400), 8).unwrap();
        head.data.write(&0f64.to_le_bytes()).unwrap();
        head.index.write(&Timestamp(400).to_le_bytes()).unwrap();
        TimeSeries::open(&component_path)
            .unwrap()
            .seal_rolled_nodes()
            .unwrap();
        std::fs::remove_dir_all(component_path.join("100")).unwrap();

        let src = Component {
            component_id,
            time_series: TimeSeries::open(&component_path).unwrap(),
            wal: crate::disruptor::Disruptor::new(1024),
            schema,
            last_timestamp: Arc::new(stellarator::util::AtomicCell::new(Timestamp(i64::MIN))),
        };
        assert_eq!(
            src.time_series.manifest().span(Timestamp(100)).unwrap().state,
            SpanState::RemoteOnly
        );

        let (lod, mut level) = level_for(dir.path(), &src, 100);
        assert!(!level.run_pass(&src).unwrap());
        // The bucket over the purged span is skipped; the resident span's
        // complete bucket emits normally ([300, 400) waits on the head).
        let samples = lod_samples(&lod);
        assert_eq!(
            samples.iter().map(|(ts, _)| *ts).collect::<Vec<_>>(),
            vec![250]
        );
        assert_eq!(samples[0].1, vec![4.0, 4.0]);
    }

    #[test]
    fn schema_prepends_min_max_dim() {
        let schema = lod_schema(&f64_schema(&[3, 2]));
        assert_eq!(schema.prim_type, PrimType::F32);
        assert_eq!(schema.dim.as_slice(), &[2, 3, 2]);
        assert_eq!(schema.size(), 2 * 6 * 4);
    }

    #[test]
    fn lod_names_round_trip() {
        let name = lod_name("cube_sat.imu.accel", 4096);
        assert_eq!(name, "cube_sat.imu.accel.__metor_internal__.lod_4096");
        assert!(is_lod_name(&name));
        assert!(!is_lod_name("cube_sat.imu.accel"));
    }
}
