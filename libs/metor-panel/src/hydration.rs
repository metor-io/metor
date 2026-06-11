//! App-wide handle for requesting remote-only history.
//!
//! Installed by [`PanelApp::remote`](crate::PanelApp::remote) (or by a
//! consumer that wires its own store); absent when the panel runs purely
//! locally, in which case views simply show gaps without fetching.

use gpui::Global;
use metor_db::remote::Hydrator;

pub struct HydratorGlobal(pub Hydrator);

impl Global for HydratorGlobal {}

/// The installed hydrator, if any. Cheap to clone — it's a queue handle.
pub fn hydrator(cx: &gpui::App) -> Option<Hydrator> {
    cx.try_global::<HydratorGlobal>().map(|h| h.0.clone())
}
