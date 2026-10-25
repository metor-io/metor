//! Drag-to-pan and scroll-to-zoom over a single [`PlotBounds`], shared by
//! the plots whose view is one rectangle (XY, list, spectrogram).
//!
//! The gesture is zone-aware: over the plot both axes move, over an axis
//! gutter only that axis does. The wrapper owns a [`PanZoom`], records the
//! plot area each prepaint, and forwards mouse events; whatever view comes
//! back is the caller's to install as its interactive override.

use gpui::{Bounds, Pixels, Point, ScrollWheelEvent, px};

use crate::views::time_series::{AxisZone, PlotBounds, axis_zone};

/// Pan/zoom gesture state for a single-axis-set plot.
pub(crate) struct PanZoom {
    /// Press position and the view at that instant; drags offset from it so
    /// the content tracks the pointer without accumulating error.
    drag_start: Option<(Point<Pixels>, PlotBounds)>,
    drag_zone: AxisZone,
    plot_area: Option<Bounds<Pixels>>,
}

impl Default for PanZoom {
    fn default() -> Self {
        Self {
            drag_start: None,
            drag_zone: AxisZone::Plot,
            plot_area: None,
        }
    }
}

impl PanZoom {
    /// The plot area as of the last prepaint.
    pub fn plot_area(&self) -> Option<Bounds<Pixels>> {
        self.plot_area
    }

    pub fn set_plot_area(&mut self, pa: Bounds<Pixels>) {
        self.plot_area = Some(pa);
    }

    /// Begin a drag at `pos`. A plot with no view yet records nothing to
    /// drag from, so the gesture is inert until release.
    pub fn press(&mut self, pos: Point<Pixels>, view: Option<PlotBounds>) {
        self.drag_zone = self
            .plot_area
            .map(|pa| axis_zone(pos, pa, 1, 1.0))
            .unwrap_or(AxisZone::Plot);
        self.drag_start = view.map(|view| (pos, view));
    }

    pub fn release(&mut self) {
        self.drag_start = None;
    }

    /// The view a drag to `pos` produces, if a drag is in progress.
    pub fn drag(&self, pos: Point<Pixels>) -> Option<PlotBounds> {
        let ((start, start_view), pa) = (self.drag_start?, self.plot_area?);
        let dx = pos.x - start.x;
        let dy = pos.y - start.y;
        let (nx, ny) = start_view.screen_delta_to_norm(pa, dx, dy);
        Some(match self.drag_zone {
            AxisZone::Plot => start_view.offset_by_norm(-nx, ny),
            AxisZone::XAxis => start_view.offset_x(-nx),
            AxisZone::YAxis(_) => start_view.offset_y(ny),
        })
    }

    /// The view a wheel step produces, zooming about the pointer along the
    /// axes of the zone it is over.
    pub fn scroll(&self, view: Option<PlotBounds>, event: &ScrollWheelEvent) -> Option<PlotBounds> {
        let view = view?;
        let pa = self.plot_area?;
        let delta = event.delta.pixel_delta(px(20.0));
        let zoom_amount = f32::from(-delta.y) as f64 / 200.0;
        let factor = (1.0_f64 + zoom_amount).clamp(0.5, 2.0);

        let (ax, ay) = view.screen_anchor(pa, event.position);
        // One axis column: its width never decides an index, so the zoom it
        // was laid out at doesn't matter here.
        Some(match axis_zone(event.position, pa, 1, 1.0) {
            AxisZone::Plot => view.zoom_at(factor, ax, 1.0 - ay),
            AxisZone::XAxis => view.zoom_x(factor, ax),
            AxisZone::YAxis(_) => view.zoom_y(factor, 1.0 - ay),
        })
    }
}
