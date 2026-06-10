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
const MANIFEST_VERSION: u32 = 1;

/// Where a span's bytes originally came from. Tiering treats the two very
/// differently: a [`SpanSource::RemoteFetch`] span is a cache entry that
/// can be purged for free, while a [`SpanSource::LocalIngest`] span is the
/// only copy until a store acks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanSource {
    LocalIngest,
    RemoteFetch,
}

/// Residency of a sealed span. The live head node is deliberately absent
/// from the manifest — its extent changes per sample and is answered by
/// the node list itself; spans only exist once a node is sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanState {
    Resident,
    RemoteOnly,
    /// A hydration fetch is in flight. Transient: never persisted (it
    /// normalizes back to [`SpanState::RemoteOnly`] on reload).
    Fetching,
}

/// One sealed node's worth of history, resident or not. The [`SealRecord`]
/// carries the byte-exact identity (lengths + checksum) that fetch,
/// offload, and verification all key on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpan {
    pub seal: SealRecord,
    pub state: SpanState,
    pub source: SpanSource,
    /// A store holds a durable, checksum-verified copy of this span.
    pub acked: bool,
}

impl NodeSpan {
    pub fn bytes(&self) -> u64 {
        self.seal.index_len + self.seal.data_len
    }

    fn overlaps(&self, range: &Range<Timestamp>) -> bool {
        self.seal.start_ts.0 < range.end.0 && range.start.0 <= self.seal.end_ts.0
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
    pub fn gaps(&self, range: &Range<Timestamp>, out: &mut GapVec) {
        for span in &self.spans {
            if span.state == SpanState::Resident || !span.overlaps(range) {
                continue;
            }
            let start = Timestamp(range.start.0.max(span.seal.start_ts.0));
            let end = Timestamp(range.end.0.min(span.seal.end_ts.0.saturating_add(1)));
            out.push(Gap {
                range: start..end,
                state: span.state,
                start_ts: span.seal.start_ts,
            });
        }
    }

    pub fn read_from(dir: &Path) -> Result<Option<Self>, Error> {
        let buf = match std::fs::read(dir.join(MANIFEST_FILE)) {
            Ok(buf) => buf,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let file: ManifestFile = postcard::from_bytes(&buf)?;
        let mut spans = file.spans;
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
        let file = ManifestFile {
            version: MANIFEST_VERSION,
            spans: self.spans.to_vec(),
        };
        atomic_write(dir, MANIFEST_FILE, &postcard::to_allocvec(&file)?)
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
        NodeSpan {
            seal: SealRecord {
                start_ts: Timestamp(start),
                end_ts: Timestamp(end),
                count: (end - start + 1) as u64,
                index_len: 0,
                data_len: 0,
                checksum: 0,
                element_size: 8,
            },
            state,
            source: SpanSource::LocalIngest,
            acked: false,
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
    fn cell_bumps_generation() {
        let cell = ManifestCell::default();
        cell.store(manifest(vec![]));
        cell.store(manifest(vec![]));
        assert_eq!(cell.load().generation, 2);
    }
}
