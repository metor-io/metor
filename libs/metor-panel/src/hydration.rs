//! App-wide handle for requesting remote-only history.
//!
//! One [`Hydrator`] per connected target that has one, registered by the
//! [`connections`](crate::connections) store as targets connect and
//! disconnect. Requests broadcast to every registered hydrator: with the
//! disjoint component namespaces the connection system assumes, the wrong
//! peer's fetch comes back empty and dies in that hydrator's backoff memo,
//! which is cheap at the handful of connections a panel realistically
//! holds. A learned component→peer routing map is the follow-up if that
//! stops being true.
//!
//! Besides whole sealed nodes, a hydrator answers range fetches and point
//! reads, which land as in-memory fragments of a node rather than the node:
//! what a value view or a zoomed-in plot needs of a node far bigger than
//! that. Point reads asked in the same frame go out together, one round
//! trip for a scrubbed dashboard.
//!
//! A hydrator forgets a request nobody repeats within
//! [`REQUEST_TTL`](metor_db::remote::REQUEST_TTL), so a position the user
//! scrubbed past never delays the current one. Views repeat their requests
//! when they render, but a paused view doesn't render; while a backlog waits
//! this repaints the windows inside that lifetime, so every view still on
//! screen asks again and the ones that closed or moved on don't.

use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use gpui::Global;
use metor_db::manifest::{ComponentManifest, Gap, SpanSource};
use metor_db::remote::{FetchError, HydratePriority, Hydrator, PAGE_BYTES, REQUEST_TTL};
use metor_proto::types::{ComponentId, Timestamp};

use crate::connections::TargetId;

/// The registered hydrator set. Cheap to clone — a shared handle.
#[derive(Clone, Default)]
pub struct Hydrators(Arc<Inner>);

#[derive(Default)]
struct Inner {
    hydrators: Mutex<Vec<(TargetId, Hydrator)>>,
    /// Whether a repaint to renew requests is already scheduled.
    renewing: AtomicBool,
}

impl Hydrators {
    /// Ask every connected peer for `range`; peers without the component
    /// drop the request in their backoff memos.
    pub fn request(
        &self,
        component_id: ComponentId,
        range: Range<Timestamp>,
        priority: HydratePriority,
        cx: &gpui::App,
    ) {
        for (_, hydrator) in self.0.hydrators.lock().unwrap().iter() {
            hydrator.request(component_id, range.clone(), priority);
        }
        self.renew_later(cx);
    }

    /// Ask every connected peer for `range` as a range fetch: the samples,
    /// landing as fragments, instead of the nodes holding them.
    pub fn request_range(
        &self,
        component_id: ComponentId,
        range: Range<Timestamp>,
        priority: HydratePriority,
    ) {
        for (_, hydrator) in self.0.hydrators.lock().unwrap().iter() {
            hydrator.request_range(component_id, range.clone(), priority);
        }
    }

    /// Ask every connected peer for the sample each reader shows at its
    /// instant, queued at once so each peer serves them in one batch.
    pub fn request_points(&self, reads: &[(ComponentId, Timestamp)], priority: HydratePriority) {
        for (_, hydrator) in self.0.hydrators.lock().unwrap().iter() {
            hydrator.request_points(reads.iter().copied(), priority);
        }
    }

    /// Await the point read of `t` on every connected peer: the range the
    /// peer holding the component vouched for, or the reason none answered.
    /// Awaiting keeps the request alive past the request lifetime; dropping
    /// the future lets it lapse.
    pub fn point(
        &self,
        component_id: ComponentId,
        t: Timestamp,
        priority: HydratePriority,
    ) -> impl Future<Output = Result<Range<Timestamp>, FetchError>> + 'static {
        type Read = Pin<Box<dyn Future<Output = Result<Range<Timestamp>, FetchError>>>>;
        let mut reads: Vec<Read> = self
            .0
            .hydrators
            .lock()
            .unwrap()
            .iter()
            .map(|(_, hydrator)| {
                let hydrator = hydrator.clone();
                Box::pin(async move { hydrator.point(component_id, t, priority).await }) as Read
            })
            .collect();
        async move {
            let mut outcome = Err(FetchError::Expired);
            std::future::poll_fn(|cx| {
                reads.retain_mut(|read| match read.as_mut().poll(cx) {
                    Poll::Ready(result) => {
                        // An answer beats a failure, and a failure beats
                        // the peers that never served the request.
                        match (&outcome, result) {
                            (Ok(_), _) => {}
                            (_, Ok(covered)) => outcome = Ok(covered),
                            (Err(FetchError::Expired), Err(err)) => outcome = Err(err),
                            (Err(_), Err(_)) => {}
                        }
                        false
                    }
                    Poll::Pending => true,
                });
                if reads.is_empty() || outcome.is_ok() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            outcome
        }
    }

    /// Repaint once, half a request lifetime from now, if a backlog is still
    /// waiting then. The repaint's own requests schedule the next one, so
    /// this stops by itself once the hydrators catch up.
    fn renew_later(&self, cx: &gpui::App) {
        if self.0.renewing.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = self.clone();
        cx.spawn(async move |cx| {
            cx.background_executor().timer(REQUEST_TTL / 2).await;
            this.0.renewing.store(false, Ordering::Release);
            let backlogged = this
                .0
                .hydrators
                .lock()
                .unwrap()
                .iter()
                .any(|(_, h)| h.backlogged());
            if backlogged {
                cx.update(|cx| cx.refresh_windows());
            }
        })
        .detach();
    }

    /// Ask for a remote-only `gap` a plot shows: as the whole node holding
    /// it, unless [`range_fetch`] finds the node far bigger than the gap or
    /// there is no node yet.
    pub fn request_gap(
        &self,
        component_id: ComponentId,
        manifest: &ComponentManifest,
        gap: &Gap,
        priority: HydratePriority,
        cx: &gpui::App,
    ) {
        match range_fetch(manifest, gap) {
            Some(range) => self.request_range(component_id, range, priority),
            None => self.request(component_id, gap.range.clone(), priority, cx),
        }
    }

    fn is_empty(&self) -> bool {
        self.0.hydrators.lock().unwrap().is_empty()
    }

    pub(crate) fn insert(&self, target: TargetId, hydrator: Hydrator) {
        self.0.hydrators.lock().unwrap().push((target, hydrator));
    }

    pub(crate) fn remove(&self, target: &TargetId) {
        self.0
            .hydrators
            .lock()
            .unwrap()
            .retain(|(id, _)| id != target);
    }

    /// The installed set, creating it on first use.
    pub(crate) fn global(cx: &mut gpui::App) -> Hydrators {
        cx.default_global::<HydratorGlobal>().0.clone()
    }
}

/// How many times a gap's share of its node the node must be before the
/// gap is range-fetched instead.
const NODE_TO_GAP: u64 = 8;

/// The range to fetch for `gap` instead of its whole node, if any.
///
/// A whole node is the better fetch whenever it is affordable: it lands
/// checksum-verified against its seal as the span itself, and replaces any
/// fragments under it, where fragments are a cache that only ever goes
/// away. A range fetch earns its place when the node is mostly bytes the
/// plot doesn't show, as when a zoomed-in window sits over a node of up to
/// 64 MB. So a gap is range-fetched when its share of the node's bytes
/// (prorated by time, as [`ComponentManifest::count_in`] prorates samples)
/// fits one page ([`PAGE_BYTES`], one round trip) and is at most an eighth
/// of the node; anything wider fetches the node.
///
/// A peer's unsealed head ([`SpanSource::PeerHead`]) has no node to
/// fetch, so its gaps are always range-fetched; a wide one pages in over
/// repeated asks.
///
/// The gap is first widened to a power-of-two grid within its span, so a
/// panning plot mostly repeats one ask instead of making a new one per
/// frame.
fn range_fetch(manifest: &ComponentManifest, gap: &Gap) -> Option<Range<Timestamp>> {
    let span = manifest.span(gap.start_ts)?;
    let cover = span.cover();
    let len = gap.range.end.0.checked_sub(gap.range.start.0)?.max(1) as u64;
    let grid = len.checked_next_power_of_two()? as i64;
    let start = gap.range.start.0.div_euclid(grid) * grid;
    let end = gap.range.end.0.saturating_add(grid - 1).div_euclid(grid) * grid;
    let range = Timestamp(start.max(cover.start.0))..Timestamp(end.min(cover.end.0));
    if span.source == SpanSource::PeerHead {
        return Some(range);
    }
    let seal_len = (span.seal.end_ts.0 - span.seal.start_ts.0) as u128 + 1;
    let share = (span.bytes() as u128 * (range.end.0 - range.start.0) as u128 / seal_len) as u64;
    (share <= PAGE_BYTES as u64 && share.saturating_mul(NODE_TO_GAP) <= span.bytes())
        .then_some(range)
}

#[derive(Default)]
pub struct HydratorGlobal(pub Hydrators);

impl Global for HydratorGlobal {}

/// The hydrator set, if any target registered one. `None` when no connected
/// target fetches history, in which case views simply show gaps.
pub fn hydrator(cx: &gpui::App) -> Option<Hydrators> {
    cx.try_global::<HydratorGlobal>()
        .map(|h| h.0.clone())
        .filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use metor_db::manifest::{NodeSpan, SpanSource, SpanState};
    use metor_db::seal::SealRecord;

    use super::*;

    const MB: u64 = 1024 * 1024;

    /// A remote span over 0..=999_999 holding `bytes`, and its gap over
    /// `range`.
    fn gap(bytes: u64, range: Range<i64>) -> Option<Range<i64>> {
        gap_in(SpanSource::RemoteFetch, bytes, range)
    }

    fn gap_in(source: SpanSource, bytes: u64, range: Range<i64>) -> Option<Range<i64>> {
        let seal = SealRecord {
            start_ts: Timestamp(0),
            end_ts: Timestamp(999_999),
            count: 1_000_000,
            index_len: bytes / 2,
            data_len: bytes / 2,
            checksum: 1,
            element_size: 8,
        };
        let manifest = ComponentManifest {
            spans: [NodeSpan::whole(seal, SpanState::RemoteOnly, source, false)].into(),
            ..Default::default()
        };
        let gap = Gap {
            range: Timestamp(range.start)..Timestamp(range.end),
            state: SpanState::RemoteOnly,
            start_ts: Timestamp(0),
            seal_start: Timestamp(0),
        };
        range_fetch(&manifest, &gap).map(|r| r.start.0..r.end.0)
    }

    #[test]
    fn a_small_gap_in_a_big_node_is_range_fetched() {
        // A zoomed-in plot over a full node: widened to the grid.
        assert_eq!(gap(64 * MB, 1000..2000), Some(0..2048));
        assert_eq!(gap(64 * MB, 5000..6000), Some(4096..6144));
        // Past one page, or a big share of the node, the node is the fetch.
        assert_eq!(gap(64 * MB, 0..500_000), None);
        assert_eq!(gap(MB, 0..200_000), None);
        // The grid never reaches past the span.
        assert_eq!(gap(64 * MB, 999_000..999_990), Some(998_400..1_000_000));
    }

    /// A peer's unsealed head has no node to fetch: however wide the gap,
    /// it is read by range.
    #[test]
    fn a_gap_in_a_peers_head_is_always_range_fetched() {
        assert_eq!(
            gap_in(SpanSource::PeerHead, MB, 0..500_000),
            Some(0..524_288)
        );
        assert_eq!(
            gap_in(SpanSource::PeerHead, 64 * MB, 1000..2000),
            Some(0..2048)
        );
    }
}
