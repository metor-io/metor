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
//! between two consecutive mirrored nodes looks uncovered even though the
//! peer holds nothing there. Comparing sample counts settles it: a seal
//! whose range already holds as many local samples as the seal does is
//! shadowed outright, so a mirror that saw the whole stream never fetches
//! a node just to learn it was empty.

use std::{ops::RangeInclusive, sync::Arc};

use smallvec::SmallVec;

use crate::{
    arc_ring::AtomicNode,
    manifest::{ComponentManifest, NodeSpan, SpanSource, SpanState},
    seal::SealRecord,
};

use super::TimeSeriesNode;

/// The manifest being rebuilt by one publish.
pub(super) struct Draft {
    pub spans: Vec<NodeSpan>,
    pub shadowed: Vec<SealRecord>,
}

impl Draft {
    pub fn of(manifest: &ComponentManifest) -> Self {
        Self {
            spans: manifest.spans.to_vec(),
            shadowed: manifest.shadowed.to_vec(),
        }
    }

    pub fn into_manifest(mut self) -> ComponentManifest {
        self.spans.sort_unstable_by_key(|s| s.cover_start.0);
        self.shadowed.sort_unstable_by_key(|s| s.start_ts.0);
        ComponentManifest {
            generation: 0,
            spans: self.spans.into_boxed_slice(),
            shadowed: self.shadowed.into_boxed_slice(),
        }
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

        let mut claims = Claims::new(
            self.spans
                .iter()
                .map(|s| (s.cover_start.0, s.cover_end.0))
                .chain(resident.iter().map(|e| (e.start, e.end))),
        );
        for remote in seals {
            let seal = remote.seal;
            if local_count(resident, seal.start_ts.0, seal.end_ts.0) >= seal.count {
                self.shadowed.push(seal);
                continue;
            }
            let pieces = claims.uncovered(seal.start_ts.0, seal.end_ts.0);
            if pieces.is_empty() {
                // Short on samples with nothing left uncovered: the local
                // nodes over this range dropped some (a full WAL). There is
                // no hole to install them into.
                self.shadowed.push(seal);
                continue;
            }
            for (start, end) in pieces {
                claims.insert(start, end);
                self.spans.push(NodeSpan {
                    cover_start: metor_proto::types::Timestamp(start),
                    cover_end: metor_proto::types::Timestamp(end),
                    ..NodeSpan::whole(seal, SpanState::RemoteOnly, remote.source, remote.acked)
                });
            }
        }
        true
    }
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

    fn of(span: &NodeSpan) -> Self {
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
    pub node: Arc<AtomicNode<TimeSeriesNode>>,
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
            Some(Extent { start, end, node })
        })
        .collect();
    extents.sort_unstable_by_key(|e| e.start);
    extents
}

/// Resident samples stamped inside `[start, end]`.
fn local_count(resident: &[Extent], start: i64, end: i64) -> u64 {
    let first = resident.partition_point(|e| e.end < start);
    resident[first..]
        .iter()
        .take_while(|e| e.start <= end)
        .map(|e| {
            let timestamps = e.node.timestamps();
            let lo = timestamps.partition_point(|t| t.0 < start);
            let hi = timestamps.partition_point(|t| t.0 <= end);
            (hi - lo) as u64
        })
        .sum()
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
    fn uncovered(&self, start: i64, end: i64) -> SmallVec<[(i64, i64); 2]> {
        let mut runs = SmallVec::new();
        let mut cursor = start;
        let first = self.0.partition_point(|c| c.1 < start);
        for &(claim_start, claim_end) in &self.0[first..] {
            if claim_start > end {
                break;
            }
            if claim_start > cursor {
                runs.push((cursor, claim_start - 1));
            }
            cursor = claim_end.saturating_add(1);
            if cursor > end {
                return runs;
            }
        }
        runs.push((cursor, end));
        runs
    }

    /// Claim an interval [`Self::uncovered`] returned.
    fn insert(&mut self, start: i64, end: i64) {
        let at = self.0.partition_point(|c| c.0 < start);
        self.0.insert(at, (start, end));
    }
}

/// Whether every sample in `timestamps` falls inside some remote seal the
/// manifest still records, so dropping them loses nothing a fetch cannot
/// bring back. Compared by sample rather than by range, since the peer's
/// nodes leave sample-free gaps between each other that a local node
/// spans.
pub(super) fn remote_holds(
    manifest: &ComponentManifest,
    timestamps: &[metor_proto::types::Timestamp],
) -> bool {
    let Some(first) = timestamps.first() else {
        return true;
    };
    let last = timestamps[timestamps.len() - 1];
    let mut seals: Vec<(i64, i64)> = manifest
        .spans
        .iter()
        .filter(|s| s.state != SpanState::Resident)
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
