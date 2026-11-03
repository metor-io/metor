mod coverage;
#[cfg(test)]
mod coverage_tests;

use std::{
    fmt::Debug,
    num::NonZeroU64,
    ops::{Range, RangeInclusive},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use metor_proto::types::{ComponentView, Timestamp};
use metor_proto_wkt::UnsealedHead;
use smallvec::SmallVec;

use crate::ComponentSchema;
use stellarator::sync::WaitQueue;
use tracing::warn;
use zerocopy::FromBytes;

use crate::{
    Error,
    append_log::AppendLog,
    arc_ring::{AtomicNode, AtomicStack, AtomicStackIter},
    disruptor::ArcAtomic,
    evict::LastAccess,
    manifest::{
        ComponentManifest, Coverage, Fragment, GapVec, ManifestCell, NodeSpan, RangeVec,
        SpanSource, SpanState,
    },
    seal::{SealRecord, SealRecordExt, seal_node},
};

use coverage::{Draft, Extent, RemoteSeal, extents, remote_holds};

#[derive(Clone)]
pub struct TimeSeries {
    pub list: Arc<AtomicStack<TimeSeriesNode>>,
    path: Arc<Path>,
    data_waker: Arc<WaitQueue>,
    has_writer: Arc<AtomicBool>,
    manifest: Arc<ManifestCell>,
    structural: Arc<Mutex<()>>,
    lifecycle_waker: Arc<WaitQueue>,
    seal_waker: Arc<WaitQueue>,
    /// The longest data-time span the head node covers, in microseconds,
    /// before a write rolls it. Zero means unbounded.
    max_node_age: Arc<AtomicI64>,
    /// The [`MirrorRun`] that last wrote here, or zero if no mirror ever
    /// has. Any run at all tags the nodes the writer creates as
    /// [`SpanSource::Mirrored`].
    mirror_run: Arc<AtomicU64>,
    /// A new mirror run began; the writer starts a fresh node on its next
    /// sample, so no node spans the time a mirror was disconnected.
    roll_pending: Arc<AtomicBool>,
    /// The first sample the latest mirror run wrote, or `i64::MIN` before
    /// any has. A peer's listed head reaches up to it, so the backfill a
    /// head offers meets the live stream (see [`Self::head_seal`]).
    run_start: Arc<AtomicI64>,
    /// The head the peer's last complete listing reported, kept to be cut
    /// again when a mirror run starts. Changed under `structural`.
    peer_head: Arc<Mutex<Option<UnsealedHead>>>,
}

/// One mirror connection's lifetime.
///
/// The mirror's live ingest enters its run on every component it writes,
/// which marks that component's nodes as [`SpanSource::Mirrored`] and rolls
/// the head when the run changes. A node therefore never spans a
/// disconnect, so the peer's data for the time the mirror missed always
/// lands in a hole between nodes, where hydration can install it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MirrorRun(NonZeroU64);

impl MirrorRun {
    /// A run distinct from every other in this process.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NonZeroU64::new(NEXT.fetch_add(1, Ordering::Relaxed)).expect("run ids start at 1"))
    }
}

#[derive(Clone)]
pub struct TimeSeriesNode {
    pub index: AppendLog<Timestamp>,
    pub data: AppendLog<u64>,
    /// When a reader last asked for this node's data; shared by the
    /// shells a splice rebuilds, so a touch lands whichever one a reader
    /// holds. See [`TimeSeries::touch`].
    pub(crate) last_access: Arc<LastAccess>,
    /// The node holds a range-fetch [`Fragment`]'s samples: sealed, never
    /// appended to, and claiming no time of its own in coverage.
    pub(crate) fragment: bool,
}

impl Debug for TimeSeriesNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimeSeriesNode").finish()
    }
}

/// Capacity of each node's index and data files. Sealed nodes are sparse
/// up to this size; only the committed prefix ever travels over a store.
pub const NODE_SIZE: u64 = 1024 * 1024 * 32;
/// Samples a node's data file holds at least, whatever the element size.
const NODE_MIN_ELEMENTS: u64 = 64;
/// Room for the append log's header.
const NODE_HEADER_SLACK: u64 = 64;

/// Size of a new node's data file: `NODE_SIZE`, or enough for
/// `NODE_MIN_ELEMENTS` samples of `element_size` bytes.
pub fn node_data_size(element_size: u64) -> u64 {
    NODE_SIZE.max(
        element_size
            .saturating_mul(NODE_MIN_ELEMENTS)
            .saturating_add(NODE_HEADER_SLACK),
    )
}

impl TimeSeriesNode {
    pub fn create(
        path: impl AsRef<Path>,
        start_timestamp: Timestamp,
        element_size: u64,
    ) -> Result<Self, Error> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let index = AppendLog::with_size(NODE_SIZE, path.join("index"), start_timestamp)?;
        let data = AppendLog::with_size(
            node_data_size(element_size),
            path.join("data"),
            element_size,
        )?;
        Ok(Self::from_logs(index, data))
    }

    pub(crate) fn from_logs(index: AppendLog<Timestamp>, data: AppendLog<u64>) -> Self {
        Self {
            index,
            data,
            last_access: Arc::new(LastAccess::now()),
            fragment: false,
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let index = AppendLog::open(path.join("index"))?;
        let data = AppendLog::open(path.join("data"))?;
        Ok(Self::from_logs(index, data))
    }

    pub fn timestamps(&self) -> &[Timestamp] {
        <[Timestamp]>::ref_from_bytes(self.index.get(..).expect("couldn't get full range"))
            .expect("mmep unaligned")
    }

    pub fn element_size(&self) -> usize {
        *self.data.extra() as usize
    }
}

/// Fragments an install replaces: their claims leave the manifest and
/// their nodes the list, together with what replaces them.
#[derive(Default)]
struct Superseded {
    claims: SmallVec<[Timestamp; 4]>,
    dirs: SmallVec<[Arc<Path>; 4]>,
    nodes: Vec<Arc<AtomicNode<TimeSeriesNode>>>,
}

impl Superseded {
    fn forget(&self, draft: &mut Draft) {
        if !self.claims.is_empty() {
            draft.fragments.retain(|f| !self.claims.contains(&f.start));
        }
    }

    /// Delete the replaced nodes' files, once nothing links them. Readers
    /// still holding slices keep the unlinked mappings alive.
    fn delete(&self) {
        for dir in &self.dirs {
            if let Err(err) = std::fs::remove_dir_all(dir) {
                warn!(?dir, ?err, "failed to delete a replaced fragment");
            }
        }
    }
}

/// `start..=end` in stretches that never cross a resident node: a node
/// inside it splits it, the stretch before the node ending on the node's
/// first timestamp and the one after starting on its last.
fn around_nodes(start: i64, end: i64, resident: &[Extent]) -> SmallVec<[(i64, i64); 2]> {
    let mut out = SmallVec::new();
    let mut cursor = start;
    let first = resident.partition_point(|e| e.end < start);
    for e in &resident[first..] {
        if e.start > end {
            break;
        }
        if cursor <= e.start {
            out.push((cursor, e.start));
        }
        let after = match e.end > e.start {
            true => e.end,
            false => e.end.saturating_add(1),
        };
        cursor = cursor.max(after);
        if cursor > end {
            return out;
        }
    }
    out.push((cursor, end));
    out
}

/// Whether `node` is closed to appends: a sealed span, or a fragment. The
/// one node that is neither is the head a writer appends to.
fn is_sealed(manifest: &ComponentManifest, node: &TimeSeriesNode) -> bool {
    node.fragment
        || node.timestamps().first().is_some_and(|start| {
            manifest
                .span(*start)
                .is_some_and(|span| span.state == SpanState::Resident)
        })
}

/// Marks a node directory as holding [`SpanSource::Mirrored`] data. Written
/// when the node is created (by a mirror run's writer, or as a hydration
/// piece), so the tag survives a crash or a restart that seals the node
/// before the manifest ever records it.
const MIRRORED_TAG: &str = "mirrored";

fn tag_mirrored(node_dir: &Path) -> Result<(), Error> {
    std::fs::File::create(node_dir.join(MIRRORED_TAG))?.sync_all()?;
    Ok(())
}

fn is_tagged_mirrored(node_dir: &Path) -> bool {
    node_dir.join(MIRRORED_TAG).exists()
}

/// Rebuild the manifest on open: the directory scan (already loaded into
/// `list`) is authoritative for residency; the persisted manifest
/// contributes remote coverage, shadowed seals, settled stretches, and
/// flags (`acked`, `source`) for spans that are still resident. Nodes
/// without a seal record stay out of the manifest until the lifecycle task
/// seals them. Corrupt sidecars degrade the same way — a bad manifest
/// behaves as absent, a bad seal leaves its node unsealed — so one torn
/// file never blocks opening the component. Remote coverage is derived
/// afresh, since what was resident when it was persisted may not be now.
fn reconcile_manifest(path: &Path, list: &AtomicStack<TimeSeriesNode>) -> ComponentManifest {
    let persisted = match ComponentManifest::read_from(path) {
        Ok(persisted) => persisted.unwrap_or_default(),
        Err(e) => {
            warn!(?path, ?e, "failed to read component manifest");
            ComponentManifest::default()
        }
    };
    let mut draft = Draft {
        spans: Vec::new(),
        shadowed: persisted.shadowed.to_vec(),
        settled: persisted.settled.to_vec(),
        fragments: Vec::new(),
    };
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
        // Older writes could persist a stale remote entry under a resident
        // node's key; flags only ever come from a resident entry.
        let lo = persisted
            .spans
            .partition_point(|s| s.cover_start.0 < start_ts.0);
        let flags = persisted.spans[lo..]
            .iter()
            .take_while(|s| s.cover_start == start_ts)
            .find(|s| s.state == SpanState::Resident);
        let source = if is_tagged_mirrored(&node_dir) {
            SpanSource::Mirrored
        } else {
            flags.map(|s| s.source).unwrap_or(SpanSource::LocalIngest)
        };
        draft.spans.push(NodeSpan::whole(
            seal,
            SpanState::Resident,
            source,
            flags.is_some_and(|s| s.acked),
        ));
    }
    let resident = draft.spans.len();
    for span in &persisted.spans {
        if draft.spans[..resident]
            .iter()
            .any(|s| s.cover_start == span.cover_start)
        {
            continue;
        }
        if span.state == SpanState::Resident {
            // Its data is gone, so nothing is reconciled over it any more.
            draft.unsettle(span.seal.start_ts.0, span.seal.end_ts.0);
        }
        match (span.state, span.source) {
            // Mirrored data only ever had the peer's seals behind it, which
            // the derivation below hands its range back to.
            (SpanState::Resident, SpanSource::Mirrored) => {}
            // A purged (or never fetched) span; whatever state was
            // persisted, locally it is remote-only now.
            (SpanState::Resident, _) => draft.spans.push(NodeSpan {
                state: SpanState::RemoteOnly,
                ..NodeSpan::whole(span.seal, span.state, span.source, span.acked)
            }),
            _ => draft.spans.push(*span),
        }
    }
    draft.derive(&extents(list.iter(), None), Some(i64::MIN..=i64::MAX), []);
    draft.into_manifest()
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

/// Delete staging directories left behind by fetches that never
/// committed, and the nodes of range-fetch fragments, which are a cache
/// that lasts one run. Runs at open, before any fetch task exists, so every
/// `.fetching` directory is guaranteed orphaned.
fn remove_stale_staging_dirs(path: &Path) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.is_dir()
            && dir
                .extension()
                .is_some_and(|ext| ext == "fetching" || ext == "fragment")
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
    /// Per-node slices of the range, newest first. The node list's links
    /// only run newer → older, so iteration must seed from the range's
    /// newest node (`end`) and stop once the oldest (`start`) has been
    /// yielded — seeding from `start` can never reach the newer nodes
    /// that hold the rest of the range.
    pub fn as_iter(&self) -> impl Iterator<Item = TimeSeriesNodeSlice> + '_ {
        let iter: AtomicStackIter<TimeSeriesNode> =
            AtomicStackIter::new(ArcAtomic::from(self.end.node.clone()));
        // Floor the walk at `start`'s oldest sample: should `start` and
        // `end` ever land on different spines (the `ptr_eq` sentinel
        // below would then never match), this caps the over-read at one
        // node instead of letting iteration run to the oldest node in the
        // archive.
        let start_floor = self.start.node.timestamps().first().map(|t| t.0);
        let mut done = false;
        iter.map_while(move |node| {
            if done {
                return None;
            }
            if let (Some(floor), Some(newest)) = (start_floor, node.timestamps().last())
                && newest.0 < floor
            {
                return None;
            }
            let is_start = Arc::ptr_eq(&node, &self.start.node);
            done = is_start;
            let start = if is_start { self.start.index } else { 0 };
            let end = if Arc::ptr_eq(&node, &self.end.node) {
                self.end.index
            } else {
                node.timestamps().len().saturating_sub(1)
            };
            Some(TimeSeriesNodeSlice {
                range: start..=end,
                node,
            })
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
            path: Arc::from(path.as_ref()),
            data_waker: Arc::new(WaitQueue::new()),
            has_writer: Arc::new(AtomicBool::new(false)),
            manifest: Arc::new(ManifestCell::default()),
            structural: Arc::new(Mutex::new(())),
            lifecycle_waker: Arc::new(WaitQueue::new()),
            seal_waker: Arc::new(WaitQueue::new()),
            max_node_age: Arc::new(AtomicI64::new(0)),
            mirror_run: Arc::new(AtomicU64::new(0)),
            roll_pending: Arc::new(AtomicBool::new(false)),
            run_start: Arc::new(AtomicI64::new(i64::MIN)),
            peer_head: Arc::new(Mutex::new(None)),
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
            path: Arc::from(path),
            data_waker: Arc::new(WaitQueue::new()),
            has_writer: Arc::new(AtomicBool::new(false)),
            manifest: Arc::new(ManifestCell::new(manifest)),
            structural: Arc::new(Mutex::new(())),
            lifecycle_waker: Arc::new(WaitQueue::new()),
            seal_waker: Arc::new(WaitQueue::new()),
            max_node_age: Arc::new(AtomicI64::new(0)),
            mirror_run: Arc::new(AtomicU64::new(0)),
            roll_pending: Arc::new(AtomicBool::new(false)),
            run_start: Arc::new(AtomicI64::new(i64::MIN)),
            peer_head: Arc::new(Mutex::new(None)),
        })
    }

    /// Pin a coherent local node set and its coverage metadata. Ingestion
    /// does not take the structural lock and continues while these are copied.
    /// Fragments are left out: a snapshot holds what this side keeps.
    pub(crate) fn snapshot_nodes(
        &self,
    ) -> (Arc<ComponentManifest>, Vec<Arc<AtomicNode<TimeSeriesNode>>>) {
        let _guard = self.structural.lock().unwrap();
        (
            self.manifest.load(),
            self.list.iter().filter(|node| !node.fragment).collect(),
        )
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

    /// Append the stretches of `range` that no one has ever produced data
    /// for, in time order. Never blocks and never allocates beyond `out`'s
    /// spill; safe to call from a render loop.
    ///
    /// The complement to [`Self::coverage`]: a gap there is history that
    /// exists but is not *here* (a remote-only span a fetch can hydrate),
    /// while an uncovered stretch is answered by nothing at all — no
    /// resident node, no seal in any state. That makes this the question
    /// a backfill asks ("what must I compute?") where hydration asks
    /// [`Self::coverage`] ("what must I fetch?").
    pub fn uncovered(&self, range: Range<Timestamp>, out: &mut RangeVec) {
        let manifest = self.manifest.load();
        // Inclusive `(start, end)` pairs; residency and state are
        // irrelevant here, only whether anything claims the time. A seal
        // claims its whole range even where local data shadows it.
        let mut claimed: SmallVec<[(i64, i64); 8]> = manifest
            .spans
            .iter()
            .map(|span| &span.seal)
            .chain(manifest.shadowed.iter())
            .map(|seal| (seal.start_ts.0, seal.end_ts.0))
            .collect();
        claimed.extend(self.list.iter().filter_map(|node| {
            let timestamps = node.timestamps();
            Some((timestamps.first()?.0, timestamps.last()?.0))
        }));
        claimed.sort_unstable();

        let mut cursor = range.start.0;
        for (start, end) in claimed {
            if end < cursor {
                continue;
            }
            if start >= range.end.0 {
                break;
            }
            if start > cursor {
                out.push(Timestamp(cursor)..Timestamp(start));
            }
            cursor = end.saturating_add(1);
            if cursor >= range.end.0 {
                return;
            }
        }
        if cursor < range.end.0 {
            out.push(Timestamp(cursor)..range.end);
        }
    }

    /// Record that a reader used `range` at `now`: every resident node
    /// overlapping it, and the newest node starting at or before its start,
    /// which holds the sample a reader carries into the range (a plot's
    /// line in from the left edge, the predecessor a value view shows).
    ///
    /// This is the working set [`crate::evict`] keeps. Only reads made for
    /// someone looking at the data touch — views, playback read-ahead —
    /// never internal scans (coverage, LoD folding, sealing, the evictor
    /// itself), or nothing would ever go idle. In memory only, never
    /// blocks or allocates, and walks only the nodes it touches plus the
    /// newer ones above them; cheap enough per trace per frame.
    pub fn touch(&self, range: Range<Timestamp>, now: Instant) {
        let stamp = LastAccess::stamp(now);
        let end = range.end.0.max(range.start.0.saturating_add(1));
        for node in self.list.iter() {
            let Some(first) = node.timestamps().first() else {
                continue;
            };
            if first.0 >= end {
                continue;
            }
            node.last_access.touch(stamp);
            if first.0 <= range.start.0 {
                break;
            }
        }
        // A fragment that held no samples has no node to touch.
        let manifest = self.manifest.load();
        for fragment in manifest.fragments_in(range.start.0, end) {
            fragment.access.touch(stamp);
        }
    }

    /// Mark this component as written by `run`'s mirror. Called by the
    /// mirror's ingest before each sample it pushes, so it costs one load
    /// when the run is unchanged.
    pub fn enter_mirror_run(&self, run: MirrorRun) {
        let run = run.0.get();
        if self.mirror_run.load(Ordering::Relaxed) != run
            && self.mirror_run.swap(run, Ordering::AcqRel) != run
        {
            self.roll_pending.store(true, Ordering::Release);
        }
    }

    /// Whether a mirror run has written here.
    #[cfg(test)]
    pub(crate) fn is_mirrored(&self) -> bool {
        self.mirror_run.load(Ordering::Acquire) != 0
    }

    /// Seal every resident node the writer has rolled past, summarize it,
    /// and record it in the manifest. Returns true when anything changed.
    /// Runs on the lifecycle task; checksums and fsyncs
    /// make it unsuitable for hot paths.
    pub fn seal_rolled_nodes(&self) -> Result<bool, Error> {
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        let mut sealed: Vec<NodeSpan> = Vec::new();
        // Newest-first; the first node is the live head and must not be
        // sealed — the writer may still be appending to it.
        for node in self.list.iter().skip(1) {
            if node.fragment {
                continue;
            }
            let Some(start_ts) = node.timestamps().first().copied() else {
                continue;
            };
            if manifest
                .span(start_ts)
                .is_some_and(|s| s.state == SpanState::Resident)
            {
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
            let source = match is_tagged_mirrored(&node_dir) {
                true => SpanSource::Mirrored,
                false => SpanSource::LocalIngest,
            };
            sealed.push(NodeSpan::whole(seal, SpanState::Resident, source, false));
        }
        let Some(touched) = sealed
            .iter()
            .map(|s| s.seal.start_ts.0..=s.seal.end_ts.0)
            .reduce(|a, b| *a.start().min(b.start())..=*a.end().max(b.end()))
        else {
            return Ok(false);
        };
        let resident = extents(self.list.iter(), None);
        self.publish_manifest(true, |draft| {
            draft.spans.extend(sealed);
            draft.derive(&resident, Some(touched), []);
        })?;
        self.seal_waker.wake_all();
        Ok(true)
    }

    /// Woken whenever a sealed span appears — [`Self::seal_rolled_nodes`]
    /// sealing what the writer rolled past, or [`Self::install_node`]
    /// splicing one in. Either way new history has become final and
    /// derivations over sealed data can advance.
    pub fn seal_waiter(&self) -> Arc<WaitQueue> {
        self.seal_waker.clone()
    }

    /// Bound the head node's data-time age; the writer rolls (and so the
    /// lifecycle task seals) once a write lands more than `age` past the
    /// head's first sample. Used for slow-writing derived components whose
    /// 32 MB head would otherwise never fill — sealed spans are the only
    /// thing manifest seeding ships to mirrors.
    pub fn set_max_node_age(&self, age: Duration) {
        self.max_node_age
            .store(age.as_micros() as i64, Ordering::Relaxed);
    }

    fn max_node_age_us(&self) -> Option<i64> {
        match self.max_node_age.load(Ordering::Relaxed) {
            0 => None,
            age => Some(age),
        }
    }

    /// Background task that seals nodes as the writer rolls past them, and
    /// carries a peer's head up to each mirror run that starts.
    /// Spawn one per component alongside the persist task.
    pub async fn lifecycle(self) {
        let mut run_start = i64::MIN;
        loop {
            if let Err(err) = self.seal_rolled_nodes() {
                warn!(?err, path = ?self.path, "failed to seal rolled nodes");
            }
            let started = self.run_start.load(Ordering::Acquire);
            if started != run_start {
                run_start = started;
                if let Err(err) = self.meet_mirror_run() {
                    warn!(?err, path = ?self.path, "failed to extend a peer's head");
                }
            }
            let _ = self.lifecycle_waker.wait().await;
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Claim the remote-only span keyed `cover_start` for fetching, along
    /// with every other piece of its seal, since one download settles them
    /// all. The seal is re-derived against what is resident now first: the
    /// live head may have grown over it since it was last derived.
    ///
    /// Returns a claimed span (its seal keys the fetch) when the claim won;
    /// `None` means the span is missing, already resident, another fetch is
    /// in flight, it is a peer's unsealed head (range fetches only), or
    /// what is left of it lies above an unsealed head, where no install
    /// could land.
    pub fn begin_fetch(&self, cover_start: Timestamp) -> Option<NodeSpan> {
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        let span = *manifest.span(cover_start)?;
        let claimed = |s: &NodeSpan| s.seal == span.seal && s.state == SpanState::Fetching;
        if span.state != SpanState::RemoteOnly
            || span.source == SpanSource::PeerHead
            || manifest.spans.iter().any(claimed)
        {
            return None;
        }
        let ceiling = self.install_ceiling();
        if ceiling.is_some_and(|c| span.cover_end >= c) {
            return None;
        }
        let resident = extents(self.list.iter(), None);
        self.publish_manifest(false, |draft| {
            draft
                .spans
                .retain(|s| !(s.seal == span.seal && s.state == SpanState::RemoteOnly));
            draft.derive(&resident, None, [RemoteSeal::of(&span)]);
            for piece in draft.spans.iter_mut() {
                if piece.seal == span.seal && ceiling.is_none_or(|c| piece.cover_end < c) {
                    piece.state = SpanState::Fetching;
                }
            }
        })
        .expect("non-persisted manifest publish cannot fail");
        let manifest = self.manifest.load();
        manifest.spans.iter().find(|s| claimed(s)).copied()
    }

    /// The start of an unsealed head, which nothing may be installed above.
    fn install_ceiling(&self) -> Option<Timestamp> {
        let head = self.list.head()?;
        let start = *head.timestamps().first()?;
        let sealed = is_sealed(&self.manifest.load(), &head);
        (!sealed || self.has_writer.load(Ordering::Acquire)).then_some(start)
    }

    /// Release a fetch claim after a failed download so a later request
    /// can retry. The seal is re-derived against what is resident now
    /// rather than restored, since residency may have moved meanwhile.
    pub fn abort_fetch(&self, cover_start: Timestamp) {
        let _guard = self.structural.lock().unwrap();
        let Some(claimed) = self
            .manifest
            .load()
            .span(cover_start)
            .filter(|span| span.state == SpanState::Fetching)
            .copied()
        else {
            return;
        };
        let resident = extents(self.list.iter(), None);
        self.publish_manifest(false, |draft| {
            draft
                .spans
                .retain(|s| !(s.seal == claimed.seal && s.state != SpanState::Resident));
            draft.derive(&resident, None, [RemoteSeal::of(&claimed)]);
        })
        .expect("non-persisted manifest publish cannot fail");
    }

    /// Record that a store holds a durable, verified copy of the span.
    /// Persisted — the ack is what licenses a later purge, so it must
    /// survive restarts.
    pub fn mark_acked(&self, start_ts: Timestamp) -> Result<(), Error> {
        let _guard = self.structural.lock().unwrap();
        if self.manifest.load().span(start_ts).is_none() {
            return Ok(());
        }
        self.publish_manifest(true, |draft| {
            for span in draft.spans.iter_mut() {
                if span.cover_start == start_ts {
                    span.acked = true;
                }
            }
        })
    }

    /// Drop a resident span's local bytes. A span qualifies when something
    /// else can give the data back: an ack (a store holds this exact node),
    /// or, for [`SpanSource::Mirrored`] data, remote seals the manifest
    /// records as fetchable right now (none with a fetch in flight) around
    /// every one of its samples. The live head belongs to the writer and
    /// never qualifies.
    ///
    /// An acked span flips to remote-only under its own seal; the manifest
    /// changes durably before the node is unlinked, so a crash in between
    /// resurrects it as resident and it is simply purged again. An unacked
    /// mirrored span leaves the manifest and the remote seals over its
    /// range are re-derived to cover it, so it reads as fetchable at once;
    /// here the node goes first, so a crash in between finds a resident
    /// entry with no node, which reopening drops. Readers holding slices
    /// keep the unmapped bytes alive until they finish.
    ///
    /// A fragment, keyed by its claim's start or its node's, is just
    /// dropped (see [`Self::purge_fragment`]). Fragments touching a purged
    /// node's range go with it: one may claim the node's edge timestamps,
    /// which only the node held samples for.
    pub fn purge_span(&self, start_ts: Timestamp) -> Result<bool, Error> {
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        let Some(span) = manifest
            .span(start_ts)
            .copied()
            .filter(|span| span.state == SpanState::Resident)
        else {
            let fragment = manifest.fragments.iter().find(|f| {
                f.start == start_ts || f.node.is_some_and(|seal| seal.start_ts == start_ts)
            });
            return match fragment {
                Some(fragment) => self.purge_fragment_locked(&manifest, fragment.start),
                None => Ok(false),
            };
        };
        let Some(node) = self
            .list
            .iter()
            .find(|n| !n.fragment && n.timestamps().first() == Some(&start_ts))
        else {
            return Ok(false);
        };
        let mirrored = !span.acked && span.source == SpanSource::Mirrored;
        if !span.acked && !(mirrored && remote_holds(&manifest, node.timestamps(), false)) {
            return Ok(false);
        }
        let node_is_head = self
            .list
            .head()
            .is_some_and(|head| Arc::ptr_eq(&head, &node));
        if node_is_head && self.has_writer.load(Ordering::Acquire) {
            return Ok(false);
        }
        let (start, end) = (span.seal.start_ts.0, span.seal.end_ts.0);
        let mut gone = self.fragments_over(&manifest, &[(start, end)]);
        gone.nodes.push(node);

        if mirrored {
            let resident = extents(self.list.iter(), gone.nodes.last());
            self.unlink_nodes(&gone, start_ts)?;
            self.publish_manifest(true, |draft| {
                draft.spans.retain(|s| s.cover_start != start_ts);
                gone.forget(draft);
                draft.unsettle(start, end);
                draft.derive(&resident, Some(start..=end), []);
            })?;
        } else {
            self.publish_manifest(true, |draft| {
                for span in draft.spans.iter_mut() {
                    if span.cover_start == start_ts {
                        span.state = SpanState::RemoteOnly;
                    }
                }
                gone.forget(draft);
                draft.unsettle(start, end);
            })?;
            self.unlink_nodes(&gone, start_ts)?;
        }
        Ok(true)
    }

    /// Unlink the nodes in `gone` in one swap and delete their files: the
    /// node keyed `start_ts` and any fragments going with it.
    fn unlink_nodes(&self, gone: &Superseded, start_ts: Timestamp) -> Result<(), Error> {
        self.link(Vec::new(), &gone.nodes)?;
        gone.delete();
        std::fs::remove_dir_all(self.path.join(start_ts.0.to_string()))?;
        std::fs::File::open(&self.path)?.sync_all()?;
        Ok(())
    }

    /// Drop the [`Fragment`] keyed `start` (its claim's start) with its
    /// node, when the remote spans it lies in can give its samples back
    /// right now: eviction never takes data only this side holds, such as
    /// a fragment of a seal the peer has since dropped. A fragment that
    /// held no samples always qualifies.
    ///
    /// The claim leaves the manifest before the node leaves the list, so a
    /// reader never sees the stretch reported held with its samples gone.
    pub fn purge_fragment(&self, start: Timestamp) -> Result<bool, Error> {
        let _guard = self.structural.lock().unwrap();
        self.purge_fragment_locked(&self.manifest.load(), start)
    }

    fn purge_fragment_locked(
        &self,
        manifest: &ComponentManifest,
        start: Timestamp,
    ) -> Result<bool, Error> {
        let gone = self.fragments_over(manifest, &[(start.0, start.0)]);
        if let Some(node) = gone.nodes.first()
            && !remote_holds(manifest, node.timestamps(), true)
        {
            return Ok(false);
        }
        if gone.claims.is_empty() {
            return Ok(false);
        }
        self.publish_manifest(false, |draft| gone.forget(draft))?;
        self.link(Vec::new(), &gone.nodes)?;
        gone.delete();
        Ok(true)
    }

    /// Install the reply to a range fetch: `timestamps` and `data` (samples
    /// of `element_size` bytes) are every sample the peer holds within
    /// `covered`, which is half-open.
    ///
    /// Only the parts of `covered` still remote-only here are claimed, each
    /// stretch as a [`Fragment`] with the reply's samples in it (or none),
    /// so whatever is resident, mid-fetch, already claimed by a fragment,
    /// or above an unsealed head is left alone. Time no span covers is
    /// never claimed: nothing would hand it back as a gap once the fragment
    /// went, so the caller learns of it from an empty or short return and
    /// re-seeds the manifest instead.
    ///
    /// A stretch is never claimed across a resident node. At a timestamp a
    /// resident node starts or ends on (two peer nodes tying), the reply
    /// lists the tie oldest node first, so the node's own samples there are
    /// dropped from the reply's front when the node lies before the stretch
    /// and from its back when it lies after, and the rest stay in the
    /// fragment, which then shares the node's edge timestamp.
    /// Every fragment lands in one swap of the node list and one publish.
    /// Returns the stretches claimed, oldest first.
    ///
    /// Fragment nodes are staged and flushed under the structural lock,
    /// which a reply of [`crate::remote::PAGE_BYTES`] holds for one 4 MB
    /// write; staging first would mean re-deriving the stretches after
    /// taking the lock, as the manifest can move in between.
    pub fn install_fragment(
        &self,
        covered: Range<Timestamp>,
        element_size: usize,
        timestamps: &[Timestamp],
        data: &[u8],
    ) -> Result<SmallVec<[Range<Timestamp>; 2]>, Error> {
        if data.len() != timestamps.len() * element_size || element_size == 0 {
            return Err(Error::SchemaMismatch);
        }
        if timestamps.windows(2).any(|w| w[1].0 < w[0].0) {
            return Err(Error::TimeTravel);
        }
        if timestamps
            .iter()
            .any(|t| t.0 < covered.start.0 || t.0 >= covered.end.0)
        {
            return Err(Error::TimeRangeOutOfBounds);
        }
        let mut claimed = SmallVec::new();
        if covered.end.0 <= covered.start.0 {
            return Ok(claimed);
        }
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        let mut gaps = GapVec::new();
        manifest.gaps(&covered, &mut gaps);
        let mut runs: Vec<(i64, i64)> = gaps
            .iter()
            .filter(|g| g.state == SpanState::RemoteOnly)
            .map(|g| (g.range.start.0, g.range.end.0 - 1))
            .collect();
        runs.sort_unstable();
        let ceiling = self.install_ceiling().map(|c| c.0);
        let resident = extents(self.list.iter(), None);
        let mut stretches: Vec<(i64, i64)> = Vec::new();
        for (start, end) in runs {
            let end = ceiling.map_or(end, |c| end.min(c - 1));
            if end < start {
                continue;
            }
            match stretches.last_mut() {
                Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
                _ => stretches.push((start, end)),
            }
        }
        let held = |t: i64| {
            let at = resident.partition_point(|e| e.end < t);
            resident.get(at).is_some_and(|e| e.start <= t)
        };
        // Samples stamped `t` in resident nodes, which answer for that many
        // of the reply's.
        let resident_at = |t: i64| -> usize {
            if !held(t) {
                return 0;
            }
            self.list
                .iter()
                .filter(|node| !node.fragment)
                .map(|node| {
                    let stamps = node.timestamps();
                    stamps.partition_point(|s| s.0 <= t) - stamps.partition_point(|s| s.0 < t)
                })
                .sum()
        };
        let mut fragments = Vec::new();
        let mut nodes = Vec::new();
        for (start, end) in stretches {
            for (start, end) in around_nodes(start, end, &resident) {
                let mut lo = timestamps.partition_point(|t| t.0 < start);
                let mut hi = timestamps.partition_point(|t| t.0 <= end);
                // A node before the stretch is the older side of a tie at
                // `start`, one after it the newer side of a tie at `end`.
                let tie_end = timestamps.partition_point(|t| t.0 <= start).min(hi);
                lo += resident_at(start).min(tie_end.saturating_sub(lo));
                if end != start {
                    let tie_start = timestamps.partition_point(|t| t.0 < end).max(lo);
                    hi -= resident_at(end).min(hi.saturating_sub(tie_start));
                }
                let samples: Vec<usize> = (lo..hi).collect();
                let mut fragment = Fragment {
                    start: Timestamp(start),
                    end: Timestamp(end),
                    node: None,
                    dir: None,
                    access: Arc::new(LastAccess::now()),
                };
                if let Some(first) = samples.first() {
                    let staging = crate::store::NodeStaging::with_capacity(
                        &self.path,
                        timestamps[*first],
                        element_size as u64,
                        samples.len(),
                    )?;
                    for i in &samples {
                        staging.push(
                            timestamps[*i],
                            &data[i * element_size..(i + 1) * element_size],
                        )?;
                    }
                    let seal = staging.seal().expect("staged at least one sample");
                    let (dir, node) = staging.commit_fragment(&seal)?;
                    fragment.node = Some(seal);
                    fragment.dir = Some(Arc::from(dir));
                    fragment.access = node.last_access.clone();
                    nodes.push(node);
                }
                claimed.push(fragment.claim());
                fragments.push(fragment);
            }
        }
        if fragments.is_empty() {
            return Ok(claimed);
        }
        if let Err(err) = self.link(nodes, &[]) {
            for dir in fragments.iter().filter_map(|f| f.dir.as_ref()) {
                let _ = std::fs::remove_dir_all(dir);
            }
            return Err(err);
        }
        self.publish_manifest(false, |draft| draft.fragments.extend(fragments))?;
        self.data_waker.wake_all();
        Ok(claimed)
    }

    /// Re-derive the coverage of every idle remote seal meeting `range`
    /// from what is resident now. Call after local data in `range` went
    /// away by a path that does not re-derive itself (as
    /// [`Self::purge_span`] does), so seals it shadowed become fetchable
    /// again. Returns whether any seal was re-derived.
    pub fn rederive_coverage(&self, range: RangeInclusive<Timestamp>) -> Result<bool, Error> {
        let _guard = self.structural.lock().unwrap();
        let resident = extents(self.list.iter(), None);
        let mut derived = false;
        self.publish_manifest(true, |draft| {
            draft.unsettle(range.start().0, range.end().0);
            derived = draft.derive(&resident, Some(range.start().0..=range.end().0), []);
        })?;
        Ok(derived)
    }

    /// Publish a manifest snapshot derived from the current one by `f`,
    /// re-sorted so lookups can rely on key order. With `persist` the
    /// snapshot is durably on disk before it becomes visible, so a crash
    /// never reveals state the disk does not back. Caller holds the
    /// structural mutex.
    fn publish_manifest(&self, persist: bool, f: impl FnOnce(&mut Draft)) -> Result<(), Error> {
        let mut draft = Draft::of(&self.manifest.load());
        f(&mut draft);
        let next = draft.into_manifest();
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
    /// checksum is `Ok`. Remote coverage the node now answers is re-derived
    /// around it.
    pub fn install_node(
        &self,
        staging: crate::store::NodeStaging,
        seal: &SealRecord,
        source: SpanSource,
    ) -> Result<(), Error> {
        let _guard = self.structural.lock().unwrap();
        self.install_node_locked(staging, seal, source, false)
    }

    /// [`Self::install_node`] with the structural mutex already held; `tie`
    /// as for [`Self::check_splice`].
    ///
    /// Fragments overlapping the seal's range give way to the node in the
    /// same swap of the node list and the same manifest publish, so readers
    /// never see both, or neither.
    fn install_node_locked(
        &self,
        staging: crate::store::NodeStaging,
        seal: &SealRecord,
        source: SpanSource,
        tie: bool,
    ) -> Result<(), Error> {
        let manifest = self.manifest.load();
        if let Some(span) = manifest.span(seal.start_ts)
            && span.state == SpanState::Resident
        {
            return if span.seal.checksum == seal.checksum {
                Ok(())
            } else {
                Err(crate::store::StoreError::ChecksumMismatch.into())
            };
        }
        let superseded = self.fragments_over(&manifest, &[(seal.start_ts.0, seal.end_ts.0)]);
        self.check_splice(seal, tie, &superseded.nodes)?;
        let (_node_dir, node) = staging.commit(seal)?;
        self.link(vec![node], &superseded.nodes)?;
        let resident = extents(self.list.iter(), None);
        self.publish_manifest(true, |draft| {
            // A whole remote span this node fulfils gives way to it.
            draft
                .spans
                .retain(|s| !(s.cover_start == seal.start_ts && s.seal == *seal));
            // A fetch claimed coverage this node now answers; drop the claim
            // so the seal re-derives below, and its fetch finds it lost.
            let stale: SmallVec<[SealRecord; 2]> = draft
                .spans
                .iter()
                .filter(|s| {
                    s.state == SpanState::Fetching
                        && s.cover_start <= seal.end_ts
                        && seal.start_ts <= s.cover_end
                })
                .map(|s| s.seal)
                .collect();
            for span in draft.spans.iter_mut() {
                if span.state == SpanState::Fetching && stale.contains(&span.seal) {
                    span.state = SpanState::RemoteOnly;
                }
            }
            superseded.forget(draft);
            // Whatever store this came from holds a copy by definition.
            let acked = matches!(source, SpanSource::RemoteFetch);
            draft
                .spans
                .push(NodeSpan::whole(*seal, SpanState::Resident, source, acked));
            draft.derive(&resident, Some(seal.start_ts.0..=seal.end_ts.0), []);
        })?;
        superseded.delete();
        self.data_waker.wake_all();
        // A span that arrives whole is as final as one the lifecycle task
        // seals, and the LoD engine sleeps on this queue.
        self.seal_waker.wake_all();
        Ok(())
    }

    /// Check that a node sealed by `seal` can be spliced into the list,
    /// with `replaced` unlinked in the same swap. Caller holds the
    /// structural mutex.
    ///
    /// The node must fill a hole: overlap with resident data is refused,
    /// except, with `tie`, a shared edge timestamp — a peer node whose
    /// first samples tie with its predecessor's last. Fragments are not
    /// resident data here: whoever installs over one replaces it.
    ///
    /// A node above the head is rejected whenever the head is *unsealed* —
    /// the working node a writer is appending to, which sealing always
    /// skips and so never has a manifest span (even across restarts,
    /// before the first live sample re-creates the writer). Hydration only
    /// fills gaps behind the live feed, so a sealed node that would outrank
    /// that head (e.g. pushed by a peer whose clock ran ahead) would break
    /// the newest-first order and race the seal of the in-flight head.
    /// An archive's head is always sealed (every node arrived via
    /// `install_node`, which publishes a Resident span), so pushes above it
    /// legitimately land. A live writer over an empty-timestamp head still
    /// rejects, keyed off `has_writer`.
    fn check_splice(
        &self,
        seal: &SealRecord,
        tie: bool,
        replaced: &[Arc<AtomicNode<TimeSeriesNode>>],
    ) -> Result<(), Error> {
        use crate::store::StoreError;
        let manifest = self.manifest.load();
        let mut newer = false;
        for node in self.list.iter() {
            if replaced.iter().any(|r| Arc::ptr_eq(r, &node)) {
                continue;
            }
            let timestamps = node.timestamps();
            let (Some(start), Some(end)) = (timestamps.first(), timestamps.last()) else {
                continue;
            };
            let overlaps = seal.start_ts.0 <= end.0 && start.0 <= seal.end_ts.0;
            let edge = seal.start_ts == *end || seal.end_ts == *start;
            if overlaps && !(tie && edge) && !node.fragment {
                return Err(StoreError::Other("span overlaps resident data".to_string()).into());
            }
            newer |= start.0 > seal.start_ts.0;
        }
        if !newer && let Some(head) = self.list.head() {
            // The head is the live writer's working node iff it is
            // unsealed; an archive's head is always sealed. Reject above an
            // unsealed head (the window after reopen, before the first live
            // sample, that a `has_writer`-only guard misses) or whenever a
            // writer is live (covers an empty-timestamp head).
            let head_unsealed = !head.timestamps().is_empty() && !is_sealed(&manifest, &head);
            if head_unsealed || self.has_writer.load(Ordering::Acquire) {
                return Err(
                    StoreError::Other("span is newer than the live head".to_string()).into(),
                );
            }
        }
        Ok(())
    }

    /// Link committed `nodes` into the list at their timestamp-ordered
    /// positions and unlink `replaced`, in one swap. Caller holds the
    /// structural mutex and checked each node with [`Self::check_splice`].
    fn link(
        &self,
        nodes: Vec<TimeSeriesNode>,
        replaced: &[Arc<AtomicNode<TimeSeriesNode>>],
    ) -> Result<(), Error> {
        if nodes.is_empty() && replaced.is_empty() {
            return Ok(());
        }
        // Newest first by first sample, then by last: a fragment holding
        // only the newer side of a tie at a node's first timestamp would
        // otherwise order arbitrarily against it. An empty node can only
        // be the writer's fresh head, newer than anything installed.
        let key = |node: &TimeSeriesNode| {
            let stamps = node.timestamps();
            match (stamps.first(), stamps.last()) {
                (Some(first), Some(last)) => (first.0, last.0),
                _ => (i64::MAX, i64::MAX),
            }
        };
        if self.list.replace(replaced, nodes, |a, b| key(a) > key(b)) {
            Ok(())
        } else {
            Err(
                crate::store::StoreError::Other("splice lost a node it replaces".to_string())
                    .into(),
            )
        }
    }

    /// The fragments whose claims meet any of the inclusive `ranges`, with
    /// their nodes, for an install over them to replace.
    fn fragments_over(&self, manifest: &ComponentManifest, ranges: &[(i64, i64)]) -> Superseded {
        let mut superseded = Superseded::default();
        if manifest.fragments.is_empty() {
            return superseded;
        }
        for fragment in manifest.fragments.iter() {
            if ranges.iter().any(|(a, b)| fragment.overlaps(*a, *b)) {
                superseded.claims.push(fragment.start);
                superseded.dirs.extend(fragment.dir.clone());
            }
        }
        superseded.nodes = self.fragment_nodes(manifest, &superseded.claims);
        superseded
    }

    /// The list nodes of the fragments keyed by `claims`.
    fn fragment_nodes(
        &self,
        manifest: &ComponentManifest,
        claims: &[Timestamp],
    ) -> Vec<Arc<AtomicNode<TimeSeriesNode>>> {
        let starts: SmallVec<[Timestamp; 4]> = manifest
            .fragments
            .iter()
            .filter(|f| claims.contains(&f.start))
            .filter_map(|f| f.node.map(|seal| seal.start_ts))
            .collect();
        if starts.is_empty() {
            return Vec::new();
        }
        self.list
            .iter()
            .filter(|node| {
                node.fragment
                    && node
                        .timestamps()
                        .first()
                        .is_some_and(|first| starts.contains(first))
            })
            .collect()
    }

    /// Install a node fetched for a claimed remote seal (see
    /// [`Self::begin_fetch`]), verified against `seal`. A seal claimed
    /// whole installs unchanged. Otherwise local data overlaps it, and each
    /// claimed piece is cut from the download into a node of its own,
    /// marked [`SpanSource::Mirrored`]: those nodes do not match any node
    /// the peer serves.
    ///
    /// Every piece lands in one swap of the node list and one publish,
    /// together with the stretches the download settled (including pieces
    /// that held no samples), and the seal is re-derived against what is
    /// resident by then, so it ends up shadowed unless local data moved
    /// during the fetch. Fragments over a landed piece give way to it.
    pub fn install_fetched(
        &self,
        staging: crate::store::NodeStaging,
        seal: &SealRecord,
    ) -> Result<(), Error> {
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        let pieces: SmallVec<[NodeSpan; 2]> = manifest
            .spans
            .iter()
            .filter(|s| s.seal == *seal && s.state == SpanState::Fetching)
            .copied()
            .collect();
        match &pieces[..] {
            [] => {
                return Err(
                    crate::store::StoreError::Other("fetch claim was lost".to_string()).into(),
                );
            }
            [piece] if piece.is_whole() => {
                return self.install_node_locked(staging, seal, SpanSource::RemoteFetch, true);
            }
            _ => {}
        }
        let covers: SmallVec<[(i64, i64); 2]> = pieces
            .iter()
            .map(|p| (p.cover_start.0, p.cover_end.0))
            .collect();
        let replaced = self.fragments_over(&manifest, &covers);
        let mut landed: SmallVec<[(NodeSpan, Option<SealRecord>); 2]> = SmallVec::new();
        let mut nodes = Vec::new();
        let mut installed: Result<(), Error> = Ok(());
        for piece in &pieces {
            let cut = staging.cut(piece.cover_start..=piece.cover_end);
            installed = cut.and_then(|cut| match cut {
                Some((node, piece_seal)) => {
                    tag_mirrored(node.dir())?;
                    self.check_splice(&piece_seal, true, &replaced.nodes)?;
                    nodes.push(node.commit(&piece_seal)?.1);
                    landed.push((*piece, Some(piece_seal)));
                    Ok(())
                }
                None => {
                    landed.push((*piece, None));
                    Ok(())
                }
            });
            if installed.is_err() {
                break;
            }
        }
        // Only fragments over what landed give way; the rest still answer
        // for pieces a failed install left remote.
        let landed_covers: SmallVec<[(i64, i64); 2]> = landed
            .iter()
            .map(|(p, _)| (p.cover_start.0, p.cover_end.0))
            .collect();
        let superseded = self.fragments_over(&manifest, &landed_covers);
        if let Err(err) = self.link(nodes, &superseded.nodes) {
            for (_, piece_seal) in &landed {
                if let Some(piece_seal) = piece_seal {
                    let dir = self.path.join(piece_seal.start_ts.0.to_string());
                    let _ = std::fs::remove_dir_all(dir);
                }
            }
            return Err(err);
        }
        let resident = extents(self.list.iter(), None);
        let remote = RemoteSeal::of(&pieces[0]);
        self.publish_manifest(true, |draft| {
            // Pieces left above an unsealed head re-derive with the rest.
            draft
                .spans
                .retain(|s| !(s.seal == *seal && s.state != SpanState::Resident));
            for (piece, piece_seal) in &landed {
                if let Some(piece_seal) = piece_seal {
                    draft.spans.push(NodeSpan::whole(
                        *piece_seal,
                        SpanState::Resident,
                        SpanSource::Mirrored,
                        false,
                    ));
                }
                draft.settle(seal, piece.cover_start, piece.cover_end);
            }
            superseded.forget(draft);
            draft.derive(&resident, None, [remote]);
        })?;
        superseded.delete();
        if landed.iter().any(|(_, piece_seal)| piece_seal.is_some()) {
            self.data_waker.wake_all();
            self.seal_waker.wake_all();
        }
        installed
    }

    /// Stage `samples` as one sealed node and splice it in behind the live
    /// head. Backfill's counterpart to the writer: [`TimerSeriesWriter`]
    /// only ever appends forward, so history computed after the fact has
    /// to arrive as a node of its own. Returns the installed seal so the
    /// caller can refresh bookkeeping derived from it (the DB's earliest
    /// timestamp); an empty batch installs nothing.
    ///
    /// Timestamps must be non-decreasing and every sample exactly
    /// `element_size` bytes. A violation errors with nothing installed —
    /// staged bytes only ever become a node at [`Self::install_node`], and
    /// the staging directory is removed when it is dropped instead.
    pub fn install_samples<'a>(
        &self,
        element_size: usize,
        samples: impl IntoIterator<Item = (Timestamp, &'a [u8])>,
        source: SpanSource,
    ) -> Result<Option<SealRecord>, Error> {
        let mut samples = samples.into_iter().peekable();
        let Some(&(start_ts, _)) = samples.peek() else {
            return Ok(None);
        };
        let staging = crate::store::NodeStaging::with_capacity(
            &self.path,
            start_ts,
            element_size as u64,
            samples.size_hint().0,
        )?;
        let mut prev = start_ts;
        for (timestamp, bytes) in samples {
            if bytes.len() != element_size {
                return Err(Error::SchemaMismatch);
            }
            if timestamp.0 < prev.0 {
                return Err(Error::TimeTravel);
            }
            staging.push(timestamp, bytes)?;
            prev = timestamp;
        }
        let seal = staging.seal().expect("staged batch is non-empty");
        self.install_node(staging, &seal, source)?;
        Ok(Some(seal))
    }

    /// Merge a peer's sealed-node manifest into this component as
    /// remote-only coverage. This is how history recorded before a mirror
    /// connected becomes visible: each seal lands as fetchable
    /// [`SpanState::RemoteOnly`] coverage without moving any payload bytes.
    /// One snapshot publish regardless of input size, so seeding a year
    /// of spans costs a single sort and fsync.
    ///
    /// Every seal is recorded, whatever it overlaps. Its coverage is its
    /// range minus what this side already holds: pieces before, after or
    /// between local data (the peer sealed across a mirror's connect, or
    /// while it was disconnected), or nothing at all, in which case the
    /// seal is kept shadowed until that local data goes away. Idempotent:
    /// re-merging known seals adds nothing. Returns the number of seals
    /// recorded.
    pub fn merge_remote_spans(
        &self,
        seals: impl IntoIterator<Item = SealRecord>,
    ) -> Result<usize, Error> {
        self.update_remote_spans(seals.into_iter().collect(), false, None)
    }

    /// [`Self::merge_remote_spans`] for a peer's complete listing: seals
    /// recorded from the peer earlier that it no longer lists (its own
    /// tiering dropped them) are forgotten, so nothing counts on the peer
    /// for data it can no longer serve. A node hydrated from a seal the
    /// listing omits stays resident but loses its ack until the peer lists
    /// it again, so it is not purged as though the peer could give it back.
    /// Seals with a fetch in flight are left to that fetch. Returns the
    /// number of seals recorded.
    pub fn sync_remote_spans(&self, listing: Vec<SealRecord>) -> Result<usize, Error> {
        self.update_remote_spans(listing, true, None)
    }

    /// [`Self::sync_remote_spans`] with the peer's unsealed head from the
    /// same listing, recorded as [`SpanSource::PeerHead`] coverage in place
    /// of the head the last listing reported (`None` drops it).
    ///
    /// The head is cut off below an unsealed head of this side's own: a
    /// mirror's live ingest already holds the head's newest stretch, and a
    /// remote span reaching above its live head would be a gap nothing can
    /// fill. What the head covered and the peer has since sealed arrives
    /// in the same listing as sealed spans, so a fragment read from the
    /// head never loses the remote span under it.
    ///
    /// A listing is a snapshot: the peer wrote on after it, and a mirror's
    /// live data starts wherever its stream did. So the head reaches up to
    /// the current mirror run's first sample, which the peer held by the
    /// time it streamed it, and the backfill meets the stream rather than
    /// stopping wherever the listing happened to. A run that starts after
    /// the listing carries it there then ([`Self::meet_mirror_run`]).
    pub fn sync_remote(
        &self,
        listing: Vec<SealRecord>,
        head: Option<UnsealedHead>,
    ) -> Result<usize, Error> {
        self.update_remote_spans(listing, true, head)
    }

    /// Carry the listed peer head up to the mirror run that just started
    /// (see [`Self::sync_remote`]). The lifecycle task calls this when the
    /// writer starts a run, since the head listed on connect is usually
    /// cut before the stream's first sample arrives.
    pub fn meet_mirror_run(&self) -> Result<(), Error> {
        self.update_remote_spans(Vec::new(), false, None)?;
        Ok(())
    }

    /// The seal a peer's `head` is recorded under, and a zero checksum,
    /// which no node a peer serves carries in practice.
    ///
    /// It runs up to the current mirror run's first sample when that is
    /// past the listing's end, and stops below this side's
    /// [`Self::install_ceiling`] either way. Cut short, the count is
    /// prorated; carried on, it stays what the peer counted, since the
    /// stretch past the listing may be anywhere from no samples to many.
    fn head_seal(&self, head: UnsealedHead) -> Option<SealRecord> {
        let (start, listed_end) = (head.start_ts.0, head.end_ts.0);
        if head.count == 0 || listed_end < start {
            return None;
        }
        let run_start = self.run_start.load(Ordering::Acquire);
        let reach = match run_start {
            i64::MIN => listed_end,
            run_start => listed_end.max(run_start - 1),
        };
        let end = match self.install_ceiling() {
            Some(ceiling) => reach.min(ceiling.0.saturating_sub(1)),
            None => reach,
        };
        if end < start {
            return None;
        }
        let len = |end: i64| (end as i128 - start as i128 + 1) as u128;
        let count =
            ((head.count as u128 * len(end.min(listed_end))) / len(listed_end)).max(1) as u64;
        Some(SealRecord {
            start_ts: head.start_ts,
            end_ts: Timestamp(end),
            count,
            index_len: count.saturating_mul(size_of::<Timestamp>() as u64),
            data_len: count.saturating_mul(head.element_size),
            checksum: 0,
            element_size: head.element_size,
        })
    }

    /// What this side holds past its sealed spans, as a peer listing it
    /// reports: the live head, and any node rolled past but not sealed yet.
    /// Read against one manifest snapshot, so a node sealed meanwhile still
    /// counts here, where a listing taken from the same snapshot omits it.
    pub fn unsealed_head(&self, manifest: &ComponentManifest) -> Option<UnsealedHead> {
        let mut head: Option<UnsealedHead> = None;
        for node in self.list.iter() {
            if is_sealed(manifest, &node) {
                continue;
            }
            let timestamps = node.timestamps();
            let (Some(first), Some(last)) = (timestamps.first(), timestamps.last()) else {
                continue;
            };
            let head = head.get_or_insert(UnsealedHead {
                start_ts: *first,
                end_ts: *last,
                count: 0,
                element_size: node.element_size() as u64,
            });
            head.start_ts = head.start_ts.min(*first);
            head.end_ts = head.end_ts.max(*last);
            head.count += timestamps.len() as u64;
        }
        head
    }

    /// Merge `seals`, a complete listing when `complete`. Only a complete
    /// listing replaces the peer's `head`; otherwise the listed one is cut
    /// again, against where this side's live data stands now.
    fn update_remote_spans(
        &self,
        mut seals: Vec<SealRecord>,
        complete: bool,
        head: Option<UnsealedHead>,
    ) -> Result<usize, Error> {
        let _guard = self.structural.lock().unwrap();
        let manifest = self.manifest.load();
        let old_head = manifest
            .spans
            .iter()
            .find(|s| s.source == SpanSource::PeerHead)
            .map(|s| s.seal);
        let head = {
            let mut listed = self.peer_head.lock().unwrap();
            if complete {
                *listed = head;
            }
            *listed
        };
        let new_head = head.and_then(|head| self.head_seal(head));
        let head_moved = old_head != new_head;
        seals.retain(|seal| seal.count > 0 && seal.start_ts.0 <= seal.end_ts.0);
        seals.sort_unstable_by_key(|seal| (seal.start_ts.0, seal.checksum));
        seals.dedup();
        let known = |seal: &SealRecord| {
            let first = manifest
                .spans
                .partition_point(|s| s.cover_start.0 < seal.start_ts.0);
            let shadowed = manifest
                .shadowed
                .partition_point(|s| s.start_ts.0 < seal.start_ts.0);
            manifest.spans[first..]
                .iter()
                .take_while(|s| s.cover_start.0 <= seal.end_ts.0)
                .any(|s| s.seal == *seal)
                || manifest.shadowed[shadowed..]
                    .iter()
                    .take_while(|s| s.start_ts == seal.start_ts)
                    .any(|s| s == seal)
        };
        let listed = |seal: &SealRecord| {
            seals
                .binary_search_by_key(&(seal.start_ts.0, seal.checksum), |s| {
                    (s.start_ts.0, s.checksum)
                })
                .is_ok()
        };
        let mut stale: Vec<SealRecord> = Vec::new();
        if complete {
            let fetching: Vec<&SealRecord> = manifest
                .spans
                .iter()
                .filter(|s| s.state == SpanState::Fetching)
                .map(|s| &s.seal)
                .collect();
            stale.extend(
                manifest
                    .spans
                    .iter()
                    .filter(|s| {
                        s.state == SpanState::RemoteOnly && s.source == SpanSource::RemoteFetch
                    })
                    .map(|s| s.seal)
                    .chain(manifest.shadowed.iter().copied())
                    .filter(|seal| !listed(seal) && !fetching.contains(&seal)),
            );
            stale.sort_unstable_by_key(|seal| (seal.start_ts.0, seal.checksum));
            stale.dedup();
        }
        let stale_settled = complete
            && manifest.settled.iter().any(|s| {
                seals
                    .binary_search_by_key(&(s.seal_start.0, s.checksum), |seal| {
                        (seal.start_ts.0, seal.checksum)
                    })
                    .is_err()
            });
        // A hydrated node's ack says the peer can give it back. A complete
        // listing without its seal withdraws that, and listing it again
        // restores it, so eviction never purges what only this side holds.
        let reacked: Vec<Timestamp> = manifest
            .spans
            .iter()
            .filter(|s| s.state == SpanState::Resident && s.source == SpanSource::RemoteFetch)
            .filter(|s| {
                let held = listed(&s.seal);
                if s.acked { complete && !held } else { held }
            })
            .map(|s| s.cover_start)
            .collect();
        let added: Vec<SealRecord> = seals.iter().filter(|seal| !known(seal)).copied().collect();
        let durable =
            !(added.is_empty() && stale.is_empty() && !stale_settled && reacked.is_empty());
        if !durable && !head_moved {
            return Ok(0);
        }
        let count = added.len();
        // The time a dropped seal or the old head answered for, which the
        // seals around it may take back.
        let freed = stale
            .iter()
            .chain(old_head.iter().filter(|_| head_moved))
            .map(|s| s.start_ts.0..=s.end_ts.0)
            .reduce(|a, b| *a.start().min(b.start())..=*a.end().max(b.end()));
        let resident = extents(self.list.iter(), None);
        let head = new_head.filter(|_| head_moved).map(|seal| RemoteSeal {
            seal,
            source: SpanSource::PeerHead,
            acked: false,
        });
        // Heads are never persisted, so moving one alone skips the write.
        self.publish_manifest(durable, |draft| {
            for seal in &stale {
                draft.forget(seal);
            }
            if head_moved {
                draft.spans.retain(|s| s.source != SpanSource::PeerHead);
            }
            for span in draft.spans.iter_mut() {
                if reacked.binary_search(&span.cover_start).is_ok() {
                    span.acked = !span.acked;
                }
            }
            if complete {
                draft.settled.retain(|s| {
                    seals
                        .binary_search_by_key(&(s.seal_start.0, s.checksum), |seal| {
                            (seal.start_ts.0, seal.checksum)
                        })
                        .is_ok()
                });
            }
            draft.derive(
                &resident,
                freed,
                added.into_iter().map(RemoteSeal::peer).chain(head),
            );
        })?;
        Ok(count)
    }

    /// Estimate how many samples fall inside `range`, counting sealed
    /// spans from the manifest and the live head from its index — no
    /// payload access. This is the cost signal plots use to decide
    /// whether a window is renderable raw or needs the envelope.
    pub fn estimate_samples(&self, range: Range<Timestamp>) -> u64 {
        let manifest = self.manifest.load();
        let mut total = manifest.count_in(&range);
        // Add every resident node the manifest hasn't sealed yet — the
        // live head plus any nodes rolled off the writer but not yet
        // sealed. Counting only the head left those rolled-but-unsealed
        // nodes invisible, skewing the panel's raw/LoD switchover.
        for node in self.list.iter() {
            let timestamps = node.timestamps();
            if timestamps.is_empty() {
                continue;
            }
            // A fragment's samples are already counted by the remote span
            // it lies in.
            if is_sealed(&manifest, &node) {
                continue;
            }
            let lo = timestamps.partition_point(|t| t.0 < range.start.0);
            let hi = timestamps.partition_point(|t| t.0 < range.end.0);
            total += (hi - lo) as u64;
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
        let head = self.list.head()?;
        Self::binary_search_nearest_from(head, timestamp, inclusive)
    }

    /// Find the nearest sample to `timestamp` walking the spine seeded
    /// from `head`. Splitting the search off the captured head (rather
    /// than re-reading `list.head()` per call) is what lets `get_range`
    /// run both endpoint searches over a single immutable snapshot: a
    /// `purge_span`/`install_node` splice between the two would otherwise
    /// rebuild the newer node shells, landing `start` and `end` on
    /// different spines so `as_iter`'s `ptr_eq` sentinel never matches.
    fn binary_search_nearest_from(
        head: Arc<AtomicNode<TimeSeriesNode>>,
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
        for node in AtomicStackIter::new(ArcAtomic::from(head)) {
            let timestamps = node.timestamps();
            let (Some(start), Some(end)) = (timestamps.first(), timestamps.last()) else {
                continue;
            };
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
                // Within a node, ties resolve to the first equal sample
                // before `timestamp` and the last after it.
                let index = match inclusive {
                    false => timestamps.partition_point(|t| t.0 < timestamp.0),
                    true => timestamps
                        .partition_point(|t| t.0 <= timestamp.0)
                        .saturating_sub(1),
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
        // Capture the head once so both endpoint searches run over the
        // same immutable spine; nodes and `prev` links never mutate once
        // published, so the snapshot stays consistent for the query.
        let head = self.list.head()?;
        let start = Self::first_at_or_after(head.clone(), range.start);
        let end = Self::last_at_or_before(head.clone(), range.end);
        match (start, end) {
            (Some(start), Some(end)) if start.timestamp.0 <= end.timestamp.0 => {
                Some(TimeSeriesSlice { start, end })
            }
            _ => Self::empty_slice(head),
        }
    }

    /// The first sample at or after `timestamp`, following a tie into
    /// older nodes so a range starting on it holds all of it.
    fn first_at_or_after(
        head: Arc<AtomicNode<TimeSeriesNode>>,
        timestamp: Timestamp,
    ) -> Option<TimestampRef> {
        let mut found = None;
        for node in AtomicStackIter::new(ArcAtomic::from(head)) {
            let timestamps = node.timestamps();
            let Some(last) = timestamps.last() else {
                continue;
            };
            if last.0 < timestamp.0 {
                break;
            }
            let index = timestamps.partition_point(|t| t.0 < timestamp.0);
            let stamp = timestamps[index];
            found = Some(TimestampRef {
                timestamp: stamp,
                node,
                index,
            });
            if index > 0 {
                break;
            }
        }
        found
    }

    /// The last sample at or before `timestamp`.
    fn last_at_or_before(
        head: Arc<AtomicNode<TimeSeriesNode>>,
        timestamp: Timestamp,
    ) -> Option<TimestampRef> {
        AtomicStackIter::new(ArcAtomic::from(head)).find_map(|node| {
            let timestamps = node.timestamps();
            let index = timestamps
                .partition_point(|t| t.0 <= timestamp.0)
                .checked_sub(1)?;
            let stamp = timestamps[index];
            Some(TimestampRef {
                timestamp: stamp,
                node,
                index,
            })
        })
    }

    /// A slice of no samples, on the newest node that has any.
    fn empty_slice(head: Arc<AtomicNode<TimeSeriesNode>>) -> Option<TimeSeriesSlice> {
        let node = AtomicStackIter::new(ArcAtomic::from(head))
            .find(|node| !node.timestamps().is_empty())?;
        let timestamp = node.timestamps()[0];
        let end = TimestampRef {
            timestamp,
            node: node.clone(),
            index: 0,
        };
        let start = TimestampRef {
            timestamp,
            node,
            index: 1,
        };
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
                run: 0,
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
    /// The mirror run this writer last pushed for, so it notices the
    /// first sample of the next one.
    run: u64,
}

impl TimerSeriesWriter {
    pub fn push_buf(&mut self, timestamp: Timestamp, buf: &[u8]) -> Result<(), Error> {
        let mut rolled = false;
        loop {
            if self.try_push_buf(timestamp, buf)? {
                break;
            }
            let node_dir = self.time_series.path.join(timestamp.0.to_string());
            let node = TimeSeriesNode::create(&node_dir, timestamp, buf.len() as u64)?;
            if self.time_series.mirror_run.load(Ordering::Acquire) != 0 {
                tag_mirrored(&node_dir)?;
            }
            let _ = self.time_series.list.try_push(node);
            rolled = true;
        }
        let run = self.time_series.mirror_run.load(Ordering::Relaxed);
        let started = run != self.run;
        if started {
            self.run = run;
            self.time_series
                .run_start
                .store(timestamp.0, Ordering::Release);
        }
        if rolled || started {
            // The previous head is now immutable, or a mirror run started;
            // let the lifecycle task seal and re-cut off the hot path. A
            // stored wakeup reaches it even while it is busy.
            self.time_series.lifecycle_waker.wake();
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
        if self.time_series.roll_pending.swap(false, Ordering::AcqRel)
            && !head.timestamps().is_empty()
        {
            return Ok(false);
        }
        if let Some(max_age) = self.time_series.max_node_age_us()
            && let Some(first) = head.timestamps().first()
            && timestamp.0.saturating_sub(first.0) > max_age
        {
            return Ok(false);
        }
        if head.index.len() as usize + size_of::<Timestamp>() > head.index.capacity() {
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
        is_sealed(&self.time_series.manifest.load(), head)
    }
}

#[cfg(test)]
mod tests {

    /// The timestamps `get_range(start..end)` covers, oldest first.
    fn range_stamps(series: &TimeSeries, start: i64, end: i64) -> Vec<i64> {
        let Some(slice) = series.get_range(Timestamp(start)..Timestamp(end)) else {
            return vec![];
        };
        let mut nodes: Vec<_> = slice.as_iter().collect();
        nodes.reverse();
        nodes
            .iter()
            .flat_map(|n| n.timestamps().iter().map(|t| t.0))
            .collect()
    }

    #[test]
    fn range_holds_whole_ties() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        for (i, ts) in [1i64, 2, 3, 3, 3, 4, 5].iter().enumerate() {
            writer
                .push_buf(Timestamp(*ts), &(i as u64).to_le_bytes())
                .unwrap();
        }
        assert_eq!(range_stamps(&series, 3, 3), [3, 3, 3]);
        assert_eq!(range_stamps(&series, 0, 3), [1, 2, 3, 3, 3]);
        assert_eq!(range_stamps(&series, 3, 9), [3, 3, 3, 4, 5]);
        assert_eq!(range_stamps(&series, 6, 9), Vec::<i64>::new());
        assert_eq!(range_stamps(&series, -5, 0), Vec::<i64>::new());
    }

    #[test]
    fn range_before_or_between_samples_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        for ts in [5i64, 10] {
            writer.push_buf(Timestamp(ts), &7u64.to_le_bytes()).unwrap();
        }
        assert_eq!(range_stamps(&series, 0, 4), Vec::<i64>::new());
        assert_eq!(range_stamps(&series, 6, 9), Vec::<i64>::new());
        assert_eq!(range_stamps(&series, 5, 5), [5]);
        assert_eq!(range_stamps(&series, 0, 99), [5, 10]);
        let empty = series.get_range(Timestamp(0)..Timestamp(4)).unwrap();
        assert_eq!(empty.len(), 0);
        assert!(empty.as_iter().all(|n| n.data().is_empty()));
    }

    /// 2 MiB samples roll a node every 64: node A ends at 63, node B is
    /// [63, 63].
    #[test]
    fn range_follows_a_tie_into_the_older_node() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        let buf = vec![1u8; 2 * 1024 * 1024];
        for ts in (0..64i64).chain([63, 63]) {
            writer.push_buf(Timestamp(ts), &buf).unwrap();
        }
        assert_eq!(series.iter_node_slices().count(), 2);
        assert_eq!(range_stamps(&series, 63, 63), [63, 63, 63]);
        assert_eq!(range_stamps(&series, 63, 100), [63, 63, 63]);
        assert_eq!(range_stamps(&series, 60, 63), [60, 61, 62, 63, 63, 63]);
        assert_eq!(range_stamps(&series, 0, 100).len(), 66);
    }
    use super::*;

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
    fn merge_remote_spans_fills_around_resident_data() {
        let dir = tempfile::tempdir().unwrap();
        let node = TimeSeriesNode::create(dir.path().join("100"), Timestamp(100), 8).unwrap();
        for ts in [100i64, 199] {
            node.data.write(&ts.to_le_bytes()).unwrap();
            node.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        let series = TimeSeries::open(dir.path()).unwrap();

        // Every seal is recorded: a tail overlap keeps its prefix, a front
        // overlap keeps its suffix, and one inside the resident data is
        // kept shadowed.
        let added = series
            .merge_remote_spans([
                remote_seal(0, 150),
                remote_seal(120, 250),
                remote_seal(110, 180),
            ])
            .unwrap();
        assert_eq!(added, 3);
        let manifest = series.manifest();
        assert_eq!(manifest.spans.len(), 2);
        assert_eq!(manifest.spans[0].seal.start_ts, Timestamp(0));
        assert_eq!(manifest.spans[0].cover(), Timestamp(0)..Timestamp(100));
        assert_eq!(manifest.spans[1].seal.start_ts, Timestamp(120));
        assert_eq!(manifest.spans[1].cover(), Timestamp(200)..Timestamp(251));
        assert_eq!(&*manifest.shadowed, &[remote_seal(110, 180)]);

        let mut gaps = GapVec::new();
        manifest.gaps(&(Timestamp(0)..Timestamp(300)), &mut gaps);
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0].range, Timestamp(0)..Timestamp(100));
        assert_eq!(gaps[1].range, Timestamp(200)..Timestamp(251));
        assert_eq!(gaps[1].start_ts, Timestamp(200));

        // Known seals, shadowed or not, merge to nothing.
        assert_eq!(
            series
                .merge_remote_spans([remote_seal(110, 180), remote_seal(120, 250)])
                .unwrap(),
            0
        );
    }

    /// The peer's node spans `[0, 140]` every 10; this side holds samples
    /// at 100 and 101, sealed. Returns the series and the peer's node.
    fn split_fixture(dir: &Path) -> (TimeSeries, TimeSeriesNode) {
        for (start, stamps) in [(100i64, &[100i64, 101][..]), (500, &[500][..])] {
            let node =
                TimeSeriesNode::create(dir.join(start.to_string()), Timestamp(start), 8).unwrap();
            for ts in stamps {
                node.data.write(&ts.to_le_bytes()).unwrap();
                node.index.write(&Timestamp(*ts).to_le_bytes()).unwrap();
            }
        }
        let series = TimeSeries::open(dir).unwrap();
        series.seal_rolled_nodes().unwrap();

        let src_dir = dir.join("peer");
        let src = TimeSeriesNode::create(src_dir.join("0"), Timestamp(0), 8).unwrap();
        for i in 0..15i64 {
            let ts = i * 10;
            src.data.write(&ts.to_le_bytes()).unwrap();
            src.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        (series, src)
    }

    /// Stage `src` in full, as a fetch would.
    fn stage(dir: &Path, src: &TimeSeriesNode, seal: &SealRecord) -> crate::store::NodeStaging {
        use crate::store::{NodeFile, NodeStaging};
        let staging = NodeStaging::create(dir, seal).unwrap();
        staging
            .append_file(NodeFile::Index, src.index.data())
            .unwrap();
        staging
            .append_file(NodeFile::Data, src.data.data())
            .unwrap();
        staging
    }

    #[test]
    fn split_remote_seal_installs_every_piece() {
        let dir = tempfile::tempdir().unwrap();
        let (series, src) = split_fixture(dir.path());
        let peer = SealRecord::compute(&src).unwrap();
        assert_eq!(series.merge_remote_spans([peer]).unwrap(), 1);

        // Local data in the middle leaves two pieces of one seal.
        let manifest = series.manifest();
        let pieces: Vec<_> = manifest
            .spans
            .iter()
            .filter(|s| s.state == SpanState::RemoteOnly)
            .map(|s| s.cover())
            .collect();
        assert_eq!(
            pieces,
            [Timestamp(0)..Timestamp(100), Timestamp(102)..Timestamp(141)]
        );

        // Claiming either piece claims both; one download settles them.
        let span = series.begin_fetch(Timestamp(102)).unwrap();
        assert_eq!(span.seal, peer);
        assert!(series.begin_fetch(Timestamp(0)).is_none());
        series
            .install_fetched(stage(dir.path(), &src, &peer), &peer)
            .unwrap();

        let manifest = series.manifest();
        assert!(
            manifest
                .spans
                .iter()
                .all(|s| s.state == SpanState::Resident)
        );
        assert_eq!(&*manifest.shadowed, &[peer]);
        let front = manifest.span(Timestamp(0)).unwrap();
        assert_eq!(front.source, SpanSource::Mirrored);
        assert_eq!(front.seal.end_ts, Timestamp(90));
        let back = manifest.span(Timestamp(110)).unwrap();
        assert_eq!(back.source, SpanSource::Mirrored);
        assert_eq!(back.seal.end_ts, Timestamp(140));
        assert_eq!(back.seal.count, 4);

        let stamps = range_stamps(&series, 0, 600);
        let mut expected: Vec<i64> = (0..15).map(|i| i * 10).filter(|t| *t != 100).collect();
        expected.extend([100, 101, 500]);
        expected.sort_unstable();
        assert_eq!(stamps, expected);
        assert_eq!(
            series.get(Timestamp(120)).unwrap().data(),
            120u64.to_le_bytes()
        );
        let mut gaps = GapVec::new();
        assert_eq!(
            series.coverage(Timestamp(0)..Timestamp(501), &mut gaps),
            Coverage::Complete
        );

        // Re-merging the settled seal adds nothing.
        assert_eq!(series.merge_remote_spans([peer]).unwrap(), 0);
    }

    #[test]
    fn whole_remote_seal_installs_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = TimeSeriesNode::create(src_dir.path().join("0"), Timestamp(0), 8).unwrap();
        for ts in [0i64, 10, 20] {
            src.data.write(&ts.to_le_bytes()).unwrap();
            src.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        let peer = SealRecord::compute(&src).unwrap();
        series.merge_remote_spans([peer]).unwrap();
        series.begin_fetch(Timestamp(0)).unwrap();
        series
            .install_fetched(stage(dir.path(), &src, &peer), &peer)
            .unwrap();
        let manifest = series.manifest();
        let span = manifest.span(Timestamp(0)).unwrap();
        assert_eq!(span.state, SpanState::Resident);
        assert_eq!(span.source, SpanSource::RemoteFetch);
        assert_eq!(span.seal, peer);
        assert!(manifest.shadowed.is_empty());
    }

    fn head(start: i64, end: i64) -> UnsealedHead {
        UnsealedHead {
            start_ts: Timestamp(start),
            end_ts: Timestamp(end),
            count: (end - start + 1) as u64,
            element_size: 8,
        }
    }

    fn head_pieces(series: &TimeSeries) -> Vec<(i64, i64)> {
        series
            .manifest()
            .spans
            .iter()
            .filter(|s| s.source == SpanSource::PeerHead)
            .map(|s| (s.cover_start.0, s.cover_end.0))
            .collect()
    }

    /// A peer's head beside a mirror's live data: it answers for what the
    /// mirror missed (before it connected, and while it was disconnected)
    /// but stops below the live head, never backs a purge of the mirror's
    /// own copy, and is never persisted.
    #[test]
    fn a_peer_head_fills_around_mirrored_data() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        series.enter_mirror_run(MirrorRun::next());
        for ts in (800..=950).step_by(10) {
            writer.push_buf(Timestamp(ts), &[0; 8]).unwrap();
        }
        // A reconnect: the old run's node seals, the new run's is live.
        series.enter_mirror_run(MirrorRun::next());
        writer.push_buf(Timestamp(2000), &[0; 8]).unwrap();
        assert!(series.seal_rolled_nodes().unwrap());

        series.sync_remote(Vec::new(), Some(head(0, 2100))).unwrap();
        assert_eq!(head_pieces(&series), [(0, 799), (951, 1999)]);
        let manifest = series.manifest();
        let piece = manifest.span(Timestamp(0)).unwrap();
        assert_eq!(piece.seal.end_ts, Timestamp(1999));
        assert_eq!(piece.seal.count, 2000);
        assert!(series.begin_fetch(Timestamp(0)).is_none());
        // Only the peer's head has the mirrored samples: they stay.
        assert!(!series.purge_span(Timestamp(800)).unwrap());

        // The head grows; what is above the live head is still left out.
        series.sync_remote(Vec::new(), Some(head(0, 2500))).unwrap();
        assert_eq!(head_pieces(&series), [(0, 799), (951, 1999)]);

        // A durable change alongside a head persists without it.
        series
            .sync_remote(vec![remote_seal(3000, 3100)], Some(head(0, 2500)))
            .unwrap();
        drop(writer);
        let reopened = TimeSeries::open(dir.path()).unwrap();
        assert!(head_pieces(&reopened).is_empty());
        assert!(reopened.manifest().span(Timestamp(3000)).is_some());
        drop(reopened);

        // The peer seals what its head held: a sealed span takes over, and
        // now backs the mirrored copy.
        series
            .sync_remote(
                vec![remote_seal(0, 1999), remote_seal(3000, 3100)],
                Some(head(2000, 2600)),
            )
            .unwrap();
        assert!(head_pieces(&series).is_empty());
        assert_eq!(
            series.manifest().span(Timestamp(0)).unwrap().source,
            SpanSource::RemoteFetch
        );
        assert!(series.purge_span(Timestamp(800)).unwrap());

        // A listing without a head drops the old one.
        let other = TimeSeries::create(dir.path().join("other")).unwrap();
        other.sync_remote(Vec::new(), Some(head(0, 10))).unwrap();
        assert_eq!(head_pieces(&other), [(0, 10)]);
        other.sync_remote(Vec::new(), None).unwrap();
        assert!(other.manifest().spans.is_empty());
    }

    /// A listing is a snapshot taken before the stream starts: the peer's
    /// head reaches on to the first sample of the mirror run, whether the
    /// run starts before the listing lands or after, so nothing the peer
    /// wrote in between is left without a span.
    #[test]
    fn a_peer_head_meets_the_mirror_run() {
        let dir = tempfile::tempdir().unwrap();

        // Listed first, the run starts after.
        let series = TimeSeries::create(dir.path().join("after")).unwrap();
        let mut writer = series.writer().unwrap();
        series.sync_remote(Vec::new(), Some(head(0, 100))).unwrap();
        assert_eq!(head_pieces(&series), [(0, 100)]);
        series.enter_mirror_run(MirrorRun::next());
        writer.push_buf(Timestamp(250), &[0; 8]).unwrap();
        series.meet_mirror_run().unwrap();
        assert_eq!(head_pieces(&series), [(0, 249)]);
        // The peer's count stands for the stretch it listed.
        assert_eq!(
            series.manifest().span(Timestamp(0)).unwrap().seal.count,
            101
        );

        // The run started first, the listing lands after.
        let series = TimeSeries::create(dir.path().join("before")).unwrap();
        let mut writer = series.writer().unwrap();
        series.enter_mirror_run(MirrorRun::next());
        writer.push_buf(Timestamp(250), &[0; 8]).unwrap();
        series.sync_remote(Vec::new(), Some(head(0, 100))).unwrap();
        assert_eq!(head_pieces(&series), [(0, 249)]);

        // A reconnect carries the last listing to the new run's start.
        series.enter_mirror_run(MirrorRun::next());
        writer.push_buf(Timestamp(400), &[0; 8]).unwrap();
        assert!(series.seal_rolled_nodes().unwrap());
        series.meet_mirror_run().unwrap();
        assert_eq!(head_pieces(&series), [(0, 249), (251, 399)]);
    }

    #[test]
    fn mirrored_ingest_seals_as_mirrored() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        series.enter_mirror_run(MirrorRun::next());
        for ts in [0i64, 1, 2] {
            writer.push_buf(Timestamp(ts), &[0; 8]).unwrap();
        }
        // A reconnect rolls the head, so no node spans the disconnect.
        series.enter_mirror_run(MirrorRun::next());
        writer.push_buf(Timestamp(10), &[0; 8]).unwrap();
        assert!(series.seal_rolled_nodes().unwrap());
        let manifest = series.manifest();
        let sealed = manifest.span(Timestamp(0)).unwrap();
        assert_eq!(sealed.source, SpanSource::Mirrored);
        assert_eq!(sealed.seal.end_ts, Timestamp(2));
        assert!(!sealed.acked);

        // Data computed locally into the same db stays local.
        let other = TimeSeries::create(dir.path().join("other")).unwrap();
        let seal = other
            .install_samples(8, [(Timestamp(5), &[0u8; 8][..])], SpanSource::LocalIngest)
            .unwrap()
            .unwrap();
        assert_eq!(
            other.manifest().span(seal.start_ts).unwrap().source,
            SpanSource::LocalIngest
        );
        assert!(!other.is_mirrored());
    }

    #[test]
    fn version_2_manifest_migrates_on_open() {
        let dir = tempfile::tempdir().unwrap();
        // A sealed local node at [100, 101] behind a live head at 500.
        let (series, src) = split_fixture(dir.path());
        let local = series.manifest().spans[0];
        drop(series);
        // Version 2 recorded the peer's [0, 140] seal tail-trimmed at the
        // local data, with no field for its front.
        #[derive(serde::Serialize)]
        struct NodeSpanV2 {
            seal: SealRecord,
            state: SpanState,
            source: SpanSource,
            acked: bool,
            cover_end: Timestamp,
        }
        let peer = SealRecord::compute(&src).unwrap();
        let v2 = |span: NodeSpan| NodeSpanV2 {
            seal: span.seal,
            state: span.state,
            source: span.source,
            acked: span.acked,
            cover_end: span.cover_end,
        };
        let mut bytes = postcard::to_allocvec(&2u32).unwrap();
        bytes.extend(
            postcard::to_allocvec(&vec![
                v2(NodeSpan {
                    cover_end: Timestamp(99),
                    ..NodeSpan::whole(peer, SpanState::RemoteOnly, SpanSource::RemoteFetch, true)
                }),
                v2(local),
            ])
            .unwrap(),
        );
        crate::seal::atomic_write(dir.path(), crate::manifest::MANIFEST_FILE, &bytes).unwrap();

        let series = TimeSeries::open(dir.path()).unwrap();
        let manifest = series.manifest();
        // The trimmed span reads as a front piece, and the tail v2 could
        // not express is fetchable now.
        let covers: Vec<_> = manifest
            .spans
            .iter()
            .filter(|s| s.seal == peer)
            .map(|s| s.cover())
            .collect();
        assert_eq!(
            covers,
            [Timestamp(0)..Timestamp(100), Timestamp(102)..Timestamp(141)]
        );
        assert_eq!(
            manifest.span(Timestamp(100)).unwrap().state,
            SpanState::Resident
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
            series.merge_remote_spans([remote_seal(0, 99)]).unwrap();
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
        assert!(series.seal_rolled_nodes().unwrap());

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
        // Idempotent on a second pass.
        assert!(!series.seal_rolled_nodes().unwrap());

        // A reopen rebuilds the same view from seal files + manifest.
        drop(series);
        let series = TimeSeries::open(dir.path()).unwrap();
        let manifest = series.manifest();
        let starts: Vec<i64> = manifest.spans.iter().map(|s| s.seal.start_ts.0).collect();
        assert_eq!(starts, vec![10, 20]);
    }

    /// A range spanning several nodes must yield every overlapping node,
    /// newest first, and nothing older than the range. Regression: the
    /// iterator used to seed from the range's *oldest* node and walk the
    /// stack's newer→older links, so everything in newer nodes was
    /// silently dropped.
    #[test]
    fn get_range_spans_multiple_nodes() {
        let dir = tempfile::tempdir().unwrap();
        for start in [0i64, 100, 200] {
            let node =
                TimeSeriesNode::create(dir.path().join(start.to_string()), Timestamp(start), 8)
                    .unwrap();
            for i in 0..10i64 {
                let ts = start + i * 10;
                node.data.write(&ts.to_le_bytes()).unwrap();
                node.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
            }
        }
        let series = TimeSeries::open(dir.path()).unwrap();

        let slice = series.get_range(Timestamp(50)..Timestamp(250)).unwrap();
        let chunks: Vec<Vec<i64>> = slice
            .as_iter()
            .map(|ns| ns.timestamps().iter().map(|t| t.0).collect())
            .collect();
        // Newest first, clamped to the range at both ends (the end bound
        // is inclusive on an exact timestamp match).
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], vec![200, 210, 220, 230, 240, 250]);
        assert_eq!(chunks[1], (100..200).step_by(10).collect::<Vec<_>>());
        assert_eq!(chunks[2], vec![50, 60, 70, 80, 90]);
        assert_eq!(slice.len(), 21);

        // Single-node ranges still clamp to both ends.
        let slice = series.get_range(Timestamp(110)..Timestamp(150)).unwrap();
        let chunks: Vec<Vec<i64>> = slice
            .as_iter()
            .map(|ns| ns.timestamps().iter().map(|t| t.0).collect())
            .collect();
        assert_eq!(chunks, vec![vec![110, 120, 130, 140, 150]]);
    }

    #[test]
    fn max_node_age_rolls_and_seals_old_heads() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        series.set_max_node_age(Duration::from_micros(100));
        let mut writer = series.writer().unwrap();

        writer.push_buf(Timestamp(0), &0u64.to_le_bytes()).unwrap();
        writer.push_buf(Timestamp(50), &1u64.to_le_bytes()).unwrap();
        assert_eq!(series.list.iter().count(), 1);

        // A write more than the age past the head's first sample rolls,
        // and the rolled node seals like any full one.
        writer
            .push_buf(Timestamp(150), &2u64.to_le_bytes())
            .unwrap();
        assert_eq!(series.list.iter().count(), 2);
        assert!(series.seal_rolled_nodes().unwrap());
        let manifest = series.manifest();
        assert_eq!(manifest.spans.len(), 1);
        assert_eq!(manifest.spans[0].seal.start_ts, Timestamp(0));
        assert_eq!(manifest.spans[0].seal.end_ts, Timestamp(50));
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
        series.seal_rolled_nodes().unwrap();
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
                .install_node(staging, &seal, SpanSource::LocalIngest)
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
        series.seal_rolled_nodes().unwrap();
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

    #[test]
    fn uncovered_reports_never_written_stretches() {
        let dir = tempfile::tempdir().unwrap();
        let node = TimeSeriesNode::create(dir.path().join("100"), Timestamp(100), 8).unwrap();
        for ts in [100i64, 101] {
            node.data.write(&ts.to_le_bytes()).unwrap();
            node.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        let series = TimeSeries::open(dir.path()).unwrap();
        series.merge_remote_spans([remote_seal(200, 299)]).unwrap();

        let mut out = RangeVec::new();
        series.uncovered(Timestamp(0)..Timestamp(400), &mut out);
        assert_eq!(
            out.as_slice(),
            &[
                Timestamp(0)..Timestamp(100),
                Timestamp(102)..Timestamp(200),
                Timestamp(300)..Timestamp(400),
            ]
        );

        // Coverage sees only the stretch that exists but is elsewhere.
        let mut gaps = GapVec::new();
        assert_eq!(
            series.coverage(Timestamp(0)..Timestamp(400), &mut gaps),
            Coverage::Partial
        );
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].range, Timestamp(200)..Timestamp(300));
    }

    fn backfill(range: std::ops::Range<i64>, step: i64) -> Vec<(Timestamp, [u8; 8])> {
        range
            .step_by(step as usize)
            .map(|ts| (Timestamp(ts), ts.to_le_bytes()))
            .collect()
    }

    fn samples(batch: &[(Timestamp, [u8; 8])]) -> impl Iterator<Item = (Timestamp, &[u8])> {
        batch.iter().map(|(ts, bytes)| (*ts, bytes.as_slice()))
    }

    fn staging_dirs(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "fetching")
            })
            .count()
    }

    #[test]
    fn install_samples_lands_behind_a_live_head() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        for ts in [500i64, 501] {
            writer.push_buf(Timestamp(ts), &ts.to_le_bytes()).unwrap();
        }

        let batch = backfill(0..100, 10);
        let seal = series
            .install_samples(8, samples(&batch), SpanSource::LocalIngest)
            .unwrap()
            .unwrap();
        assert_eq!(seal.start_ts, Timestamp(0));
        assert_eq!(seal.end_ts, Timestamp(90));

        // The live head is untouched and still the newest data.
        assert_eq!(series.latest().unwrap().timestamp(), Timestamp(501));
        assert_eq!(series.start_timestamp().unwrap(), Timestamp(0));
        let manifest = series.manifest();
        assert_eq!(
            manifest.span(Timestamp(0)).unwrap().state,
            SpanState::Resident
        );

        let mut out = RangeVec::new();
        series.uncovered(Timestamp(0)..Timestamp(502), &mut out);
        assert_eq!(out.as_slice(), &[Timestamp(91)..Timestamp(500)]);
        // Past the newest sample there is no history either, so a range
        // running into the future reports that stretch too.
        out.clear();
        series.uncovered(Timestamp(0)..Timestamp(600), &mut out);
        assert_eq!(
            out.as_slice(),
            &[
                Timestamp(91)..Timestamp(500),
                Timestamp(502)..Timestamp(600),
            ]
        );

        writer
            .push_buf(Timestamp(502), &502i64.to_le_bytes())
            .unwrap();
        assert_eq!(series.latest().unwrap().timestamp(), Timestamp(502));
        assert_eq!(series.list.iter().count(), 2);
    }

    #[test]
    fn install_samples_into_an_empty_series_then_live_writes_roll() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let batch = backfill(0..40, 10);
        series
            .install_samples(8, samples(&batch), SpanSource::LocalIngest)
            .unwrap()
            .unwrap();

        let mut writer = series.writer().unwrap();
        writer
            .push_buf(Timestamp(100), &100i64.to_le_bytes())
            .unwrap();

        // The installed node is sealed, so the writer rolls above it
        // rather than appending into it.
        assert_eq!(series.list.iter().count(), 2);
        let manifest = series.manifest();
        assert_eq!(manifest.spans.len(), 1);
        assert_eq!(manifest.spans[0].seal.start_ts, Timestamp(0));
        assert_eq!(manifest.spans[0].seal.end_ts, Timestamp(30));
        assert!(manifest.span(Timestamp(100)).is_none());
        assert_eq!(series.latest().unwrap().timestamp(), Timestamp(100));
    }

    #[test]
    fn install_samples_rejects_out_of_order_and_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        writer
            .push_buf(Timestamp(50), &50i64.to_le_bytes())
            .unwrap();

        let descending = [
            (Timestamp(10), 10i64.to_le_bytes()),
            (Timestamp(5), 5i64.to_le_bytes()),
        ];
        assert!(
            series
                .install_samples(8, samples(&descending), SpanSource::LocalIngest)
                .is_err()
        );

        let overlapping = backfill(40..70, 10);
        assert!(
            series
                .install_samples(8, samples(&overlapping), SpanSource::LocalIngest)
                .is_err()
        );

        assert_eq!(series.latest().unwrap().timestamp(), Timestamp(50));
        assert!(series.manifest().spans.is_empty());
        assert_eq!(staging_dirs(dir.path()), 0);
    }

    #[test]
    fn install_samples_of_an_empty_batch_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let empty: [(Timestamp, [u8; 8]); 0] = [];
        assert!(
            series
                .install_samples(8, samples(&empty), SpanSource::LocalIngest)
                .unwrap()
                .is_none()
        );
        assert_eq!(staging_dirs(dir.path()), 0);
    }

    #[test]
    fn install_node_wakes_seal_waiter() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let waiter = series.seal_waiter();
        let mut wait = std::pin::pin!(waiter.wait());
        // Register the way the LoD engine does before it sleeps.
        assert!(wait.as_mut().subscribe().is_pending());

        let batch = backfill(0..40, 10);
        series
            .install_samples(8, samples(&batch), SpanSource::LocalIngest)
            .unwrap()
            .unwrap();
        assert!(wait.as_mut().subscribe().is_ready());
    }

    const IMAGE_SIZE: usize = 2 * 1024 * 1024;

    fn push_patterns(writer: &mut TimerSeriesWriter, range: Range<i64>, size: usize) {
        let mut buf = vec![0u8; size];
        for ts in range {
            buf.fill(ts as u8);
            buf[0] = !(ts as u8);
            writer.push_buf(Timestamp(ts), &buf).unwrap();
        }
    }

    fn assert_patterns(series: &TimeSeries, range: Range<i64>, size: usize) {
        for ts in range {
            let sample = series.get(Timestamp(ts)).unwrap();
            let data = sample.data();
            assert_eq!(data.len(), size);
            assert_eq!(data[0], !(ts as u8), "ts {ts}");
            assert!(data[1..].iter().all(|b| *b == ts as u8), "ts {ts}");
        }
    }

    fn node_starts(series: &TimeSeries) -> Vec<i64> {
        let mut starts: Vec<i64> = series
            .iter_node_slices()
            .map(|n| n.full_timestamps()[0].0)
            .collect();
        starts.sort();
        starts
    }

    fn data_file_len(dir: &Path, start: i64) -> u64 {
        std::fs::metadata(dir.join(start.to_string()).join("data"))
            .unwrap()
            .len()
    }

    #[test]
    fn node_data_size_scales_past_node_size() {
        assert_eq!(node_data_size(8), NODE_SIZE);
        assert_eq!(node_data_size(0), NODE_SIZE);
        assert_eq!(node_data_size(IMAGE_SIZE as u64), 128 * 1024 * 1024 + 64);
        assert_eq!(node_data_size(u64::MAX), u64::MAX);
    }

    #[test]
    fn large_elements_roll_every_min_elements() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        push_patterns(&mut writer, 0..100, IMAGE_SIZE);
        assert_eq!(node_starts(&series), vec![0, NODE_MIN_ELEMENTS as i64]);
        assert_eq!(
            data_file_len(dir.path(), 0),
            node_data_size(IMAGE_SIZE as u64) + 1
        );
        assert_patterns(&series, 0..100, IMAGE_SIZE);
    }

    #[test]
    fn node_sized_before_scaling_opens_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let node_dir = dir.path().join("0");
        std::fs::create_dir_all(&node_dir).unwrap();
        let old = TimeSeriesNode::from_logs(
            AppendLog::with_size(NODE_SIZE, node_dir.join("index"), Timestamp(0)).unwrap(),
            AppendLog::with_size(NODE_SIZE, node_dir.join("data"), IMAGE_SIZE as u64).unwrap(),
        );
        let mut buf = vec![0u8; IMAGE_SIZE];
        for ts in 0..4i64 {
            buf.fill(ts as u8);
            buf[0] = !(ts as u8);
            old.data.write(&buf).unwrap();
            old.index.write(&Timestamp(ts).to_le_bytes()).unwrap();
        }
        drop(old);

        let series = TimeSeries::open(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        push_patterns(&mut writer, 4..20, IMAGE_SIZE);
        // 32 MiB less the header holds 15 samples; the rest roll to a scaled node.
        assert_eq!(node_starts(&series), vec![0, 15]);
        assert_eq!(data_file_len(dir.path(), 0), NODE_SIZE + 1);
        assert_eq!(
            data_file_len(dir.path(), 15),
            node_data_size(IMAGE_SIZE as u64) + 1
        );
        assert_patterns(&series, 0..20, IMAGE_SIZE);
    }

    #[test]
    fn full_index_rolls_before_data() {
        let dir = tempfile::tempdir().unwrap();
        let node_dir = dir.path().join("0");
        std::fs::create_dir_all(&node_dir).unwrap();
        // A 24-byte header plus room for four timestamps.
        let index = AppendLog::with_size(24 + 4 * 8, node_dir.join("index"), Timestamp(0)).unwrap();
        index.write(&Timestamp(0).to_le_bytes()).unwrap();
        let data = AppendLog::with_size(NODE_SIZE, node_dir.join("data"), 1u64).unwrap();
        data.write(&[0]).unwrap();
        drop((index, data));

        let series = TimeSeries::open(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        for ts in 1..10i64 {
            writer.push_buf(Timestamp(ts), &[ts as u8]).unwrap();
        }
        assert_eq!(node_starts(&series), vec![0, 4]);
        let first = TimeSeriesNode::open(&node_dir).unwrap();
        assert_eq!(first.timestamps().len(), 4);
        assert_eq!(first.data.len(), 4);
        for ts in 0..10i64 {
            assert_eq!(series.get(Timestamp(ts)).unwrap().data(), &[ts as u8]);
        }
    }
}
