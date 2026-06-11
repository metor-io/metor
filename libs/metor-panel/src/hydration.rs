//! App-wide handle for requesting remote-only history.
//!
//! Installed by [`PanelApp::remote`](crate::PanelApp::remote) (or by a
//! consumer that wires its own store); absent when the panel runs purely
//! locally, in which case views simply show gaps without fetching.

use gpui::Global;
use metor_db::remote::{Envelopes, Hydrator};

pub struct HydratorGlobal(pub Hydrator);

impl Global for HydratorGlobal {}

/// The installed hydrator, if any. Cheap to clone — it's a queue handle.
pub fn hydrator(cx: &gpui::App) -> Option<Hydrator> {
    cx.try_global::<HydratorGlobal>().map(|h| h.0.clone())
}

pub struct EnvelopesGlobal(pub Envelopes);

impl Global for EnvelopesGlobal {}

/// The installed envelope fetcher, if any. Like the hydrator, absent on
/// purely-local panels — wide views then fall back to gap bands.
pub fn envelopes(cx: &gpui::App) -> Option<Envelopes> {
    cx.try_global::<EnvelopesGlobal>().map(|e| e.0.clone())
}
