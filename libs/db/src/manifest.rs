use std::{
    ops::Range,
    path::Path,
    sync::{Arc, RwLock},
};

use metor_proto::types::Timestamp;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::{
    Error,
    seal::{SealRecord, atomic_write},
};

pub const MANIFEST_FILE: &str = "manifest";
const MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanSource {
    LocalIngest,
    RemoteFetch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanState {
    Resident,
    RemoteOnly,
    Fetching,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpan {
    pub seal: SealRecord,
    pub state: SpanState,
    pub source: SpanSource,
    /// A store holds a durable, checksum-verified copy of this span.
    pub acked: bool,
    /// Inclusive end of the span's effective coverage. Always within the
    /// seal's range, and equal to `seal.end_ts` except for spans merged
    /// from a peer whose tail overlapped locally-resident data — those are
    /// trimmed rather than dropped, so coverage math sees only the part
    /// this side is missing. Fetches still move the whole node; the trim
    /// is applied at install. The span's identity stays `seal.start_ts`
    /// (coverage never trims the front, which would change identity).
    pub cover_end: Timestamp,
}

impl NodeSpan {
    /// A span covering its seal's full range — every span that does not
    /// come from a trimmed peer merge.
    pub fn whole(seal: SealRecord, state: SpanState, source: SpanSource, acked: bool) -> Self {
        Self {
            seal,
            state,
            source,
            acked,
            cover_end: seal.end_ts,
        }
    }

    pub fn bytes(&self) -> u64 {
        self.seal.index_len + self.seal.data_len
    }
}

/// A sub-range of a query that exists remotely but not locally — the
/// actionable unit the hydrator fetches. Gaps are reported per span, not
/// merged, since fetches happen per node anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub range: Range<Timestamp>,
    pub state: SpanState,
    /// Identifies the span (and so the node) to fetch.
    pub start_ts: Timestamp,
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
/// sorted by start timestamp. Mutators clone-and-swap whole snapshots
/// through [`ManifestCell`]; readers hold a snapshot for as long as they
/// like without blocking anyone.
#[derive(Debug, Default, Clone)]
pub struct ComponentManifest {
    pub generation: u64,
    pub spans: Box<[NodeSpan]>,
}

impl ComponentManifest {
    pub fn span(&self, start_ts: Timestamp) -> Option<&NodeSpan> {
        self.spans
            .binary_search_by_key(&start_ts.0, |s| s.seal.start_ts.0)
            .ok()
            .map(|i| &self.spans[i])
    }

    /// Append the non-resident overlaps of `range` to `out`.
    ///
    /// Coverage ranges are disjoint and sorted, so the scan starts at the
    /// first overlapping span and stops past the range — components with
    /// years of sealed spans pay O(log n + overlapped), not O(n), and this
    /// runs per trace per frame.
    pub fn gaps(&self, range: &Range<Timestamp>, out: &mut GapVec) {
        for span in self.overlapping(range) {
            if span.state == SpanState::Resident {
                continue;
            }
            let start = Timestamp(range.start.0.max(span.seal.start_ts.0));
            let end = Timestamp(range.end.0.min(span.cover_end.0.saturating_add(1)));
            if end.0 > start.0 {
                out.push(Gap {
                    range: start..end,
                    state: span.state,
                    start_ts: span.seal.start_ts,
                });
            }
        }
    }

    /// Estimate how many samples the sealed spans hold inside `range`,
    /// prorating the two boundary spans linearly. The live head is not in
    /// the manifest, so callers add its overlap separately. This is the
    /// signal plots use to choose raw rendering versus the envelope — it
    /// needs no data access at all.
    pub fn count_in(&self, range: &Range<Timestamp>) -> u64 {
        let mut total = 0u64;
        for span in self.overlapping(range) {
            let ov_start = range.start.0.max(span.seal.start_ts.0);
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
            .take_while(move |s| s.seal.start_ts.0 < end)
    }

    pub fn read_from(dir: &Path) -> Result<Option<Self>, Error> {
        let buf = match std::fs::read(dir.join(MANIFEST_FILE)) {
            Ok(buf) => buf,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let (version, rest) = postcard::take_from_bytes::<u32>(&buf)?;
        if version != MANIFEST_VERSION {
            return Err(Error::UnsupportedManifestVersion(version));
        }
        let mut spans = postcard::from_bytes::<Vec<NodeSpan>>(rest)?;
        for span in &mut spans {
            // In-flight fetches do not survive a restart.
            if span.state == SpanState::Fetching {
                span.state = SpanState::RemoteOnly;
            }
        }
        spans.sort_unstable_by_key(|s| s.seal.start_ts.0);
        Ok(Some(Self {
            generation: 0,
            spans: spans.into_boxed_slice(),
        }))
    }

    pub fn write_to(&self, dir: &Path) -> Result<(), Error> {
        atomic_write(dir, MANIFEST_FILE, &self.snapshot_bytes()?)
    }

    pub(crate) fn snapshot_bytes(&self) -> Result<Vec<u8>, Error> {
        let spans = self
            .spans
            .iter()
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
            spans,
        })?)
    }
}

#[derive(Serialize, Deserialize)]
struct ManifestFile {
    version: u32,
    spans: Vec<NodeSpan>,
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

    fn manifest(spans: Vec<NodeSpan>) -> ComponentManifest {
        ComponentManifest {
            generation: 0,
            spans: spans.into_boxed_slice(),
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
                    let s = range.start.0.max(span.seal.start_ts.0);
                    let e = range.end.0.min(span.cover_end.0 + 1);
                    if e > s {
                        reference.push(Gap {
                            range: Timestamp(s)..Timestamp(e),
                            state: span.state,
                            start_ts: span.seal.start_ts,
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
                spans: Vec::new(),
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
    fn cell_bumps_generation() {
        let cell = ManifestCell::default();
        cell.store(manifest(vec![]));
        cell.store(manifest(vec![]));
        assert_eq!(cell.load().generation, 2);
    }
}
