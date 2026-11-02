//! Deriving what a remote seal still has to offer once local data overlaps
//! it.
//!
//! A remote seal (a peer's sealed node, or a span a store holds) covers its
//! range minus everything this side already answers. Mirrored nodes, cut
//! hydration pieces and other remote seals all claim time, so one seal may
//! leave several pieces or none at all; a seal left with none is kept in
//! [`ComponentManifest::shadowed`] so that dropping the local data over it
//! hands the range back.
//!
//! Local nodes are bounded by their first and last samples, so the gap
//! between two mirrored nodes looks uncovered by range even when the peer
//! holds nothing there. Only a download can tell, so the first fetch of a
//! seal settles it: the stretches it reconciled are recorded in
//! [`ComponentManifest::settled`] and never offered again until local data
//! inside them is dropped.
//!
//! Two of the peer's nodes can share a boundary timestamp (its writer
//! rolled between samples stamped alike). A claim that is a peer seal's own
//! edge therefore leaves that timestamp to the neighbouring seal, so the
//! samples on both sides of the tie stay fetchable.

use std::{ops::RangeInclusive, sync::Arc};

use metor_proto::types::Timestamp;
use smallvec::SmallVec;

use crate::{
    arc_ring::AtomicNode,
    manifest::{ComponentManifest, NodeSpan, Settled, SpanSource, SpanState},
    seal::SealRecord,
};

use super::TimeSeriesNode;

type Runs = SmallVec<[(i64, i64); 2]>;

/// The manifest being rebuilt by one publish.
pub(super) struct Draft {
    pub spans: Vec<NodeSpan>,
    pub shadowed: Vec<SealRecord>,
    pub settled: Vec<Settled>,
}

impl Draft {
    pub fn of(manifest: &ComponentManifest) -> Self {
        Self {
            spans: manifest.spans.to_vec(),
            shadowed: manifest.shadowed.to_vec(),
            settled: manifest.settled.to_vec(),
        }
    }

    pub fn into_manifest(mut self) -> ComponentManifest {
        self.spans.sort_unstable_by_key(|s| s.cover_start.0);
        self.shadowed.sort_unstable_by_key(|s| s.start_ts.0);
        self.settled
            .sort_unstable_by_key(|s| (s.seal_start.0, s.checksum, s.start.0));
        debug_assert!(
            self.spans
                .windows(2)
                .all(|w| w[0].cover_start < w[1].cover_start),
            "manifest keys must be unique: {:?}",
            self.spans
                .iter()
                .map(|s| (s.cover_start.0, s.state))
                .collect::<Vec<_>>()
        );
        ComponentManifest {
            generation: 0,
            spans: self.spans.into_boxed_slice(),
            shadowed: self.shadowed.into_boxed_slice(),
            settled: self.settled.into_boxed_slice(),
        }
    }

    /// Record that everything `seal` holds within `start..=end` is resident.
    pub fn settle(&mut self, seal: &SealRecord, start: Timestamp, end: Timestamp) {
        self.settled.push(Settled {
            seal_start: seal.start_ts,
            checksum: seal.checksum,
            start,
            end,
        });
    }

    /// Local data in `start..=end` went away, so no seal is reconciled
    /// there any more.
    pub fn unsettle(&mut self, start: i64, end: i64) {
        let mut kept = Vec::with_capacity(self.settled.len());
        for settled in self.settled.drain(..) {
            for (a, b) in subtract(settled.start.0, settled.end.0, &[(start, end)]) {
                kept.push(Settled {
                    start: Timestamp(a),
                    end: Timestamp(b),
                    ..settled
                });
            }
        }
        self.settled = kept;
    }

    /// Forget `seal` entirely: its pieces, its shadowed record and what was
    /// settled of it.
    pub fn forget(&mut self, seal: &SealRecord) {
        self.spans
            .retain(|s| s.state == SpanState::Resident || s.seal != *seal);
        self.shadowed.retain(|s| s != seal);
        self.settled.retain(|s| !s.of(seal));
    }

    /// Recompute the coverage of every idle remote seal whose range meets
    /// `touched`, plus `added`, against everything else in the draft and
    /// `resident`. Seals with a fetch in flight are left alone; the fetch
    /// settles them when it lands. Returns whether anything was derived.
    pub fn derive(
        &mut self,
        resident: &[Extent],
        touched: Option<RangeInclusive<i64>>,
        added: impl IntoIterator<Item = RemoteSeal>,
    ) -> bool {
        let mut seals: Vec<RemoteSeal> = added.into_iter().collect();
        if let Some(touched) = touched {
            let meets = |seal: &SealRecord| {
                seal.start_ts.0 <= *touched.end() && *touched.start() <= seal.end_ts.0
            };
            let fetching: SmallVec<[SealRecord; 2]> = self
                .spans
                .iter()
                .filter(|s| s.state == SpanState::Fetching && meets(&s.seal))
                .map(|s| s.seal)
                .collect();
            self.spans.retain(|span| {
                let pull = span.state == SpanState::RemoteOnly
                    && meets(&span.seal)
                    && !fetching.contains(&span.seal);
                if pull {
                    seals.push(RemoteSeal::of(span));
                }
                !pull
            });
            self.shadowed.retain(|seal| {
                let pull = meets(seal);
                if pull {
                    seals.push(RemoteSeal::peer(*seal));
                }
                !pull
            });
        }
        if seals.is_empty() {
            return false;
        }
        seals.sort_unstable_by_key(|s| (s.seal.start_ts.0, s.seal.checksum));
        seals.dedup_by_key(|s| s.seal);

        self.spans.sort_unstable_by_key(|s| s.cover_start.0);
        let mut claims = self.claims(resident);
        let mut keys: Vec<i64> = self.spans.iter().map(|s| s.cover_start.0).collect();
        for remote in seals {
            let seal = remote.seal;
            let mut settled: Runs = self
                .settled
                .iter()
                .filter(|s| s.of(&seal))
                .map(|s| (s.start.0, s.end.0))
                .collect();
            settled.sort_unstable();
            let mut emitted = false;
            for (start, end) in claims.uncovered(seal.start_ts.0, seal.end_ts.0) {
                for (mut start, end) in subtract(start, end, &settled) {
                    // Only a tie on both sides of two seals' shared edge can
                    // collide; the timestamp is the neighbour's then.
                    if let Err(at) = keys.binary_search(&start) {
                        keys.insert(at, start);
                    } else if start < end {
                        start += 1;
                        let at = keys.partition_point(|k| *k < start);
                        keys.insert(at, start);
                    } else {
                        continue;
                    }
                    claims.insert(peer_claim(&seal, start, end));
                    self.spans.push(NodeSpan {
                        cover_start: Timestamp(start),
                        cover_end: Timestamp(end),
                        ..NodeSpan::whole(seal, SpanState::RemoteOnly, remote.source, remote.acked)
                    });
                    emitted = true;
                }
            }
            if !emitted {
                self.shadowed.push(seal);
            }
        }
        true
    }

    /// Everything that answers for time: remote coverage and resident
    /// nodes. Expects `spans` sorted.
    fn claims(&self, resident: &[Extent]) -> Claims {
        let remote = self
            .spans
            .iter()
            .filter(|s| s.state != SpanState::Resident)
            .map(|s| peer_claim(&s.seal, s.cover_start.0, s.cover_end.0));
        let local = resident.iter().map(|e| {
            let peer_node = self
                .spans
                .binary_search_by_key(&e.start, |s| s.cover_start.0)
                .ok()
                .map(|i| &self.spans[i])
                .filter(|s| {
                    s.state == SpanState::Resident
                        && s.source == SpanSource::RemoteFetch
                        && s.is_whole()
                });
            match peer_node {
                Some(span) => peer_claim(&span.seal, e.start, e.end),
                None => (e.start, e.end),
            }
        });
        Claims::new(remote.chain(local))
    }
}

/// What a stretch of `seal` claims for other seals: all of it, except a
/// timestamp on the seal's own edge, which a neighbouring seal sharing
/// that tie may also hold samples at.
fn peer_claim(seal: &SealRecord, mut start: i64, mut end: i64) -> (i64, i64) {
    if start < end && start == seal.start_ts.0 {
        start += 1;
    }
    if start < end && end == seal.end_ts.0 {
        end -= 1;
    }
    (start, end)
}

/// `[start, end]` minus the sorted, disjoint `cuts`.
fn subtract(start: i64, end: i64, cuts: &[(i64, i64)]) -> Runs {
    let mut runs = Runs::new();
    let mut cursor = start;
    for &(a, b) in cuts {
        if b < cursor {
            continue;
        }
        if a > end {
            break;
        }
        if a > cursor {
            runs.push((cursor, a - 1));
        }
        cursor = b.saturating_add(1);
        if cursor > end {
            return runs;
        }
    }
    runs.push((cursor, end));
    runs
}

/// A remote seal awaiting derivation, with the flags its pieces carry.
#[derive(Clone, Copy)]
pub(super) struct RemoteSeal {
    pub seal: SealRecord,
    pub source: SpanSource,
    pub acked: bool,
}

impl RemoteSeal {
    /// A seal a peer serves: by definition held durably elsewhere.
    pub fn peer(seal: SealRecord) -> Self {
        Self {
            seal,
            source: SpanSource::RemoteFetch,
            acked: true,
        }
    }

    pub fn of(span: &NodeSpan) -> Self {
        Self {
            seal: span.seal,
            source: span.source,
            acked: span.acked,
        }
    }
}

/// A resident node's sample range, oldest first in the slices built by
/// [`extents`].
pub(super) struct Extent {
    pub start: i64,
    pub end: i64,
}

/// Every non-empty node in `nodes` except `without`, sorted by start.
pub(super) fn extents(
    nodes: impl Iterator<Item = Arc<AtomicNode<TimeSeriesNode>>>,
    without: Option<&Arc<AtomicNode<TimeSeriesNode>>>,
) -> Vec<Extent> {
    let mut extents: Vec<Extent> = nodes
        .filter(|node| without.is_none_or(|w| !Arc::ptr_eq(w, node)))
        .filter_map(|node| {
            let timestamps = node.timestamps();
            let (start, end) = (timestamps.first()?.0, timestamps.last()?.0);
            Some(Extent { start, end })
        })
        .collect();
    extents.sort_unstable_by_key(|e| e.start);
    extents
}

/// Claimed time as sorted, disjoint inclusive intervals.
struct Claims(Vec<(i64, i64)>);

impl Claims {
    fn new(intervals: impl Iterator<Item = (i64, i64)>) -> Self {
        let mut sorted: Vec<(i64, i64)> = intervals.collect();
        sorted.sort_unstable();
        let mut merged: Vec<(i64, i64)> = Vec::with_capacity(sorted.len());
        for (start, end) in sorted {
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        Self(merged)
    }

    /// The runs of `[start, end]` no interval claims.
    fn uncovered(&self, start: i64, end: i64) -> Runs {
        let first = self.0.partition_point(|c| c.1 < start);
        subtract(start, end, &self.0[first..])
    }

    /// Claim a stretch of a run [`Self::uncovered`] returned.
    fn insert(&mut self, (start, end): (i64, i64)) {
        let at = self.0.partition_point(|c| c.0 < start);
        self.0.insert(at, (start, end));
    }
}

/// Whether every sample in `timestamps` falls inside some remote seal the
/// manifest records as fetchable right now, so dropping them loses nothing
/// a fetch cannot bring back. Seals with a fetch in flight do not count:
/// that fetch settles against the residency it started from. Compared by
/// sample rather than by range, since the peer's nodes leave sample-free
/// gaps between each other that a local node spans.
pub(super) fn remote_holds(manifest: &ComponentManifest, timestamps: &[Timestamp]) -> bool {
    let Some(first) = timestamps.first() else {
        return true;
    };
    let last = timestamps[timestamps.len() - 1];
    let fetching: SmallVec<[&SealRecord; 2]> = manifest
        .spans
        .iter()
        .filter(|s| s.state == SpanState::Fetching)
        .map(|s| &s.seal)
        .collect();
    let mut seals: Vec<(i64, i64)> = manifest
        .spans
        .iter()
        .filter(|s| s.state == SpanState::RemoteOnly && !fetching.contains(&&s.seal))
        .map(|s| &s.seal)
        .chain(manifest.shadowed.iter())
        .filter(|s| s.start_ts.0 <= last.0 && first.0 <= s.end_ts.0)
        .map(|s| (s.start_ts.0, s.end_ts.0))
        .collect();
    seals.sort_unstable();
    let mut next = 0;
    for (start, end) in seals {
        if timestamps[next].0 < start {
            return false;
        }
        next = timestamps.partition_point(|t| t.0 <= end).max(next);
        if next == timestamps.len() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncovered_runs_skip_claims() {
        let claims = Claims::new([(10, 19), (15, 25), (40, 49)].into_iter());
        assert_eq!(claims.0, vec![(10, 25), (40, 49)]);
        assert_eq!(&claims.uncovered(0, 60)[..], &[(0, 9), (26, 39), (50, 60)]);
        assert_eq!(&claims.uncovered(12, 20)[..], &[]);
        assert_eq!(&claims.uncovered(20, 45)[..], &[(26, 39)]);
        assert_eq!(&claims.uncovered(30, 35)[..], &[(30, 35)]);
    }
}
