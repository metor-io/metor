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

use std::ops::Range;
use std::sync::{Arc, Mutex};

use gpui::Global;
use metor_db::remote::Hydrator;
use metor_proto::types::{ComponentId, Timestamp};

use crate::connections::TargetId;

/// The registered hydrator set. Cheap to clone — a shared handle.
#[derive(Clone, Default)]
pub struct Hydrators(Arc<Mutex<Vec<(TargetId, Hydrator)>>>);

impl Hydrators {
    /// Ask every connected peer for `range`; peers without the component
    /// drop the request in their backoff memos.
    pub fn request(&self, component_id: ComponentId, range: Range<Timestamp>) {
        for (_, hydrator) in self.0.lock().unwrap().iter() {
            hydrator.request(component_id, range.clone());
        }
    }

    fn is_empty(&self) -> bool {
        self.0.lock().unwrap().is_empty()
    }

    pub(crate) fn insert(&self, target: TargetId, hydrator: Hydrator) {
        self.0.lock().unwrap().push((target, hydrator));
    }

    pub(crate) fn remove(&self, target: &TargetId) {
        self.0.lock().unwrap().retain(|(id, _)| id != target);
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
