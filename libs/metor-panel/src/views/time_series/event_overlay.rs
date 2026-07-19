//! One event-flag annotation layer on a time-series plot.
//!
//! A plot owns a `Vec<Entity<EventOverlay>>`; each names an
//! [`EventKindKey`](crate::plot_events::EventKindKey) — a built-in
//! log/alarm/sequence source or a recorded message id — whose events the plot
//! draws as vertical flags in its top gutter. The overlay is the persisted,
//! inspector-visible handle; the live events come from the
//! [`EventSourceRegistry`](crate::plot_events::EventSourceRegistry) source the
//! key resolves to, so no event data is duplicated here.

use gpui::SharedString;

use crate::plot_events::EventKindKey;

#[derive(facet::Facet)]
#[facet(pod)]
pub struct EventOverlay {
    /// Display name, snapshotted from the source when the overlay was added.
    pub label: SharedString,
    /// Draw this overlay's flags. Cleared to hide without removing.
    pub visible: bool,
    #[facet(opaque)]
    pub key: EventKindKey,
}

impl EventOverlay {
    pub fn new(label: impl Into<SharedString>, key: EventKindKey) -> Self {
        Self {
            label: label.into(),
            visible: true,
            key,
        }
    }
}
