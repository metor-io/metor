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
use metor_db::remote::{FetchError, HydratePriority, Hydrator, REQUEST_TTL, RangeQuery};
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

    /// Ask every connected peer for `query` as a range fetch: the samples,
    /// landing as fragments, instead of the nodes holding them.
    pub fn request_range(
        &self,
        component_id: ComponentId,
        query: RangeQuery,
        priority: HydratePriority,
    ) {
        for (_, hydrator) in self.0.hydrators.lock().unwrap().iter() {
            hydrator.request_range(component_id, query.clone(), priority);
        }
    }

    /// Ask every connected peer for the sample each reader shows at its
    /// instant, queued at once so each peer serves them in one batch.
    pub fn request_points(&self, reads: &[(ComponentId, Timestamp)], priority: HydratePriority) {
        for (_, hydrator) in self.0.hydrators.lock().unwrap().iter() {
            hydrator.request_ranges(
                reads.iter().map(|(id, t)| (*id, RangeQuery::instant(*t))),
                priority,
            );
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
