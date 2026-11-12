use std::{
    ops::Range,
    path::Path,
    sync::{Arc, RwLock},
};

use crate::evict::LastAccess;

use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::{
    Error,
    seal::{SealRecord, atomic_write},
};

pub const MANIFEST_FILE: &str = "manifest";
const MANIFEST_VERSION: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanSource {
    LocalIngest,
    RemoteFetch,
    /// A local copy of data a peer is the authority for, in node boundaries
    /// the peer does not share: what a mirror's live ingest writes, and the
    /// pieces hydration cuts from a peer node around data already resident.
    /// The peer's seals stay in the manifest beside it (as pieces or
    /// [`ComponentManifest::shadowed`]), so dropping a mirrored node only
    /// gives its range back to them.
    Mirrored,
    /// A stretch of a peer's unsealed head (see
    /// [`metor_proto_wkt::UnsealedHead`]): history the peer holds but has
    /// not sealed into a node, so a mirror can read it before the peer
    /// seals. Always [`SpanState::RemoteOnly`], and only range-fetchable:
    /// its seal is a synthetic record (checksum 0) no `FetchNode` serves,
    /// which every listing of the peer replaces, by the next head or by the
    /// sealed nodes the peer cut from it. Fragments read from it stay valid
    /// across that, as they would over any remote span. In memory only:
    /// never persisted, never shadowed, and never a reason to drop local
    /// data, since the peer has not committed to keeping it.
    PeerHead,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanState {
    Resident,
    RemoteOnly,
    Fetching,
}

/// One entry of a component's manifest: a sealed node this side holds, or
/// a stretch of a remote seal it can fetch.
///
/// An entry's identity is `cover_start`. Every entry covers a non-empty
/// range disjoint from every other entry's (resident nodes never overlap,
/// and remote coverage is derived as its seal minus everything else), so
/// the key is unique and sorting by it sorts coverage too. A resident
/// entry covers exactly its seal, so its key is also its node's start.
/// A remote seal partly overlapped by local data is recorded as one entry
/// per uncovered piece, each carrying the whole seal it is fetched by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpan {
    pub seal: SealRecord,
    pub state: SpanState,
    pub source: SpanSource,
    /// A store holds a durable, checksum-verified copy of this span.
    pub acked: bool,
    /// Inclusive start of the span's effective coverage, within the seal.
    pub cover_start: Timestamp,
    /// Inclusive end of the span's effective coverage, within the seal.
    pub cover_end: Timestamp,
}

impl NodeSpan {
    /// A span covering its seal's full range.
    pub fn whole(seal: SealRecord, state: SpanState, source: SpanSource, acked: bool) -> Self {
        Self {
            seal,
            state,
            source,
            acked,
            cover_start: seal.start_ts,
            cover_end: seal.end_ts,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.seal.index_len + self.seal.data_len
    }

    /// The covered range, half-open.
    pub fn cover(&self) -> Range<Timestamp> {
        self.cover_start..Timestamp(self.cover_end.0.saturating_add(1))
    }

    /// Whether the span covers its whole seal, so fetching it installs the
    /// seal's node unchanged.
    pub fn is_whole(&self) -> bool {
        self.cover_start == self.seal.start_ts && self.cover_end == self.seal.end_ts
    }
}

/// A sub-range of a query that exists remotely but not locally — the
/// actionable unit the hydrator fetches. Gaps are reported per span, not
/// merged, since fetches happen per node anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub range: Range<Timestamp>,
    pub state: SpanState,
    /// The span's key (its `cover_start`), which the hydrator claims.
    pub start_ts: Timestamp,
    /// Where the span's seal starts: the seal, not the key, identifies
    /// what a fetch downloads, and stays put as coverage is re-derived.
    pub seal_start: Timestamp,
}

pub type GapVec = SmallVec<[Gap; 4]>;

pub type RangeVec = SmallVec<[Range<Timestamp>; 4]>;

/// How much of a queried range is locally answerable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// No known remote-only data in the range.
    Complete,
    /// Some local data plus at least one gap.
    Partial,
    /// Only remote data overlaps the range.
    Empty,
}

/// An immutable snapshot of every sealed span a component knows about,
/// sorted by `cover_start`. Mutators clone-and-swap whole snapshots
/// through [`ManifestCell`]; readers hold a snapshot for as long as they
/// like without blocking anyone.
#[derive(Debug, Default, Clone)]
pub struct ComponentManifest {
    pub generation: u64,
    pub spans: Box<[NodeSpan]>,
    /// Remote seals whose whole range local data already answers, sorted
    /// by start. They cover nothing, but are kept so that when the local
    /// data shadowing them is dropped, their range becomes fetchable
    /// again (see `TimeSeries::purge_span`).
    pub shadowed: Box<[SealRecord]>,
    /// Stretches of remote seals a fetch already reconciled, sorted by
    /// seal. Whatever a seal holds there is resident (or was nothing at
    /// all, such as the gap between two mirrored nodes), so derivation
    /// stops offering them until local data inside is dropped.
    pub settled: Box<[Settled]>,
    /// Stretches of remote-only spans held as fragments, sorted by start
    /// and disjoint. In memory only.
    pub fragments: Box<[Fragment]>,
}

/// A stretch of remote-only history this side read with a range fetch
/// (`GetTimeSeries`), rather than as a whole sealed node.
///
/// It claims exactly the range the peer vouched for, `start..=end`, which
/// can hold fewer samples than its edges suggest or none at all: a stretch
/// checked and found empty is as answered as one full of samples. The
/// samples it does hold live in a node of their own, linked into the
/// component's node list like any other, so every read path sees them.
///
/// Fragments are a cache. They are never sealed into the manifest's spans,
/// never acked, offloaded or persisted (their node directories are deleted
/// on open), and the remote spans under them stay remote-only: coverage
/// just stops reporting the claimed stretch as a gap. Eviction drops them
/// like any other hydrated data, and a whole node installed over one
/// replaces it.
#[derive(Debug, Clone)]
pub struct Fragment {
    /// Inclusive start of the claimed stretch.
    pub start: Timestamp,
    /// Inclusive end of the claimed stretch.
    pub end: Timestamp,
    /// The seal of the fragment's node, or `None` when the stretch held
    /// no samples.
    pub node: Option<SealRecord>,
    /// Where the node lives, so dropping the fragment can delete it.
    pub(crate) dir: Option<Arc<Path>>,
    /// Shared with the node when there is one, so a read that touches the
    /// node keeps the fragment, and an empty fragment can still go idle.
    pub(crate) access: Arc<LastAccess>,
}

impl Fragment {
    pub fn claim(&self) -> Range<Timestamp> {
        self.start..Timestamp(self.end.0.saturating_add(1))
    }

    pub fn bytes(&self) -> u64 {
        self.node.map_or(0, |seal| seal.index_len + seal.data_len)
    }

    pub(crate) fn overlaps(&self, start: i64, end: i64) -> bool {
        self.start.0 <= end && start <= self.end.0
    }

    /// The fragment as eviction sees it: a resident, unacked span of
    /// fetched data covering its claim.
    pub fn as_span(&self) -> NodeSpan {
        let seal = self.node.unwrap_or(SealRecord {
            start_ts: self.start,
            end_ts: self.end,
            count: 0,
            index_len: 0,
            data_len: 0,
            checksum: 0,
            element_size: 0,
        });
        NodeSpan {
            cover_start: self.start,
            cover_end: self.end,
            ..NodeSpan::whole(seal, SpanState::Resident, SpanSource::RemoteFetch, false)
        }
    }
}

/// A stretch of a remote seal that needs no fetch: everything the seal
/// holds in `start..=end` is resident. Recorded when a fetch of the seal
/// lands, and cut back when local data in the stretch goes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settled {
    pub seal_start: Timestamp,
    pub checksum: u64,
    pub start: Timestamp,
    pub end: Timestamp,
}

impl Settled {
    pub fn of(&self, seal: &SealRecord) -> bool {
        self.seal_start == seal.start_ts && self.checksum == seal.checksum
    }
}

impl ComponentManifest {
    /// The span keyed by `cover_start`.
    pub fn span(&self, cover_start: Timestamp) -> Option<&NodeSpan> {
        self.spans
            .binary_search_by_key(&cover_start.0, |s| s.cover_start.0)
            .ok()
            .map(|i| &self.spans[i])
    }

    /// Append the non-resident overlaps of `range` to `out`.
    ///
    /// Coverage ranges are disjoint and sorted, so the scan starts at the
    /// first overlapping span and stops past the range — components with
    /// years of sealed spans pay O(log n + overlapped), not O(n), and this
    /// runs per trace per frame.
    ///
    /// Stretches [`Self::fragments`] hold are not gaps, so a span partly
    /// read by range fetches can report several.
    pub fn gaps(&self, range: &Range<Timestamp>, out: &mut GapVec) {
        for span in self.overlapping(range) {
            if span.state == SpanState::Resident {
                continue;
            }
            let start = range.start.0.max(span.cover_start.0);
            let end = range.end.0.min(span.cover_end.0.saturating_add(1));
            let mut push = |start: i64, end: i64| {
                out.push(Gap {
                    range: Timestamp(start)..Timestamp(end),
                    state: span.state,
                    start_ts: span.cover_start,
                    seal_start: span.seal.start_ts,
                })
            };
            let mut cursor = start;
            for fragment in self.fragments_in(start, end) {
                if fragment.start.0 > cursor {
                    push(cursor, fragment.start.0);
                }
                cursor = cursor.max(fragment.end.0.saturating_add(1));
            }
            if end > cursor {
                push(cursor, end);
            }
        }
    }

    /// The fragments overlapping `start..end`, in order.
    pub(crate) fn fragments_in(&self, start: i64, end: i64) -> impl Iterator<Item = &Fragment> {
        let lo = self.fragments.partition_point(|f| f.end.0 < start);
        self.fragments[lo..]
            .iter()
            .take_while(move |f| f.start.0 < end)
    }

    /// Estimate how many samples the sealed spans hold inside `range`,
    /// prorating the two boundary spans linearly. The live head is not in
    /// the manifest, so callers add its overlap separately. This is the
    /// signal plots use to choose raw rendering versus the envelope — it
    /// needs no data access at all.
    pub fn count_in(&self, range: &Range<Timestamp>) -> u64 {
        let mut total = 0u64;
        for span in self.overlapping(range) {
            let ov_start = range.start.0.max(span.cover_start.0);
            let ov_end = (range.end.0 - 1).min(span.cover_end.0);
            if ov_end < ov_start {
                continue;
            }
            // seal.count covers the seal's whole range even when coverage
            // is trimmed, so prorate against the seal.
            let seal_len = (span.seal.end_ts.0 - span.seal.start_ts.0) as u128 + 1;
            let ov_len = (ov_end - ov_start) as u128 + 1;
            total += ((span.seal.count as u128 * ov_len) / seal_len) as u64;
        }
        total
    }

    fn overlapping(&self, range: &Range<Timestamp>) -> impl Iterator<Item = &NodeSpan> {
        let lo = self
            .spans
            .partition_point(|s| s.cover_end.0 < range.start.0);
        let end = range.end.0;
        self.spans[lo..]
            .iter()
            .take_while(move |s| s.cover_start.0 < end)
    }

    pub fn read_from(dir: &Path) -> Result<Option<Self>, Error> {
        let buf = match std::fs::read(dir.join(MANIFEST_FILE)) {
            Ok(buf) => buf,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let (version, rest) = postcard::take_from_bytes::<u32>(&buf)?;
        let (mut spans, mut shadowed, mut settled) = match version {
            MANIFEST_VERSION => {
                let file = postcard::from_bytes::<ManifestBody>(rest)?;
                (file.spans, file.shadowed, file.settled)
            }
            2 => {
                let spans = postcard::from_bytes::<Vec<NodeSpanV2>>(rest)?;
                (
                    spans.into_iter().map(NodeSpan::from).collect(),
                    Vec::new(),
                    Vec::new(),
                )
            }
            version => return Err(Error::UnsupportedManifestVersion(version)),
        };
        for span in &mut spans {
            // In-flight fetches do not survive a restart.
            if span.state == SpanState::Fetching {
                span.state = SpanState::RemoteOnly;
            }
        }
        spans.sort_unstable_by_key(|s| s.cover_start.0);
        shadowed.sort_unstable_by_key(|s| s.start_ts.0);
        settled.sort_unstable_by_key(|s| (s.seal_start.0, s.checksum, s.start.0));
        Ok(Some(Self {
            generation: 0,
            spans: spans.into_boxed_slice(),
            shadowed: shadowed.into_boxed_slice(),
            settled: settled.into_boxed_slice(),
            fragments: Box::default(),
        }))
    }

    pub fn write_to(&self, dir: &Path) -> Result<(), Error> {
        atomic_write(dir, MANIFEST_FILE, &self.snapshot_bytes()?)
    }

    pub(crate) fn snapshot_bytes(&self) -> Result<Vec<u8>, Error> {
        let spans = self
            .spans
            .iter()
            .filter(|span| span.source != SpanSource::PeerHead)
            .map(|span| NodeSpan {
                state: if span.state == SpanState::Fetching {
                    SpanState::RemoteOnly
                } else {
                    span.state
                },
                ..*span
            })
            .collect();
        Ok(postcard::to_allocvec(&ManifestFile {
            version: MANIFEST_VERSION,
            body: ManifestBody {
                spans,
                shadowed: self.shadowed.to_vec(),
                settled: self.settled.to_vec(),
            },
        })?)
    }
}

#[derive(Serialize, Deserialize)]
struct ManifestFile {
    version: u32,
    body: ManifestBody,
}

#[derive(Serialize, Deserialize)]
struct ManifestBody {
    spans: Vec<NodeSpan>,
    shadowed: Vec<SealRecord>,
    settled: Vec<Settled>,
}

/// A version-2 span, which could only trim its coverage's tail.
#[derive(Serialize, Deserialize)]
struct NodeSpanV2 {
    seal: SealRecord,
    state: SpanState,
    source: SpanSource,
    acked: bool,
    cover_end: Timestamp,
}

impl From<NodeSpanV2> for NodeSpan {
    fn from(v2: NodeSpanV2) -> Self {
        Self {
            cover_end: v2.cover_end,
            ..NodeSpan::whole(v2.seal, v2.state, v2.source, v2.acked)
        }
    }
}

/// The swap point between background mutators and readers. Reads clone an
/// `Arc` under a briefly-held lock — no allocation, no waiting on
/// mutators' multi-step work (those serialize on the component's
/// structural mutex, not on this lock).
#[derive(Default)]
pub struct ManifestCell {
    snapshot: RwLock<Arc<ComponentManifest>>,
}

impl ManifestCell {
    pub fn new(manifest: ComponentManifest) -> Self {
        Self {
            snapshot: RwLock::new(Arc::new(manifest)),
        }
    }

    pub fn load(&self) -> Arc<ComponentManifest> {
        self.snapshot.read().unwrap().clone()
    }

    /// Publish a new snapshot, stamping it one generation past the old.
    pub(crate) fn store(&self, mut manifest: ComponentManifest) {
        let mut snapshot = self.snapshot.write().unwrap();
        manifest.generation = snapshot.generation + 1;
        *snapshot = Arc::new(manifest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: i64, end: i64, state: SpanState) -> NodeSpan {
        NodeSpan::whole(
            SealRecord {
                start_ts: Timestamp(start),
                end_ts: Timestamp(end),
                count: (end - start + 1) as u64,
                index_len: 0,
                data_len: 0,
                checksum: 0,
                element_size: 8,
            },
            state,
            SpanSource::LocalIngest,
            false,
        )
    }

    fn trimmed(start: i64, end: i64, cover_end: i64, state: SpanState) -> NodeSpan {
        NodeSpan {
            cover_end: Timestamp(cover_end),
            ..span(start, end, state)
        }
    }

    fn piece(start: i64, end: i64, from: i64, to: i64, state: SpanState) -> NodeSpan {
        NodeSpan {
            cover_start: Timestamp(from),
            cover_end: Timestamp(to),
            ..span(start, end, state)
        }
    }

    fn manifest(spans: Vec<NodeSpan>) -> ComponentManifest {
        ComponentManifest {
            spans: spans.into_boxed_slice(),
            ..Default::default()
        }
    }

    #[test]
    fn gaps_clamp_to_query_range() {
        let m = manifest(vec![
            span(0, 99, SpanState::RemoteOnly),
            span(100, 199, SpanState::Resident),
            span(200, 299, SpanState::Fetching),
        ]);
        let mut gaps = GapVec::new();
        m.gaps(&(Timestamp(50)..Timestamp(250)), &mut gaps);
        assert_eq!(gaps.len(), 2);
        assert_eq!(gaps[0].range, Timestamp(50)..Timestamp(100));
        assert_eq!(gaps[0].state, SpanState::RemoteOnly);
        assert_eq!(gaps[1].range, Timestamp(200)..Timestamp(250));
        assert_eq!(gaps[1].state, SpanState::Fetching);
    }

    #[test]
    fn gaps_outside_range_are_ignored() {
        let m = manifest(vec![span(0, 99, SpanState::RemoteOnly)]);
        let mut gaps = GapVec::new();
        m.gaps(&(Timestamp(100)..Timestamp(200)), &mut gaps);
        assert!(gaps.is_empty());
    }

    /// The partition-point scan must agree with a full linear scan for
    /// every query position relative to span boundaries.
    #[test]
    fn gaps_match_linear_reference() {
        let m = manifest(vec![
            span(0, 9, SpanState::RemoteOnly),
            span(20, 29, SpanState::Resident),
            trimmed(30, 49, 39, SpanState::RemoteOnly),
            span(50, 59, SpanState::Fetching),
            // One seal split around resident data at [70, 89].
            piece(60, 99, 60, 69, SpanState::RemoteOnly),
            span(70, 89, SpanState::Resident),
            piece(60, 99, 90, 99, SpanState::RemoteOnly),
            span(100, 199, SpanState::RemoteOnly),
        ]);
        for start in -5..210 {
            for len in [1, 7, 35, 120, 250] {
                let range = Timestamp(start)..Timestamp(start + len);
                let mut fast = GapVec::new();
                m.gaps(&range, &mut fast);

                let mut reference = GapVec::new();
                for span in &m.spans {
                    if span.state == SpanState::Resident {
                        continue;
                    }
                    let s = range.start.0.max(span.cover_start.0);
                    let e = range.end.0.min(span.cover_end.0 + 1);
                    if e > s {
                        reference.push(Gap {
                            range: Timestamp(s)..Timestamp(e),
                            state: span.state,
                            start_ts: span.cover_start,
                            seal_start: span.seal.start_ts,
                        });
                    }
                }
                assert_eq!(fast, reference, "range {range:?}");
            }
        }
    }

    #[test]
    fn trimmed_span_gap_stops_at_cover_end() {
        let m = manifest(vec![trimmed(0, 99, 49, SpanState::RemoteOnly)]);
        let mut gaps = GapVec::new();
        m.gaps(&(Timestamp(0)..Timestamp(100)), &mut gaps);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].range, Timestamp(0)..Timestamp(50));
        let mut gaps = GapVec::new();
        m.gaps(&(Timestamp(60)..Timestamp(100)), &mut gaps);
        assert!(gaps.is_empty());
    }

    #[test]
    fn count_in_prorates_boundary_spans() {
        // 100 samples over [0, 99] and [100, 199] each.
        let m = manifest(vec![
            span(0, 99, SpanState::Resident),
            span(100, 199, SpanState::RemoteOnly),
        ]);
        assert_eq!(m.count_in(&(Timestamp(0)..Timestamp(200))), 200);
        assert_eq!(m.count_in(&(Timestamp(50)..Timestamp(150))), 100);
        assert_eq!(m.count_in(&(Timestamp(0)..Timestamp(1))), 1);
        assert_eq!(m.count_in(&(Timestamp(200)..Timestamp(300))), 0);
        assert_eq!(m.count_in(&(Timestamp(-100)..Timestamp(0))), 0);
    }

    #[test]
    fn front_trimmed_piece_gap_starts_at_cover_start() {
        let m = manifest(vec![piece(0, 99, 50, 99, SpanState::RemoteOnly)]);
        let mut gaps = GapVec::new();
        m.gaps(&(Timestamp(0)..Timestamp(100)), &mut gaps);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].range, Timestamp(50)..Timestamp(100));
        assert_eq!(gaps[0].start_ts, Timestamp(50));
        assert_eq!(m.span(Timestamp(50)).unwrap().seal.start_ts, Timestamp(0));
        assert!(m.span(Timestamp(0)).is_none());
        assert_eq!(m.count_in(&(Timestamp(0)..Timestamp(50))), 0);
        assert_eq!(m.count_in(&(Timestamp(0)..Timestamp(100))), 50);
    }

    #[test]
    fn count_in_prorates_trimmed_cover_against_seal() {
        // 100 samples sealed over [0, 99], coverage trimmed to [0, 49]:
        // the covered half holds ~50 of the seal's samples.
        let m = manifest(vec![trimmed(0, 99, 49, SpanState::RemoteOnly)]);
        assert_eq!(m.count_in(&(Timestamp(0)..Timestamp(100))), 50);
        assert_eq!(m.count_in(&(Timestamp(60)..Timestamp(100))), 0);
    }

    #[test]
    fn persistence_round_trip_normalizes_fetching() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest(vec![
            span(0, 99, SpanState::Fetching),
            span(100, 199, SpanState::Resident),
        ]);
        m.write_to(dir.path()).unwrap();
        let read = ComponentManifest::read_from(dir.path()).unwrap().unwrap();
        assert_eq!(read.spans.len(), 2);
        assert_eq!(read.spans[0].state, SpanState::RemoteOnly);
        assert_eq!(read.spans[1].state, SpanState::Resident);
    }

    #[test]
    fn missing_manifest_reads_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ComponentManifest::read_from(dir.path()).unwrap().is_none());
    }

    #[test]
    fn trimmed_cover_survives_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let m = manifest(vec![trimmed(0, 99, 49, SpanState::RemoteOnly)]);
        m.write_to(dir.path()).unwrap();
        let read = ComponentManifest::read_from(dir.path()).unwrap().unwrap();
        assert_eq!(read.spans[0].cover_end, Timestamp(49));
    }

    #[test]
    fn unknown_manifest_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write(
            dir.path(),
            MANIFEST_FILE,
            &postcard::to_allocvec(&ManifestFile {
                version: 99,
                body: ManifestBody {
                    spans: Vec::new(),
                    shadowed: Vec::new(),
                    settled: Vec::new(),
                },
            })
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            ComponentManifest::read_from(dir.path()),
            Err(Error::UnsupportedManifestVersion(99))
        ));
    }

    #[test]
    fn version_2_migrates_with_cover_start_at_seal_start() {
        let dir = tempfile::tempdir().unwrap();
        let trimmed = trimmed(0, 99, 49, SpanState::Fetching);
        let v2 = |span: NodeSpan| NodeSpanV2 {
            seal: span.seal,
            state: span.state,
            source: span.source,
            acked: span.acked,
            cover_end: span.cover_end,
        };
        let mut bytes = postcard::to_allocvec(&2u32).unwrap();
        bytes.extend(
            postcard::to_allocvec(&vec![v2(span(100, 199, SpanState::Resident)), v2(trimmed)])
                .unwrap(),
        );
        atomic_write(dir.path(), MANIFEST_FILE, &bytes).unwrap();

        let read = ComponentManifest::read_from(dir.path()).unwrap().unwrap();
        assert_eq!(read.spans.len(), 2);
        assert_eq!(read.spans[0].cover_start, Timestamp(0));
        assert_eq!(read.spans[0].cover_end, Timestamp(49));
        assert_eq!(read.spans[0].state, SpanState::RemoteOnly);
        assert!(read.spans[1].is_whole());
        assert!(read.shadowed.is_empty());

        // Writing back upgrades the file.
        read.write_to(dir.path()).unwrap();
        let buf = std::fs::read(dir.path().join(MANIFEST_FILE)).unwrap();
        let (version, _) = postcard::take_from_bytes::<u32>(&buf).unwrap();
        assert_eq!(version, MANIFEST_VERSION);
    }

    #[test]
    fn shadowed_seals_survive_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let shadow = span(0, 99, SpanState::RemoteOnly).seal;
        let m = ComponentManifest {
            spans: vec![span(0, 99, SpanState::Resident)].into_boxed_slice(),
            shadowed: vec![shadow].into_boxed_slice(),
            ..Default::default()
        };
        m.write_to(dir.path()).unwrap();
        let read = ComponentManifest::read_from(dir.path()).unwrap().unwrap();
        assert_eq!(&*read.shadowed, &[shadow]);
    }

    #[test]
    fn cell_bumps_generation() {
        let cell = ManifestCell::default();
        cell.store(manifest(vec![]));
        cell.store(manifest(vec![]));
        assert_eq!(cell.load().generation, 2);
    }
}
