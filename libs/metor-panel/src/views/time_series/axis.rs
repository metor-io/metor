//! One Y axis of a time-series plot.
//!
//! A plot owns a `Vec<Entity<YAxis>>`; each [`Trace`](super::Trace) names the
//! axis it belongs to by index ([`Trace::axis_index`](super::Trace)). An axis
//! auto-scales over only the traces assigned to it and can be panned and
//! zoomed independently of the others, so signals with wildly different
//! magnitudes can share one plot and still be read separately.
//!
//! `color` tints the axis chrome (tick labels and the axis rule) when a plot
//! has more than one axis; a single-axis plot renders in the neutral theme
//! color, unchanged from before multi-axis support existed.

use gpui::SharedString;

use super::override_field::Override;

#[derive(Clone, facet::Facet)]
#[facet(pod)]
pub struct YAxis {
    pub label: SharedString,
    pub y_min_override: Override<f64>,
    pub y_max_override: Override<f64>,
    /// `Auto` resolves at render time — the sole visible trace's color if the
    /// axis has exactly one, otherwise a palette color by axis index.
    /// `Custom` pins an explicit color.
    pub color: Override<gpui::Hsla>,
}

impl YAxis {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            y_min_override: Override::Auto,
            y_max_override: Override::Auto,
            color: Override::Auto,
        }
    }
}
