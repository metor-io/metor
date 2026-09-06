//! Time-series adapter for the shared event flag renderer.
pub(super) use crate::plot_events::flags::{
    ClusterPaint, EventCluster, FLAG_HIT_PX, GUTTER_H, cluster_events,
};

pub(super) fn paint_event_flags(
    bounds: gpui::Bounds<gpui::Pixels>,
    view: &super::PlotView,
    clusters: &[ClusterPaint],
    window: &mut gpui::Window,
    cx: &mut gpui::App,
) {
    crate::plot_events::flags::paint_event_flags(
        super::plot_area(bounds, view.axis_count()),
        clusters,
        window,
        cx,
    );
}
