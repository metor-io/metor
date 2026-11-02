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
//! A hydrator forgets a request nobody repeats within
//! [`REQUEST_TTL`](metor_db::remote::REQUEST_TTL), so a position the user
//! scrubbed past never delays the current one. Views repeat their requests
//! when they render, but a paused view doesn't render; while a backlog waits
//! this repaints the windows inside that lifetime, so every view still on
//! screen asks again and the ones that closed or moved on don't.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gpui::Global;
use metor_db::remote::{HydratePriority, Hydrator, REQUEST_TTL};
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
