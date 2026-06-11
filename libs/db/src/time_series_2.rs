use std::{
    fmt::Debug,
    ops::{Range, RangeInclusive},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use metor_proto::types::{ComponentView, Timestamp};

use crate::ComponentSchema;
use stellarator::sync::WaitQueue;
use tracing::warn;
use zerocopy::FromBytes;

use crate::{
    Error,
    append_log::AppendLog,
    arc_ring::{AtomicNode, AtomicStack, AtomicStackIter},
    disruptor::ArcAtomic,
    manifest::{ComponentManifest, Coverage, GapVec, ManifestCell, NodeSpan, SpanSource, SpanState},
    seal::{SealRecord, SealRecordExt, seal_node},
};

#[derive(Clone)]
pub struct TimeSeries {
    pub list: Arc<AtomicStack<TimeSeriesNode>>,
    path: PathBuf,
    data_waker: Arc<WaitQueue>,
    has_writer: Arc<AtomicBool>,
    /// Sealed-span bookkeeping; see [`ManifestCell`]. The live head node
    /// is answered by `list`, not the manifest.
    manifest: Arc<ManifestCell>,
    /// Serializes multi-step structural work (sealing, hydration install,
    /// purge) across background tasks. Readers and the ingest writer never
    /// take it.
    structural: Arc<Mutex<()>>,
    /// Woken when the writer rolls to a new node, signalling that the
    /// previous head is ready to seal.
    lifecycle_waker: Arc<WaitQueue>,
}

#[derive(Clone)]
pub struct TimeSeriesNode {
    pub index: AppendLog<Timestamp>,
    pub data: AppendLog<u64>,
}

impl Debug for TimeSeriesNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimeSeriesNode").finish()
    }
}

/// Capacity of each node's index and data files. Sealed nodes are sparse
/// up to this size; only the committed prefix ever travels over a store.
pub const NODE_SIZE: u64 = 1024 * 1024 * 32;

impl TimeSeriesNode {
    pub fn create(
        path: impl AsRef<Path>,
        start_timestamp: Timestamp,
        element_size: u64,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let index = AppendLog::with_size(NODE_SIZE, path.join("index"), start_timestamp)?;
        let data = AppendLog::with_size(NODE_SIZE, path.join("data"), element_size)?;
        let time_series = Self { index, data };
        Ok(time_series)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let index = AppendLog::open(path.join("index"))?;
        let data = AppendLog::open(path.join("data"))?;
        let time_series = Self { index, data };
        Ok(time_series)
    }

    pub fn timestamps(&self) -> &[Timestamp] {
        <[Timestamp]>::ref_from_bytes(self.index.get(..).expect("couldn't get full range"))
            .expect("mmep unaligned")
    }

    pub fn element_size(&self) -> usize {
        *self.data.extra() as usize
    }
}

/// Rebuild the manifest on open: the directory scan (already loaded into
/// `list`) is authoritative for residency; the persisted manifest
/// contributes remote-only spans and flags (`acked`, `source`) for spans
/// that are still resident. Nodes without a seal record stay out of the
/// manifest until the lifecycle task seals them. Corrupt sidecars degrade
/// the same way — a bad manifest behaves as absent, a bad seal leaves its
/// node unsealed — so one torn file never blocks opening the component.
fn reconcile_manifest(path: &Path, list: &AtomicStack<TimeSeriesNode>) -> ComponentManifest {
    let persisted = match ComponentManifest::read_from(path) {
        Ok(persisted) => persisted.unwrap_or_default(),
        Err(e) => {
            warn!(?path, ?e, "failed to read component manifest");
            ComponentManifest::default()
        }
    };
    let mut spans: Vec<NodeSpan> = Vec::new();
    for node in list.iter() {
        let Some(start_ts) = node.timestamps().first().copied() else {
            continue;
        };
        let node_dir = path.join(start_ts.0.to_string());
        let seal = match SealRecord::read(&node_dir) {
            Ok(Some(seal)) => seal,
            Ok(None) => continue,
            Err(e) => {
                warn!(?node_dir, ?e, "failed to read seal record");
                continue;
            }
        };
        let flags = persisted.span(start_ts);
        spans.push(NodeSpan::whole(
            seal,
            SpanState::Resident,
            flags.map(|s| s.source).unwrap_or(SpanSource::LocalIngest),
            flags.map(|s| s.acked).unwrap_or(false),
        ));
    }
    for span in &persisted.spans {
        // A span with no resident node was purged (or never fetched);
        // whatever state was persisted, locally it is remote-only now.
        if !spans.iter().any(|s| s.seal.start_ts == span.seal.start_ts) {
            spans.push(NodeSpan {
                state: SpanState::RemoteOnly,
                ..*span
            });
        }
    }
    spans.sort_unstable_by_key(|s| s.seal.start_ts.0);
    ComponentManifest {
        generation: 0,
        spans: spans.into_boxed_slice(),
    }
}

/// Timestamp-named node directories under `path`, oldest first. The stack
/// makes the last-pushed node its head, so pushing in this order restores
/// the newest-first iteration invariant that queries and the writer rely
/// on; `read_dir` alone yields filesystem order, not time order.
pub(crate) fn node_dirs_oldest_first(path: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut dirs: Vec<(i64, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let node_path = entry?.path();
        if !node_path.is_dir() {
            continue;
        }
        let Some(start_ts) = node_path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.parse::<i64>().ok())
        else {
            continue;
        };
        dirs.push((start_ts, node_path));
    }
    dirs.sort_unstable_by_key(|(start_ts, _)| *start_ts);
    Ok(dirs.into_iter().map(|(_, path)| path).collect())
}

/// Write the node's summary sidecar unless a current one exists. Summary
/// data is derived — failing to produce it must never fail the seal or
/// install that triggered it, so errors only warn.
fn write_summary_sidecar(
    node: &TimeSeriesNode,
    schema: &ComponentSchema,
    seal: &SealRecord,
    node_dir: &Path,
) {
    use crate::summary::NodeSummary;
    if NodeSummary::read_header(node_dir, seal.checksum).is_some() {
        return;
    }
    if let Some(summary) = NodeSummary::compute(node, schema, seal)
        && let Err(e) = summary.write(node_dir)
    {
        warn!(?node_dir, ?e, "failed to write node summary");
    }
}

/// Delete staging directories left behind by fetches that never
/// committed. Runs at open, before any fetch task exists, so every
/// `.fetching` directory is guaranteed orphaned.
fn remove_stale_staging_dirs(path: &Path) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.is_dir()
            && dir.extension().is_some_and(|ext| ext == "fetching")
            && let Err(e) = std::fs::remove_dir_all(&dir)
        {
            warn!(?dir, ?e, "failed to remove stale staging directory");
        }
    }
}

#[derive(Clone)]
pub struct TimestampRef {
    node: Arc<AtomicNode<TimeSeriesNode>>,
    timestamp: Timestamp,
    index: usize,
}

#[derive(Clone)]
pub struct TimeSeriesNodeSlice {
    node: Arc<AtomicNode<TimeSeriesNode>>,
    range: RangeInclusive<usize>,
}

impl TimeSeriesNodeSlice {
    pub fn timestamps(&self) -> &[Timestamp] {
        let start: usize = *self.range.start();
        let mut end = *self.range.end();
        if end >= self.node.timestamps().len() {
            println!(
                "End index out of bounds {end:?} >= {}",
                self.node.timestamps().len()
            );
            end = self.node.timestamps().len().saturating_sub(1);
        }
        &self.node.timestamps()[start..=end]
    }

    pub fn data(&self) -> &[u8] {
        let element_size = self.node.element_size();
        let data_len = self.node.data.data().len();
        let start = (self.range.start() * element_size).min(data_len);
        let end = (self.range.end().saturating_add(1) * element_size).min(data_len);
        &self.node.data.data()[start..end]
    }

    /// Stable identity for the underlying node, suitable for use as a cache key.
    /// Two slices that share the same node will return the same id.
    pub fn node_id(&self) -> usize {
        Arc::as_ptr(&self.node) as usize
    }

    /// Timestamps for every sample in the underlying node, ignoring this slice's
    /// visible range. Useful for caches that index by full-node identity.
    pub fn full_timestamps(&self) -> &[Timestamp] {
        self.node.timestamps()
    }

    /// Raw sample bytes for every sample in the underlying node, ignoring this
    /// slice's visible range.
    pub fn full_data(&self) -> &[u8] {
        self.node.data.data()
    }

    pub fn iter_values<'a>(
        &'a self,
        schema: &'a ComponentSchema,
    ) -> impl Iterator<Item = (Timestamp, ComponentView<'a>)> + 'a {
        let element_size = schema.size();
        let timestamps = self.timestamps();
        let data = self.data();
        timestamps.iter().enumerate().filter_map(move |(i, ts)| {
            let start = i * element_size;
            let buf = data.get(start..start + element_size)?;
            let (_size, view) = schema.parse_value(buf).ok()?;
            Some((*ts, view))
        })
    }
}

pub struct TimeSeriesSlice {
    start: TimestampRef,
    end: TimestampRef,
}

impl TimeSeriesSlice {
    pub fn as_iter(&self) -> impl Iterator<Item = TimeSeriesNodeSlice> + '_ {
        let iter: AtomicStackIter<TimeSeriesNode> =
            AtomicStackIter::new(ArcAtomic::from(self.start.node.clone()));
        iter.map(move |node| {
            let start = if Arc::ptr_eq(&node, &self.start.node) {
                self.start.index
            } else {
                0
            };
            let end = if Arc::ptr_eq(&node, &self.end.node) {
                self.end.index
            } else {
                node.timestamps().len().saturating_sub(1)
            };
            TimeSeriesNodeSlice {
                range: start..=end,
                node,
            }
        })
    }

    pub fn len(&self) -> usize {
        self.as_iter().map(|node| node.timestamps().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl TimestampRef {
    pub fn timestamp(&self) -> Timestamp {
        self.timestamp
    }

    pub fn data(&self) -> &[u8] {
        self.node
            .data
            .get(self.index * self.node.element_size()..(self.index + 1) * self.node.element_size())
            .expect("buffer out of bounds")
    }
}

impl TimeSeries {
    pub fn create(path: impl AsRef<Path>) -> Result<Self, Error> {
        Ok(Self {
            list: Arc::new(AtomicStack::new()),
            path: path.as_ref().to_path_buf(),
            data_waker: Arc::new(WaitQueue::new()),
            has_writer: Arc::new(AtomicBool::new(false)),
            manifest: Arc::new(ManifestCell::default()),
            structural: Arc::new(Mutex::new(())),
            lifecycle_waker: Arc::new(WaitQueue::new()),
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let list = Arc::new(AtomicStack::new());

        if path.exists() {
            remove_stale_staging_dirs(path);
            for node_path in node_dirs_oldest_first(path)? {
                match TimeSeriesNode::open(&node_path) {
                    Ok(node) => {
                        list.push(node);
                    }
                    Err(e) => {
                        warn!(?node_path, ?e, "failed to open time series node");
                    }
                }
            }
        }

        let manifest = reconcile_manifest(path, &list);
        Ok(Self {
            list,
            path: path.to_path_buf(),
            data_waker: Arc::new(WaitQueue::new()),
            has_writer: Arc::new(AtomicBool::new(false)),
            manifest: Arc::new(ManifestCell::new(manifest)),
            structural: Arc::new(Mutex::new(())),
            lifecycle_waker: Arc::new(WaitQueue::new()),
        })
    }

    /// The current sealed-span snapshot.
    pub fn manifest(&self) -> Arc<ComponentManifest> {
        self.manifest.load()
    }

    /// Classify how much of `range` is locally answerable, appending the
    /// remote-only sub-ranges to `gaps`. Never blocks and never allocates
    /// beyond `gaps`' spill; safe to call from a render loop.
    pub fn coverage(&self, range: Range<Timestamp>, gaps: &mut GapVec) -> Coverage {
        self.manifest.load().gaps(&range, gaps);
        if gaps.is_empty() {
            return Coverage::Complete;
        }
        let any_resident = self.list.iter().any(|node| {
            let timestamps = node.timestamps();
            match (timestamps.first(), timestamps.last()) {
                (Some(start), Some(end)) => start.0 < range.end.0 && range.start.0 <= end.0,
                _ => false,
            }
        });
        if any_resident {
            Coverage::Partial
        } else {
            Coverage::Empty
        }
    }

    /// Seal every resident node the writer has rolled past, summarize it,
    /// and record it in the manifest. Returns true when anything changed.
    /// Runs on the lifecycle task; checksums, summary passes, and fsyncs
    /// make it unsuitable for hot paths.
    pub fn seal_rolled_nodes(&self, schema: &ComponentSchema) -> Result<bool, Error> {
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        let mut sealed: Vec<NodeSpan> = Vec::new();
        // Newest-first; the first node is the live head and must not be
        // sealed — the writer may still be appending to it.
        for node in self.list.iter().skip(1) {
            let Some(start_ts) = node.timestamps().first().copied() else {
                continue;
            };
            if manifest.span(start_ts).is_some() {
                continue;
            }
            let node_dir = self.path.join(start_ts.0.to_string());
            let seal = match SealRecord::read(&node_dir)? {
                Some(seal) => seal,
                None => match seal_node(&node, &node_dir)? {
                    Some(seal) => seal,
                    None => continue,
                },
            };
            write_summary_sidecar(&node, schema, &seal, &node_dir);
            sealed.push(NodeSpan::whole(
                seal,
                SpanState::Resident,
                SpanSource::LocalIngest,
                false,
            ));
        }
        if sealed.is_empty() {
            return Ok(false);
        }
        self.publish_manifest(true, |spans| spans.extend(sealed))?;
        Ok(true)
    }

    /// Background task that seals nodes as the writer rolls past them.
    /// Spawn one per component alongside the persist task.
    pub async fn lifecycle(self, schema: ComponentSchema) {
        loop {
            if let Err(err) = self.seal_rolled_nodes(&schema) {
                warn!(?err, path = ?self.path, "failed to seal rolled nodes");
            }
            let _ = self.lifecycle_waker.wait().await;
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Claim a remote-only span for fetching. Returns the claimed span
    /// (its seal keys the fetch; its coverage bounds what may install)
    /// when the claim won; `None` means the span is missing, already
    /// resident, or another fetch is in flight.
    pub fn begin_fetch(&self, start_ts: Timestamp) -> Option<NodeSpan> {
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        let span = manifest.span(start_ts)?;
        if span.state != SpanState::RemoteOnly {
            return None;
        }
        let claimed = *span;
        self.update_span_in_memory(start_ts, |span| {
            span.state = SpanState::Fetching;
        });
        Some(claimed)
    }

    /// Release a fetch claim after a failed download so a later request
    /// can retry.
    pub fn abort_fetch(&self, start_ts: Timestamp) {
        let _guard = self.structural.lock().unwrap();
        if self
            .manifest
            .load()
            .span(start_ts)
            .is_none_or(|span| span.state != SpanState::Fetching)
        {
            return;
        }
        self.update_span_in_memory(start_ts, |span| {
            span.state = SpanState::RemoteOnly;
        });
    }

    /// Record that a store holds a durable, verified copy of the span.
    /// Persisted — the ack is what licenses a later purge, so it must
    /// survive restarts.
    pub fn mark_acked(&self, start_ts: Timestamp) -> Result<(), Error> {
        let _guard = self.structural.lock().unwrap();
        if self.manifest.load().span(start_ts).is_none() {
            return Ok(());
        }
        self.publish_manifest(true, |spans| {
            for span in spans.iter_mut() {
                if span.seal.start_ts == start_ts {
                    span.acked = true;
                }
            }
        })
    }

    /// Drop a resident span's local bytes. Only acked, non-head spans
    /// qualify — the ack is the proof a store can give the data back, and
    /// the head belongs to the writer. Ordering is crash-safe: the
    /// manifest flips to remote-only durably first, then the node is
    /// unlinked and its directory removed; a crash in between resurrects
    /// the node as resident on reopen and it is simply purged again.
    /// Readers holding slices keep the unmapped bytes alive until they
    /// finish.
    pub fn purge_span(&self, start_ts: Timestamp) -> Result<bool, Error> {
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        let Some(span) = manifest.span(start_ts) else {
            return Ok(false);
        };
        if span.state != SpanState::Resident || !span.acked {
            return Ok(false);
        }
        let Some(node) = self
            .list
            .iter()
            .find(|n| n.timestamps().first() == Some(&start_ts))
        else {
            return Ok(false);
        };
        let node_is_head = self
            .list
            .head()
            .is_some_and(|head| Arc::ptr_eq(&head, &node));
        if node_is_head && self.has_writer.load(Ordering::Acquire) {
            return Ok(false);
        }

        self.publish_manifest(true, |spans| {
            for span in spans.iter_mut() {
                if span.seal.start_ts == start_ts {
                    span.state = SpanState::RemoteOnly;
                }
            }
        })?;

        self.list.unlink(&node);
        std::fs::remove_dir_all(self.path.join(start_ts.0.to_string()))?;
        std::fs::File::open(&self.path)?.sync_all()?;
        Ok(true)
    }

    /// Swap in a snapshot with `start_ts`'s span passed through `f`.
    /// In-memory only — transient states (`Fetching`) never persist.
    /// Caller holds the structural mutex.
    fn update_span_in_memory(&self, start_ts: Timestamp, f: impl Fn(&mut NodeSpan)) {
        self.publish_manifest(false, |spans| {
            for span in spans.iter_mut() {
                if span.seal.start_ts == start_ts {
                    f(span);
                }
            }
        })
        .expect("non-persisted manifest publish cannot fail");
    }

    /// Publish a manifest snapshot derived from the current spans by `f`,
    /// re-sorted by start so lookups can rely on time order. With
    /// `persist` the snapshot is durably on disk before it becomes
    /// visible, so a crash never reveals state the disk does not back.
    /// Caller holds the structural mutex.
    fn publish_manifest(
        &self,
        persist: bool,
        f: impl FnOnce(&mut Vec<NodeSpan>),
    ) -> Result<(), Error> {
        let mut spans: Vec<NodeSpan> = self.manifest.load().spans.to_vec();
        f(&mut spans);
        spans.sort_unstable_by_key(|s| s.seal.start_ts.0);
        let next = ComponentManifest {
            generation: 0,
            spans: spans.into_boxed_slice(),
        };
        if persist {
            next.write_to(&self.path)?;
        }
        self.manifest.store(next);
        Ok(())
    }

    /// Install a downloaded node: verify it against `seal`, promote the
    /// staging directory into place, splice the node into the list at its
    /// timestamp-ordered position, and publish the manifest span.
    /// Idempotent — re-installing an already-resident span with the same
    /// checksum is `Ok`.
    ///
    /// While a live writer exists the span must be strictly older than its
    /// head: hydration only ever fills gaps behind the live feed, so a
    /// sealed node that would outrank the head (e.g. pushed by a peer
    /// whose clock ran ahead) is rejected — installing it would break the
    /// newest-first node order and race the seal of the in-flight head.
    /// Without a writer (an archive store receiving pushed nodes) new
    /// spans legitimately land above the head.
    pub fn install_node(
        &self,
        staging: crate::store::NodeStaging,
        seal: &SealRecord,
        source: SpanSource,
        schema: &ComponentSchema,
    ) -> Result<(), Error> {
        use crate::store::StoreError;
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        if let Some(span) = manifest.span(seal.start_ts)
            && span.state == SpanState::Resident
        {
            return if span.seal.checksum == seal.checksum {
                Ok(())
            } else {
                Err(StoreError::ChecksumMismatch.into())
            };
        }
        // Reject overlap with any resident data; an installed node must
        // fill a hole, not shadow samples that already exist.
        let mut succ: Option<Arc<AtomicNode<TimeSeriesNode>>> = None;
        for node in self.list.iter() {
            let timestamps = node.timestamps();
            let (Some(start), Some(end)) = (timestamps.first(), timestamps.last()) else {
                continue;
            };
            if seal.start_ts.0 <= end.0 && start.0 <= seal.end_ts.0 {
                return Err(StoreError::Other("span overlaps resident data".to_string()).into());
            }
            if start.0 > seal.end_ts.0 {
                // Iteration is newest-first, so the last node we see that
                // is newer than the install is its direct successor.
                succ = Some(node);
            }
        }
        if succ.is_none()
            && self.list.head().is_some()
            && self.has_writer.load(Ordering::Acquire)
        {
            return Err(StoreError::Other("span is newer than the live head".to_string()).into());
        }
        let (node_dir, node) = staging.commit(seal)?;
        write_summary_sidecar(&node, schema, seal, &node_dir);
        match succ {
            Some(succ) => {
                if !self.list.insert_older(&succ, node) {
                    return Err(StoreError::Other("insert lost its anchor".to_string()).into());
                }
            }
            None => self.list.push(node),
        }

        self.publish_manifest(true, |spans| {
            spans.retain(|s| s.seal.start_ts != seal.start_ts);
            // Whatever store this came from holds a copy by definition.
            let acked = matches!(source, SpanSource::RemoteFetch);
            spans.push(NodeSpan::whole(*seal, SpanState::Resident, source, acked));
        })?;
        self.data_waker.wake_all();
        Ok(())
    }

    /// Merge a peer's sealed-node manifest into this component as
    /// remote-only coverage. This is how history recorded before a mirror
    /// connected becomes visible: each seal lands as a fetchable
    /// [`SpanState::RemoteOnly`] span without moving any payload bytes.
    /// One snapshot publish regardless of input size, so seeding a year
    /// of spans costs a single sort and fsync.
    ///
    /// A seal whose tail overlaps data this side already holds (the peer
    /// sealed across the mirror's connect time) is trimmed to the missing
    /// prefix rather than dropped. A seal whose start is already covered
    /// is skipped outright — trimming the front would change the span's
    /// identity (`seal.start_ts`). Idempotent: re-merging known seals
    /// adds nothing. Returns the number of spans added.
    pub fn merge_remote_spans(
        &self,
        seals: impl IntoIterator<Item = SealRecord>,
    ) -> Result<usize, Error> {
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        // Everything locally covered: manifest coverage plus resident
        // nodes (the unsealed head is in the list but not the manifest).
        let mut covered: Vec<(i64, i64)> = manifest
            .spans
            .iter()
            .map(|s| (s.seal.start_ts.0, s.cover_end.0))
            .collect();
        covered.extend(self.list.iter().filter_map(|node| {
            let ts = node.timestamps();
            Some((ts.first()?.0, ts.last()?.0))
        }));
        covered.sort_unstable();

        let mut candidates: Vec<SealRecord> = seals
            .into_iter()
            .filter(|seal| seal.count > 0 && seal.start_ts.0 <= seal.end_ts.0)
            .collect();
        candidates.sort_unstable_by_key(|seal| seal.start_ts.0);

        let mut added: Vec<NodeSpan> = Vec::new();
        for seal in candidates {
            if manifest.span(seal.start_ts).is_some() {
                continue;
            }
            let front_covered = |intervals: &[(i64, i64)]| {
                let idx = intervals.partition_point(|iv| iv.0 <= seal.start_ts.0);
                idx > 0 && intervals[idx - 1].1 >= seal.start_ts.0
            };
            if front_covered(&covered)
                || added
                    .last()
                    .is_some_and(|prev| prev.cover_end.0 >= seal.start_ts.0)
            {
                continue;
            }
            // Trim to just before the first covered interval inside the
            // seal's range, if any.
            let next = covered.partition_point(|iv| iv.0 <= seal.start_ts.0);
            let cover_end = match covered.get(next) {
                Some(iv) if iv.0 <= seal.end_ts.0 => Timestamp(iv.0 - 1),
                _ => seal.end_ts,
            };
            added.push(NodeSpan {
                cover_end,
                ..NodeSpan::whole(
                    seal,
                    SpanState::RemoteOnly,
                    SpanSource::RemoteFetch,
                    // The peer holds the copy by definition, so a later
                    // hydrated replica is a free cache eviction.
                    true,
                )
            });
        }
        if added.is_empty() {
            return Ok(0);
        }
        let count = added.len();
        self.publish_manifest(true, |spans| spans.extend(added))?;
        Ok(count)
    }

    /// Estimate how many samples fall inside `range`, counting sealed
    /// spans from the manifest and the live head from its index — no
    /// payload access. This is the cost signal plots use to decide
    /// whether a window is renderable raw or needs the envelope.
    pub fn estimate_samples(&self, range: Range<Timestamp>) -> u64 {
        let manifest = self.manifest.load();
        let mut total = manifest.count_in(&range);
        if let Some(head) = self.list.head() {
            let timestamps = head.timestamps();
            let unsealed = timestamps
                .first()
                .is_some_and(|start| manifest.span(*start).is_none());
            if unsealed {
                let lo = timestamps.partition_point(|t| t.0 < range.start.0);
                let hi = timestamps.partition_point(|t| t.0 < range.end.0);
                total += (hi - lo) as u64;
            }
        }
        total
    }

    pub fn start_timestamp(&self) -> Option<Timestamp> {
        self.list
            .iter()
            .filter_map(|node| node.timestamps().first().copied())
            .min()
    }

    /// Iterate every node as a [`TimeSeriesNodeSlice`] covering its full
    /// extent. Newest-first, matching [`Self::list`]'s iter; empty nodes
    /// are skipped. Use this when consumers want to walk the entire
    /// component (e.g., XY plot pairing) without a time-range cull.
    pub fn iter_node_slices(&self) -> impl Iterator<Item = TimeSeriesNodeSlice> + '_ {
        self.list.iter().filter_map(|node| {
            let len = node.timestamps().len();
            if len == 0 {
                return None;
            }
            Some(TimeSeriesNodeSlice {
                range: 0..=len - 1,
                node,
            })
        })
    }

    pub fn get(&self, timestamp: Timestamp) -> Option<TimestampRef> {
        for node in self.list.iter() {
            let timestamps = node.timestamps();
            let Ok(index) = timestamps.binary_search(&timestamp) else {
                continue;
            };
            let element_size = node.element_size();
            let i = index * element_size;
            if node.data.get(i..i + element_size).is_none() {
                continue;
            }
            return Some(TimestampRef {
                node,
                timestamp,
                index,
            });
        }
        None
    }

    pub fn binary_search_nearest(
        &self,
        timestamp: Timestamp,
        inclusive: bool,
    ) -> Option<TimestampRef> {
        // three different cases per node
        // 1. Timestamp > node.end
        //   a. prev_node.end.dist(timestamp) < node.start.dist(timestamp) then prev_node.end
        //   b. prev_node.end.dist(timestamp) > node.start.dist(timestamp) then node.start
        // 2. Timestamp < node.start
        //  set prev_node  and skip node
        // 3. Timestamp is within node.start..node.end
        //  node.timestamps.bst(timestamp)

        let mut prev_node: Option<TimestampRef> = None;
        for node in self.list.iter() {
            let timestamps = node.timestamps();
            let start = timestamps.first()?;
            let end = timestamps.last()?;
            if timestamp.0 > end.0 {
                if let Some(prev) = &prev_node
                    && prev.timestamp.0.abs_diff(timestamp.0) < start.0.abs_diff(timestamp.0)
                {
                    return prev_node;
                }
                return Some(TimestampRef {
                    timestamp: *start,
                    index: node.timestamps().len().saturating_sub(inclusive as usize),
                    node,
                });
            }
            if timestamp.0 < start.0 {
                prev_node = Some(TimestampRef {
                    timestamp: *end,
                    index: 0,
                    node,
                });
                continue;
            }
            if timestamp.0 >= start.0 && timestamp.0 <= end.0 {
                let index = match timestamps.binary_search(&timestamp) {
                    Ok(i) => i,
                    Err(i) => i.saturating_sub(inclusive as usize),
                };
                let timestamp = timestamps[index];
                return Some(TimestampRef {
                    timestamp,
                    node,
                    index,
                });
            }
        }

        prev_node
    }

    pub fn get_range(&self, range: Range<Timestamp>) -> Option<TimeSeriesSlice> {
        let start = self.binary_search_nearest(range.start, false)?;
        let end = self.binary_search_nearest(range.end, true)?;
        Some(TimeSeriesSlice { start, end })
    }

    pub async fn wait(&self) {
        let _ = self.data_waker.wait().await;
    }

    pub fn waiter(&self) -> Arc<WaitQueue> {
        self.data_waker.clone()
    }

    pub fn latest(&self) -> Option<TimestampRef> {
        let head = self.list.head()?;
        let index = head.timestamps().len().checked_sub(1)?;
        Some(TimestampRef {
            timestamp: head.timestamps()[index],
            node: head,
            index,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.list.head().is_none()
    }

    pub fn writer(&self) -> Option<TimerSeriesWriter> {
        if self.has_writer.swap(true, Ordering::Acquire) {
            None
        } else {
            Some(TimerSeriesWriter {
                time_series: self.clone(),
                current: None,
            })
        }
    }
}

pub struct TimerSeriesWriter {
    time_series: TimeSeries,
    /// The node this writer last appended to. When the head is a node we
    /// have not written (first write, reopen, a structural splice, or an
    /// installed node), we re-check that it is safe to append.
    current: Option<Arc<AtomicNode<TimeSeriesNode>>>,
}

impl TimerSeriesWriter {
    pub fn push_buf(&mut self, timestamp: Timestamp, buf: &[u8]) -> Result<(), Error> {
        let mut rolled = false;
        loop {
            if self.try_push_buf(timestamp, buf)? {
                break;
            }
            let _ = self.time_series.list.try_push(TimeSeriesNode::create(
                self.time_series.path.join(timestamp.0.to_string()),
                timestamp,
                buf.len() as u64,
            )?);
            rolled = true;
        }
        if rolled {
            // The previous head is now immutable; let the lifecycle task
            // seal it off the hot path.
            self.time_series.lifecycle_waker.wake_all();
        }
        self.time_series.data_waker.wake_all();
        Ok(())
    }

    fn try_push_buf(&mut self, timestamp: Timestamp, buf: &[u8]) -> Result<bool, Error> {
        let Some(head) = self.time_series.list.head() else {
            return Ok(false);
        };
        let head_is_current = self
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &head));
        if !head_is_current && self.head_is_sealed(&head) {
            // Never append into a sealed node (e.g. one installed from a
            // store); roll a fresh node above it instead.
            return Ok(false);
        }
        match head.data.write(buf) {
            Ok(_) => {}
            Err(Error::MapOverflow) => return Ok(false),
            Err(err) => {
                println!("{err:?}");
                return Err(err);
            }
        };
        head.index.write(&timestamp.to_le_bytes())?;
        self.current = Some(head);
        Ok(true)
    }

    fn head_is_sealed(&self, head: &Arc<AtomicNode<TimeSeriesNode>>) -> bool {
        let Some(start_ts) = head.timestamps().first().copied() else {
            return false;
        };
        self.time_series
            .manifest
            .load()
            .span(start_ts)
            .is_some_and(|span| span.state == SpanState::Resident)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_schema() -> ComponentSchema {
        ComponentSchema::new(metor_proto::types::PrimType::I64, &[1])
    }

    fn remote_seal(start: i64, end: i64) -> SealRecord {
        SealRecord {
            start_ts: Timestamp(start),
            end_ts: Timestamp(end),
            count: (end - start + 1) as u64,
            index_len: 8,
            data_len: 8,
            checksum: 1,
            element_size: 8,
        }
    }

    #[test]
    fn merge_remote_spans_batches_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();

        let seals = [
            remote_seal(0, 99),
            remote_seal(100, 199),
            remote_seal(200, 299),
        ];
        let before = series.manifest().generation;
        assert_eq!(series.merge_remote_spans(seals).unwrap(), 3);
        let manifest = series.manifest();
        // The whole batch lands in one snapshot publish.
        assert_eq!(manifest.generation, before + 1);
        assert_eq!(manifest.spans.len(), 3);
        for span in &manifest.spans {
            assert_eq!(span.state, SpanState::RemoteOnly);
            assert_eq!(span.source, SpanSource::RemoteFetch);
            assert!(span.acked);
        }

        assert_eq!(series.merge_remote_spans(seals).unwrap(), 0);
        assert_eq!(series.manifest().generation, before + 1);
    }

    #[test]
    fn merge_remote_spans_trims_against_resident_data() {
        let dir = tempfile::tempdir().unwrap();
        let node =
            TimeSeriesNode::create(dir.path().join("100"), Timestamp(100), 8).unwrap();
        for ts in [100i64, 199] {
            node.data.write(&ts.to_le_bytes()).unwrap();
            node.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        let series = TimeSeries::open(dir.path()).unwrap();

        // Tail overlap trims; front overlap and full coverage skip.
        let added = series
            .merge_remote_spans([
                remote_seal(0, 150),
                remote_seal(120, 250),
                remote_seal(110, 180),
            ])
            .unwrap();
        assert_eq!(added, 1);
        let manifest = series.manifest();
        assert_eq!(manifest.spans.len(), 1);
        assert_eq!(manifest.spans[0].seal.start_ts, Timestamp(0));
        assert_eq!(manifest.spans[0].cover_end, Timestamp(99));

        // The trimmed span's gap stops at the resident boundary.
        let mut gaps = GapVec::new();
        manifest.gaps(&(Timestamp(0)..Timestamp(300)), &mut gaps);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].range, Timestamp(0)..Timestamp(100));
    }

    #[test]
    fn trimmed_span_installs_only_uncovered_prefix() {
        use crate::store::{NodeFile, NodeStaging};

        // Local resident data at [100, 101].
        let dir = tempfile::tempdir().unwrap();
        let resident =
            TimeSeriesNode::create(dir.path().join("100"), Timestamp(100), 8).unwrap();
        for ts in [100i64, 101] {
            resident.data.write(&ts.to_le_bytes()).unwrap();
            resident.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        let series = TimeSeries::open(dir.path()).unwrap();

        // The peer's node spans [0, 140] and overlaps the resident data.
        let src_dir = tempfile::tempdir().unwrap();
        let src = TimeSeriesNode::create(src_dir.path().join("0"), Timestamp(0), 8).unwrap();
        for i in 0..15i64 {
            let ts = i * 10;
            src.data.write(&ts.to_le_bytes()).unwrap();
            src.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        let remote_seal = SealRecord::compute(&src).unwrap();
        assert_eq!(series.merge_remote_spans([remote_seal]).unwrap(), 1);

        let span = series.begin_fetch(Timestamp(0)).unwrap();
        assert_eq!(span.cover_end, Timestamp(99));

        // Stage the full node as a fetch would, then trim and install.
        let staging = NodeStaging::create(dir.path(), &span.seal).unwrap();
        staging
            .append_file(NodeFile::Index, src.index.data())
            .unwrap();
        staging.append_file(NodeFile::Data, src.data.data()).unwrap();
        let local_seal = staging.trim_to(span.cover_end).unwrap();
        assert_eq!(local_seal.end_ts, Timestamp(90));
        series
            .install_node(staging, &local_seal, SpanSource::RemoteFetch, &test_schema())
            .unwrap();

        let manifest = series.manifest();
        let installed = manifest.span(Timestamp(0)).unwrap();
        assert_eq!(installed.state, SpanState::Resident);
        assert_eq!(installed.seal.end_ts, Timestamp(90));
        assert_eq!(installed.cover_end, Timestamp(90));
        assert_eq!(series.start_timestamp().unwrap(), Timestamp(0));
        assert_eq!(series.latest().unwrap().timestamp(), Timestamp(101));
        assert!(
            dir.path()
                .join("0")
                .join(crate::summary::SUMMARY_FILE)
                .exists()
        );
        let mut gaps = GapVec::new();
        assert_eq!(
            series.coverage(Timestamp(0)..Timestamp(102), &mut gaps),
            Coverage::Complete
        );
    }

    #[test]
    fn estimate_samples_counts_manifest_and_live_head() {
        let dir = tempfile::tempdir().unwrap();
        let node = TimeSeriesNode::create(dir.path().join("100"), Timestamp(100), 8).unwrap();
        for ts in [100i64, 110, 120, 130] {
            node.data.write(&ts.to_le_bytes()).unwrap();
            node.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        let series = TimeSeries::open(dir.path()).unwrap();
        // 100 sealed samples of remote-only coverage behind the head.
        series.merge_remote_spans([remote_seal(0, 99)]).unwrap();

        // The unsealed head contributes its in-range samples exactly.
        assert_eq!(series.estimate_samples(Timestamp(100)..Timestamp(131)), 4);
        assert_eq!(series.estimate_samples(Timestamp(115)..Timestamp(131)), 2);
        // Remote coverage prorates from seal counts.
        assert_eq!(series.estimate_samples(Timestamp(0)..Timestamp(100)), 100);
        assert_eq!(series.estimate_samples(Timestamp(0)..Timestamp(131)), 104);
    }

    #[test]
    fn merged_spans_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let series = TimeSeries::create(dir.path()).unwrap();
            series
                .merge_remote_spans([remote_seal(0, 99)])
                .unwrap();
        }
        let series = TimeSeries::open(dir.path()).unwrap();
        let manifest = series.manifest();
        assert_eq!(manifest.spans.len(), 1);
        assert_eq!(manifest.spans[0].state, SpanState::RemoteOnly);
        assert!(manifest.spans[0].acked);
    }

    #[test]
    fn open_restores_newest_first_order() {
        let dir = tempfile::tempdir().unwrap();
        // Starts chosen so lexicographic directory order (100 < 20 < 3 < 9)
        // disagrees with numeric order, catching any reliance on read_dir
        // ordering.
        let starts = [9i64, 100, 20, 3];
        for start in starts {
            let node =
                TimeSeriesNode::create(dir.path().join(start.to_string()), Timestamp(start), 8)
                    .unwrap();
            node.data.write(&start.to_le_bytes()).unwrap();
            node.index.write(&Timestamp(start).to_le_bytes()).unwrap();
        }

        let series = TimeSeries::open(dir.path()).unwrap();
        let starts_newest_first: Vec<i64> = series
            .list
            .iter()
            .map(|node| node.timestamps()[0].0)
            .collect();
        assert_eq!(starts_newest_first, vec![100, 20, 9, 3]);
        assert_eq!(series.latest().unwrap().timestamp().0, 100);
    }

    #[test]
    fn seal_rolled_nodes_skips_head_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        for start in [10i64, 20, 30] {
            let node =
                TimeSeriesNode::create(dir.path().join(start.to_string()), Timestamp(start), 8)
                    .unwrap();
            node.data.write(&start.to_le_bytes()).unwrap();
            node.index.write(&Timestamp(start).to_le_bytes()).unwrap();
        }

        let series = TimeSeries::open(dir.path()).unwrap();
        assert!(series.manifest().spans.is_empty());
        assert!(series.seal_rolled_nodes(&test_schema()).unwrap());

        let manifest = series.manifest();
        let starts: Vec<i64> = manifest.spans.iter().map(|s| s.seal.start_ts.0).collect();
        // 30 is the live head and stays unsealed.
        assert_eq!(starts, vec![10, 20]);
        assert!(
            manifest
                .spans
                .iter()
                .all(|s| s.state == SpanState::Resident)
        );
        // Sealing also writes the summary sidecar each span derives from.
        for start in [10i64, 20] {
            assert!(
                dir.path()
                    .join(start.to_string())
                    .join(crate::summary::SUMMARY_FILE)
                    .exists()
            );
        }
        // Idempotent on a second pass.
        assert!(!series.seal_rolled_nodes(&test_schema()).unwrap());

        // A reopen rebuilds the same view from seal files + manifest.
        drop(series);
        let series = TimeSeries::open(dir.path()).unwrap();
        let manifest = series.manifest();
        let starts: Vec<i64> = manifest.spans.iter().map(|s| s.seal.start_ts.0).collect();
        assert_eq!(starts, vec![10, 20]);
    }

    #[test]
    fn purged_span_survives_reopen_as_remote_only() {
        let dir = tempfile::tempdir().unwrap();
        for start in [10i64, 20] {
            let node =
                TimeSeriesNode::create(dir.path().join(start.to_string()), Timestamp(start), 8)
                    .unwrap();
            node.data.write(&start.to_le_bytes()).unwrap();
            node.index.write(&Timestamp(start).to_le_bytes()).unwrap();
        }
        let series = TimeSeries::open(dir.path()).unwrap();
        series.seal_rolled_nodes(&test_schema()).unwrap();
        drop(series);

        // Simulate a purge: the node dir is gone but the manifest remembers.
        std::fs::remove_dir_all(dir.path().join("10")).unwrap();
        let series = TimeSeries::open(dir.path()).unwrap();
        let manifest = series.manifest();
        assert_eq!(manifest.spans.len(), 1);
        assert_eq!(manifest.spans[0].seal.start_ts, Timestamp(10));
        assert_eq!(manifest.spans[0].state, SpanState::RemoteOnly);

        let mut gaps = GapVec::new();
        let coverage = series.coverage(Timestamp(0)..Timestamp(100), &mut gaps);
        assert_eq!(coverage, Coverage::Partial);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].range, Timestamp(10)..Timestamp(11));
    }

    #[test]
    fn install_node_rejects_span_newer_than_head() {
        use crate::store::{NodeFile, NodeStaging, SealedChunk};

        let dir = tempfile::tempdir().unwrap();
        let head = TimeSeriesNode::create(dir.path().join("10"), Timestamp(10), 8).unwrap();
        head.data.write(&10i64.to_le_bytes()).unwrap();
        head.index.write(&Timestamp(10).to_le_bytes()).unwrap();
        let series = TimeSeries::open(dir.path()).unwrap();
        // The guard protects the head of a live feed; without a writer the
        // install would be a legitimate archive append.
        let _writer = series.writer().unwrap();

        // A sealed node newer than the head, as a peer push would stage it.
        let src_dir = tempfile::tempdir().unwrap();
        let src = TimeSeriesNode::create(src_dir.path().join("20"), Timestamp(20), 8).unwrap();
        src.data.write(&20i64.to_le_bytes()).unwrap();
        src.index.write(&Timestamp(20).to_le_bytes()).unwrap();
        let seal = SealRecord::compute(&src).unwrap();

        let staging = NodeStaging::create(dir.path(), &seal).unwrap();
        staging
            .append(&SealedChunk {
                file: NodeFile::Index,
                offset: 0,
                bytes: src.index.data(),
            })
            .unwrap();
        staging
            .append(&SealedChunk {
                file: NodeFile::Data,
                offset: 0,
                bytes: src.data.data(),
            })
            .unwrap();
        assert!(
            series
                .install_node(staging, &seal, SpanSource::LocalIngest, &test_schema())
                .is_err()
        );
        // The head is untouched and the staging directory cleaned up.
        assert_eq!(series.latest().unwrap().timestamp(), Timestamp(10));
        assert!(!dir.path().join("20").exists());
        assert!(series.manifest().span(Timestamp(20)).is_none());
    }

    #[test]
    fn coverage_empty_when_only_remote_data_overlaps() {
        let dir = tempfile::tempdir().unwrap();
        for start in [10i64, 200] {
            let node =
                TimeSeriesNode::create(dir.path().join(start.to_string()), Timestamp(start), 8)
                    .unwrap();
            node.data.write(&start.to_le_bytes()).unwrap();
            node.index.write(&Timestamp(start).to_le_bytes()).unwrap();
        }
        let series = TimeSeries::open(dir.path()).unwrap();
        series.seal_rolled_nodes(&test_schema()).unwrap();
        drop(series);
        std::fs::remove_dir_all(dir.path().join("10")).unwrap();
        // Drop the resident node too so only remote data overlaps the
        // queried range.
        std::fs::remove_dir_all(dir.path().join("200")).unwrap();

        let series = TimeSeries::open(dir.path()).unwrap();
        let mut gaps = GapVec::new();
        assert_eq!(
            series.coverage(Timestamp(0)..Timestamp(50), &mut gaps),
            Coverage::Empty
        );

        gaps.clear();
        assert_eq!(
            series.coverage(Timestamp(500)..Timestamp(600), &mut gaps),
            Coverage::Complete
        );
    }
}
