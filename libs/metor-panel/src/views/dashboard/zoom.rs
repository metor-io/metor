//! Dashboard zoom: the canvas-to-screen mapping and the element that makes
//! widget content follow it.
//!
//! gpui has no subtree scale transform, so zoom is a layout change rather
//! than a paint one. Widget rects stay in unscaled dashboard units on disk and
//! are multiplied by the zoom when laid out; every pointer position goes back
//! through [`Viewport::to_canvas`] before it touches a rect. What a widget
//! draws inside its box follows through [`Zoomed`], which scales the rem and
//! the inherited text size — so rem-based spacing and text that does not pin
//! its own size shrink with the box, while explicit `px` sizes stay put.
use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Point, Size, TextStyleRefinement, Window, point, px,
};

pub(super) const MIN_ZOOM: f32 = 0.25;
pub(super) const MAX_ZOOM: f32 = 2.0;

/// Ratio between neighbouring zoom levels for the Zoom In / Zoom Out commands.
pub(super) const ZOOM_STEP: f32 = 1.25;

/// How strongly one pixel of wheel travel zooms; a 20px mouse-wheel notch is
/// about 10%.
const WHEEL_ZOOM_RATE: f32 = 0.005;

/// Breathing room kept past the content's far edges when fitting, matching
/// the margin `auto_place` leaves at the near edges.
const FIT_MARGIN: f32 = 20.0;

pub(super) fn clamp_zoom(zoom: f32) -> f32 {
    if zoom.is_finite() {
        zoom.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    }
}

/// The zoom multiplier a wheel delta asks for. Positive `dy` zooms in, the
/// same direction a plot zooms for the same gesture.
pub(super) fn wheel_zoom_factor(dy: f32) -> f32 {
    (dy * WHEEL_ZOOM_RATE).exp()
}

/// Which part of the dashboard is on screen, and how large.
///
/// Screen coordinates here are relative to the canvas container's origin;
/// canvas coordinates are the dashboard units widget rects are stored in.
/// `scroll` is kept in screen pixels so a pan moves by what the wheel says
/// whatever the zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Viewport {
    pub(super) zoom: f32,
    /// Negative values bring content below and right of the origin into view.
    pub(super) scroll: Point<f32>,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            scroll: point(0.0, 0.0),
        }
    }
}

impl Viewport {
    pub(super) fn at_zoom(zoom: f32) -> Self {
        Self {
            zoom: clamp_zoom(zoom),
            ..Self::default()
        }
    }

    pub(super) fn to_canvas(self, screen: Point<f32>) -> Point<f32> {
        point(
            (screen.x - self.scroll.x) / self.zoom,
            (screen.y - self.scroll.y) / self.zoom,
        )
    }

    pub(super) fn to_screen(self, canvas: Point<f32>) -> Point<f32> {
        point(
            canvas.x * self.zoom + self.scroll.x,
            canvas.y * self.zoom + self.scroll.y,
        )
    }

    /// Change the zoom while the canvas point under `anchor` (screen) stays
    /// under it, which is what makes zooming toward the cursor feel anchored.
    pub(super) fn zoom_around(&mut self, zoom: f32, anchor: Point<f32>) {
        let zoom = clamp_zoom(zoom);
        let fixed = self.to_canvas(anchor);
        self.zoom = zoom;
        self.scroll = point(anchor.x - fixed.x * zoom, anchor.y - fixed.y * zoom);
    }

    /// Keep the pan inside the scaled content. `extent` is the content's
    /// far corner in canvas units; without a known viewport only the origin
    /// side can be enforced.
    pub(super) fn clamp(&mut self, extent: Point<f32>, view: Option<Size<f32>>) {
        let (min_x, min_y) = match view {
            Some(view) => (
                -(extent.x * self.zoom - view.width).max(0.0),
                -(extent.y * self.zoom - view.height).max(0.0),
            ),
            None => (f32::NEG_INFINITY, f32::NEG_INFINITY),
        };
        self.scroll.x = self.scroll.x.clamp(min_x, 0.0);
        self.scroll.y = self.scroll.y.clamp(min_y, 0.0);
    }

    /// The viewport that shows all content at once, never magnified: a
    /// dashboard that already fits comes back at 1:1. Below the minimum zoom
    /// it shows what fits from the top-left.
    pub(super) fn fit(extent: Point<f32>, view: Size<f32>) -> Self {
        let w = extent.x + FIT_MARGIN;
        let h = extent.y + FIT_MARGIN;
        let zoom = if extent.x <= 0.0 || extent.y <= 0.0 {
            1.0
        } else {
            (view.width / w).min(view.height / h).min(1.0)
        };
        Self::at_zoom(zoom)
    }
}

/// Lays a widget's content out as if the window's rem and inherited text
/// size were scaled by the dashboard zoom.
///
/// Both overrides only reach lengths resolved against them: gpui's spacing
/// helpers (`p_2`, `gap_1`, …), `rems(…)`, and text that inherits its size.
/// Explicit `px(…)` sizes — most view text, and any glyphs a view shapes
/// itself — keep their size and clip at the widget's edge instead.
pub(super) struct Zoomed {
    zoom: f32,
    child: AnyElement,
}

impl Zoomed {
    pub(super) fn new(zoom: f32, child: impl IntoElement) -> Self {
        Self {
            zoom,
            child: child.into_any_element(),
        }
    }

    /// Run one layout phase under the scaled rem and text size. The inherited
    /// size is read before the rem override so it is scaled exactly once.
    fn scaled_with<R>(zoom: f32, window: &mut Window, f: impl FnOnce(&mut Window) -> R) -> R {
        let rem = window.rem_size();
        let font_size = window.text_style().font_size.to_pixels(rem);
        let text = TextStyleRefinement {
            font_size: Some(px(f32::from(font_size) * zoom).into()),
            ..Default::default()
        };
        window.with_rem_size(Some(rem * zoom), |window| {
            window.with_text_style(Some(text), f)
        })
    }
}

impl IntoElement for Zoomed {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Zoomed {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let child = &mut self.child;
        let layout =
            Self::scaled_with(self.zoom, window, |window| child.request_layout(window, cx));
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let child = &mut self.child;
        Self::scaled_with(self.zoom, window, |window| child.prepaint(window, cx));
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let child = &mut self.child;
        Self::scaled_with(self.zoom, window, |window| child.paint(window, cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::size;

    fn close(a: Point<f32>, b: Point<f32>) -> bool {
        (a.x - b.x).abs() < 1e-3 && (a.y - b.y).abs() < 1e-3
    }

    #[test]
    fn screen_and_canvas_round_trip() {
        let v = Viewport {
            zoom: 0.5,
            scroll: point(-40.0, -10.0),
        };
        let c = point(300.0, 120.0);
        assert!(close(v.to_screen(c), point(110.0, 50.0)));
        assert!(close(v.to_canvas(v.to_screen(c)), c));
    }

    #[test]
    fn zooming_keeps_the_anchor_fixed() {
        let mut v = Viewport {
            zoom: 1.0,
            scroll: point(-100.0, -60.0),
        };
        let anchor = point(250.0, 180.0);
        let before = v.to_canvas(anchor);
        v.zoom_around(0.4, anchor);
        assert_eq!(v.zoom, 0.4);
        assert!(close(v.to_canvas(anchor), before));
        v.zoom_around(1.7, anchor);
        assert!(close(v.to_canvas(anchor), before));
    }

    #[test]
    fn zoom_is_clamped_and_still_anchored() {
        let mut v = Viewport::default();
        let anchor = point(80.0, 40.0);
        let before = v.to_canvas(anchor);
        v.zoom_around(0.01, anchor);
        assert_eq!(v.zoom, MIN_ZOOM);
        assert!(close(v.to_canvas(anchor), before));
        v.zoom_around(f32::NAN, anchor);
        assert_eq!(v.zoom, 1.0);
        assert_eq!(clamp_zoom(9.0), MAX_ZOOM);
    }

    #[test]
    fn scroll_clamps_against_the_scaled_extent() {
        let extent = point(2000.0, 1000.0);
        let view = size(800.0, 600.0);
        let mut v = Viewport {
            zoom: 0.5,
            scroll: point(-5000.0, -5000.0),
        };
        v.clamp(extent, Some(view));
        // 1000×500 on screen: only the width overflows.
        assert!(close(v.scroll, point(-200.0, 0.0)));

        v.scroll = point(30.0, 30.0);
        v.clamp(extent, Some(view));
        assert!(close(v.scroll, point(0.0, 0.0)));
    }

    #[test]
    fn fit_shows_everything_without_magnifying() {
        let view = size(800.0, 600.0);
        let fit = Viewport::fit(point(1580.0, 580.0), view);
        assert!((fit.zoom - 0.5).abs() < 1e-6);
        assert_eq!(fit.scroll, point(0.0, 0.0));
        // The whole extent, margin included, lands inside the viewport.
        let corner = fit.to_screen(point(1600.0, 600.0));
        assert!(corner.x <= view.width + 1e-3 && corner.y <= view.height + 1e-3);

        assert_eq!(Viewport::fit(point(100.0, 100.0), view).zoom, 1.0);
        assert_eq!(Viewport::fit(point(0.0, 0.0), view).zoom, 1.0);
        assert_eq!(Viewport::fit(point(100_000.0, 10.0), view).zoom, MIN_ZOOM);
    }

    #[test]
    fn wheel_direction_matches_plots() {
        assert!(wheel_zoom_factor(20.0) > 1.0);
        assert!(wheel_zoom_factor(-20.0) < 1.0);
        assert_eq!(wheel_zoom_factor(0.0), 1.0);
    }
}
