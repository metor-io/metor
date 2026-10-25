//! Linear bar meter — "how full is it", in one glance.
//!
//! The instrument a tank level, a wheel's stored momentum, or a duty cycle
//! wants: a track, a fill, and the thresholds the control system already
//! declared. Limits are never configured here — [`binding::limit_marks`]
//! reads them from the alarm definitions, so a meter and a plot of the same
//! element always agree about where the redline is.
//!
//! Scales that span zero fill from the zero position outward rather than from
//! the low end (see [`fill_span`]). Signed quantities are the common case in
//! flight software — body rates, momentum, torque — and a bar that grows from
//! the bottom for a negative rate reads as "nearly empty" when it means
//! "hard over the other way".
//!
//! The track can take the silhouette of the thing it measures (see
//! [`MeterShape`]). Every shape is vector geometry painted twice — once in
//! the track color, once in the fill color under a content mask cut to the
//! fill's slice of the scale — so the fill follows the outline exactly with
//! nothing rasterized, and the same trick clips limit ticks to the outline.

use std::sync::Arc;

use gpui::{
    App, Bounds, ContentMask, Context, Hsla, IntoElement, PathBuilder, Pixels, Point, SharedString,
    Window, canvas, div, fill, point, prelude::*, px, size,
};
use metor_db::DB;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::binding::{
    self, ElementRef, LiveBinding, ScalarInstrument, component_meta, default_label, stream_scalar,
};
use super::format_number;
use crate::theme::{theme, ui_px};

/// Which way the bar grows.
#[derive(Clone, Copy, Debug, Default, PartialEq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum Orientation {
    #[default]
    Vertical,
    Horizontal,
}

/// The silhouette of a meter's track.
///
/// The scale always runs linearly along the meter's axis, so a shaped meter
/// reads like a sight glass on the real vessel: the fill line sits at the
/// fraction of the shape's height the value represents, not the fraction of
/// its volume. Horizontal meters lay each shape on its side, its high end to
/// the right.
#[derive(Clone, Copy, Debug, Default, PartialEq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum MeterShape {
    #[default]
    Bar,
    /// A capsule: a tube with fully rounded ends.
    Tank,
    /// A tube with shallow elliptical ends and a rim across its high end.
    Cylinder,
    /// A round tank, kept circular and centered whatever the tile's aspect.
    Sphere,
    /// A cell with its terminal at the high end; the scale covers the body.
    Battery,
}

/// Thickness of a limit tick drawn across the track.
const TICK_PX: f32 = 1.5;
/// Corner rounding shared by the track and its fill.
const TRACK_RADIUS: f32 = 3.0;
/// Height of a horizontal bar. Other shapes need the tile's height to read.
const BAR_THICKNESS: f32 = 10.0;
/// A cylinder's end cap depth along the axis, as a fraction of its breadth.
const CYLINDER_CAP: f32 = 0.18;
/// A battery terminal's breadth as a fraction of the body's.
const TERMINAL_BREADTH: f32 = 0.45;
/// A battery terminal's length along the axis, as a fraction of the meter's.
const TERMINAL_LENGTH: f32 = 0.08;
/// Width of the rim drawn across a cylinder's high end.
const RIM_PX: f32 = 1.0;
/// Control-point distance, as a fraction of the radius, that makes one cubic
/// Bézier approximate a quarter ellipse.
const QUARTER_ARC: f32 = 0.552_284_8;

/// Persisted shape of a [`Meter`], shared by the tile and dashboard
/// surfaces — the two differ only in how they host the view, not in what an
/// operator can configure.
///
/// `label` and `unit` are `Option`: `None` means "never edited", so a
/// restore keeps whatever the component's metadata supplies rather than
/// freezing a stale copy of it into the layout.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MeterConfig {
    pub component: String,
    pub element: usize,
    pub label: Option<String>,
    pub min: f64,
    pub max: f64,
    pub unit: Option<String>,
    pub orientation: Orientation,
    pub shape: MeterShape,
    pub color: Option<Hsla>,
    pub hide_value: bool,
    pub hide_limits: bool,
}

impl Default for MeterConfig {
    fn default() -> Self {
        Self {
            component: String::new(),
            element: 0,
            label: None,
            // A 0..0 scale would divide by zero; unit scale is the least
            // surprising thing to draw before anyone sets a range.
            min: 0.0,
            max: 1.0,
            unit: None,
            orientation: Orientation::default(),
            shape: MeterShape::default(),
            color: None,
            hide_value: false,
            hide_limits: false,
        }
    }
}

/// Single-element bar meter.
#[derive(facet::Facet)]
pub struct Meter {
    /// Component and element edited by the inspector. Binding fields lead the page.
    pub source: crate::data_binding::Binding,
    pub element: usize,
    pub label: SharedString,
    pub min: f64,
    pub max: f64,
    pub unit: SharedString,
    pub orientation: Orientation,
    pub shape: MeterShape,
    pub color: Hsla,
    pub show_value: bool,
    pub show_limits: bool,
    #[facet(opaque)]
    live: LiveBinding<f64>,
}

impl ScalarInstrument for Meter {
    fn scalar(&mut self) -> (&mut SharedString, &mut SharedString, &mut LiveBinding<f64>) {
        (&mut self.label, &mut self.unit, &mut self.live)
    }
}

impl Meter {
    pub fn from_config(cfg: &MeterConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let source = crate::data_binding::Binding::from_text(&cfg.component, &db, cx);
        let at = ElementRef::new(source.id(), cfg.element);
        let meta = component_meta(&db, at.component);
        let mut live = LiveBinding::new(at, db, cx);
        stream_scalar(&mut live, at, cfg.label.is_some(), cfg.unit.is_some(), cx);
        Self {
            label: cfg
                .label
                .clone()
                .map(SharedString::from)
                .unwrap_or_else(|| default_label(&meta.element_names, at.element, &meta.name)),
            min: cfg.min,
            max: cfg.max,
            unit: cfg
                .unit
                .clone()
                .map(SharedString::from)
                .unwrap_or(meta.unit),
            orientation: cfg.orientation,
            shape: cfg.shape,
            color: cfg.color.unwrap_or_else(|| theme(cx).control_active),
            show_value: !cfg.hide_value,
            show_limits: !cfg.hide_limits,
            source,
            element: cfg.element,
            live,
        }
    }

    /// Where this meter currently reads from.
    fn at(&self) -> ElementRef {
        ElementRef::new(self.source.id(), self.element)
    }

    /// Restart the stream when the inspector has re-pointed the meter. Label
    /// and unit described the old component, so they are re-derived.
    fn rebind(&mut self, cx: &mut Context<Self>) {
        if let Some((at, meta)) = self.live.moved(&mut self.source, self.element, cx) {
            self.label = default_label(&meta.element_names, at.element, &meta.name);
            self.unit = meta.unit;
            stream_scalar(&mut self.live, at, false, false, cx);
        }
    }

    pub fn to_config(&self) -> MeterConfig {
        MeterConfig {
            // An expression's component is named by a content hash, so what
            // round-trips is the text of whatever is bound now.
            component: self.source.text(&self.live.db),
            element: self.element,
            label: Some(self.label.to_string()),
            min: self.min,
            max: self.max,
            unit: Some(self.unit.to_string()),
            orientation: self.orientation,
            shape: self.shape,
            color: Some(self.color),
            hide_value: !self.show_value,
            hide_limits: !self.show_limits,
        }
    }
}

/// The `(start, end)` of the fill along the track, each in `0..=1` measured
/// from the scale's low end.
///
/// A scale spanning zero is bipolar: the fill runs between the zero position
/// and the value, in whichever direction the value lies. Any other scale
/// fills from the low end.
pub(crate) fn fill_span(value: f64, min: f64, max: f64) -> (f32, f32) {
    let Some(v) = scale_position(value, min, max) else {
        return (0.0, 0.0);
    };
    match scale_position(0.0, min, max) {
        Some(zero) if min < 0.0 && max > 0.0 => (zero.min(v), zero.max(v)),
        _ => (0.0, v),
    }
}

/// Where `value` sits along a `min..max` scale, pinned to its ends; `None`
/// when the scale is degenerate.
pub(crate) fn scale_position(value: f64, min: f64, max: f64) -> Option<f32> {
    let span = max - min;
    if !span.is_finite() || span.abs() < f64::EPSILON {
        return None;
    }
    Some(((value - min) / span).clamp(0.0, 1.0) as f32)
}

/// A starting scale for a meter on `at`, derived from whatever limits the
/// control system declared for that element.
///
/// A meter that opens already showing its redline is worth far more than one
/// that opens at `0..1` and has to be retuned by hand. The scale runs to
/// [`SCALE_HEADROOM`] past the outermost limit so the redline sits inside the
/// track rather than exactly at its end, and stays symmetric about zero when
/// any limit is negative — signed quantities read wrong on a one-sided scale.
/// Falls back to unit scale when nothing is declared.
pub(crate) fn suggested_scale(at: ElementRef, cx: &App) -> (f64, f64) {
    let marks = binding::limit_marks(at, cx);
    let extent = marks.iter().map(|(v, _)| v.abs()).fold(0.0_f64, f64::max);
    if extent <= 0.0 || !extent.is_finite() {
        return (0.0, 1.0);
    }
    let bound = extent * SCALE_HEADROOM;
    if marks.iter().any(|(v, _)| *v < 0.0) {
        (-bound, bound)
    } else {
        (0.0, bound)
    }
}

/// How far past the outermost declared limit a suggested scale runs.
const SCALE_HEADROOM: f64 = 1.25;

/// Where a limit value sits along the track, or `None` when it falls outside
/// the meter's scale and would otherwise pin misleadingly to an end.
pub(crate) fn limit_position(value: f64, min: f64, max: f64) -> Option<f32> {
    let span = max - min;
    if !span.is_finite() || span.abs() < f64::EPSILON {
        return None;
    }
    let t = (value - min) / span;
    (0.0..=1.0).contains(&t).then_some(t as f32)
}

impl Render for Meter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rebind(cx);
        let theme = theme(cx);
        let at = self.at();
        let vertical = self.orientation == Orientation::Vertical;

        let (min, max) = (self.min, self.max);
        let color = self.color;
        let track_color = theme.border_primary;
        let tint = binding::alarm_tint(at, cx);
        let marks: Vec<(f32, Hsla)> = if self.show_limits {
            binding::limit_marks(at, cx)
                .into_iter()
                .filter_map(|(v, c)| limit_position(v, min, max).map(|t| (t, c)))
                .collect()
        } else {
            Vec::new()
        };
        let span = self.live.value.map(|v| fill_span(v, min, max));

        let shape = self.shape;
        let rim_color = theme.bg_primary;

        let bar = canvas(
            move |bounds, _window, _cx| bounds,
            move |_, bounds, window, _cx| {
                if let Some(tint) = tint {
                    window.paint_quad(fill(bounds, tint));
                }
                let outline = Outline::new(shape, bounds, vertical);
                outline.paint(track_color, window);
                if let Some((start, end)) = span
                    && end > start
                {
                    let mask = slice(outline.scale, start, end, vertical);
                    window.with_content_mask(Some(ContentMask { bounds: mask }), |window| {
                        outline.paint(color, window)
                    });
                }
                for (t, mark_color) in &marks {
                    let band = tick(outline.scale, *t, vertical);
                    window.with_content_mask(Some(ContentMask { bounds: band }), |window| {
                        outline.paint(*mark_color, window)
                    });
                }
                outline.paint_rim(rim_color, window);
            },
        );

        // A horizontal bar is a thin strip under the label; every other
        // shape is a silhouette that needs the tile's height to be legible.
        let bar = if vertical || shape != MeterShape::Bar {
            div().flex_1().min_h_0().w_full().child(bar.size_full())
        } else {
            div()
                .w_full()
                .h(ui_px(BAR_THICKNESS))
                .child(bar.size_full())
        };

        let mut tile = div()
            .size_full()
            .flex()
            .flex_col()
            .gap(ui_px(3.0))
            .p(ui_px(6.0))
            .text_size(ui_px(10.0));

        if !self.label.is_empty() {
            tile = tile.child(
                div()
                    .text_color(theme.text_secondary)
                    .truncate()
                    .child(self.label.clone()),
            );
        }

        tile = tile.child(bar);

        if self.show_value {
            let value = match self.live.value {
                Some(v) => format_number(v),
                None => "—".to_string(),
            };
            // The number never shrinks: when the line is too narrow the unit
            // truncates away first, since the reading is the one thing that
            // has to stay legible.
            let mut reading = div()
                .flex()
                .items_baseline()
                .gap(ui_px(2.0))
                .overflow_hidden()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.text_primary)
                        .child(SharedString::from(value)),
                );
            if !self.unit.is_empty() {
                reading = reading.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(ui_px(9.0))
                        .text_color(theme.text_tertiary)
                        .child(self.unit.clone()),
                );
            }
            tile = tile.child(reading);
        }

        tile
    }
}

/// The sub-rectangle of `bounds` between two fractions of the track, measured
/// from the low end of the scale — which is the bottom when vertical.
fn slice(bounds: Bounds<Pixels>, start: f32, end: f32, vertical: bool) -> Bounds<Pixels> {
    if vertical {
        let h = bounds.size.height;
        Bounds::new(
            point(bounds.origin.x, bounds.origin.y + h * (1.0 - end)),
            size(bounds.size.width, h * (end - start)),
        )
    } else {
        let w = bounds.size.width;
        Bounds::new(
            point(bounds.origin.x + w * start, bounds.origin.y),
            size(w * (end - start), bounds.size.height),
        )
    }
}

/// A thin band across the track at one fraction of the scale.
fn tick(bounds: Bounds<Pixels>, t: f32, vertical: bool) -> Bounds<Pixels> {
    if vertical {
        let y = bounds.origin.y + bounds.size.height * (1.0 - t) - px(TICK_PX / 2.0);
        Bounds::new(
            point(bounds.origin.x, y),
            size(bounds.size.width, px(TICK_PX)),
        )
    } else {
        let x = bounds.origin.x + bounds.size.width * t - px(TICK_PX / 2.0);
        Bounds::new(
            point(x, bounds.origin.y),
            size(px(TICK_PX), bounds.size.height),
        )
    }
}

/// A meter's bounds seen along its axis, so each shape is described once —
/// standing upright — and lies on its side for a horizontal meter.
///
/// `along` runs from the scale's low end (bottom, or left), `across` from the
/// left edge (or top).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Track {
    bounds: Bounds<Pixels>,
    vertical: bool,
}

impl Track {
    fn length(&self) -> Pixels {
        if self.vertical {
            self.bounds.size.height
        } else {
            self.bounds.size.width
        }
    }

    fn breadth(&self) -> Pixels {
        if self.vertical {
            self.bounds.size.width
        } else {
            self.bounds.size.height
        }
    }

    fn point(&self, along: Pixels, across: Pixels) -> Point<Pixels> {
        let o = self.bounds.origin;
        if self.vertical {
            point(o.x + across, o.y + self.bounds.size.height - along)
        } else {
            point(o.x + along, o.y + across)
        }
    }

    /// The rectangle covering `along` × `across`, each a `(from, to)` pair.
    fn span(&self, along: (Pixels, Pixels), across: (Pixels, Pixels)) -> Bounds<Pixels> {
        let (a, b) = (self.point(along.0, across.0), self.point(along.1, across.1));
        Bounds::from_corners(
            point(a.x.min(b.x), a.y.min(b.y)),
            point(a.x.max(b.x), a.y.max(b.y)),
        )
    }
}

/// One filled part of a shape's outline.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Piece {
    Rounded {
        bounds: Bounds<Pixels>,
        radius: Pixels,
    },
    /// A tube whose ends are half-ellipses `cap` deep along the axis.
    Cylinder { track: Track, cap: Pixels },
}

/// A [`MeterShape`] laid out in a meter's bounds: the pieces that paint its
/// silhouette, and the rectangle its scale maps onto.
#[derive(Clone, Debug, PartialEq)]
struct Outline {
    pieces: SmallVec<[Piece; 2]>,
    /// The region `0..=1` of the scale spans along the axis. Fill slices and
    /// limit ticks are cut from this, then clipped to the silhouette.
    scale: Bounds<Pixels>,
}

impl Outline {
    fn new(shape: MeterShape, bounds: Bounds<Pixels>, vertical: bool) -> Self {
        let track = Track { bounds, vertical };
        let (length, breadth) = (track.length(), track.breadth());
        let rounded = |bounds: Bounds<Pixels>, radius: Pixels| Piece::Rounded { bounds, radius };
        let half_min = |b: Bounds<Pixels>| b.size.width.min(b.size.height) / 2.0;
        match shape {
            MeterShape::Bar => Self {
                pieces: SmallVec::from_slice(&[rounded(bounds, px(TRACK_RADIUS))]),
                scale: bounds,
            },
            MeterShape::Tank => Self {
                pieces: SmallVec::from_slice(&[rounded(bounds, half_min(bounds))]),
                scale: bounds,
            },
            MeterShape::Cylinder => {
                let cap = (breadth * CYLINDER_CAP).min(length / 4.0);
                Self {
                    pieces: SmallVec::from_slice(&[Piece::Cylinder { track, cap }]),
                    scale: bounds,
                }
            }
            MeterShape::Sphere => {
                let d = bounds.size.width.min(bounds.size.height);
                let circle = Bounds::centered_at(bounds.center(), size(d, d));
                Self {
                    pieces: SmallVec::from_slice(&[rounded(circle, d / 2.0)]),
                    scale: circle,
                }
            }
            MeterShape::Battery => {
                let terminal = length * TERMINAL_LENGTH;
                let inset = breadth * (1.0 - TERMINAL_BREADTH) / 2.0;
                let body = track.span((px(0.0), length - terminal), (px(0.0), breadth));
                let nub = track.span((length - terminal, length), (inset, breadth - inset));
                Self {
                    pieces: SmallVec::from_slice(&[
                        rounded(body, px(TRACK_RADIUS).min(half_min(body))),
                        rounded(nub, px(TRACK_RADIUS / 2.0).min(half_min(nub))),
                    ]),
                    scale: body,
                }
            }
        }
    }

    fn paint(&self, color: Hsla, window: &mut Window) {
        for piece in &self.pieces {
            match *piece {
                Piece::Rounded { bounds, radius } => {
                    let mut quad = fill(bounds, color);
                    quad.corner_radii = gpui::Corners::all(radius);
                    window.paint_quad(quad);
                }
                Piece::Cylinder { track, cap } => {
                    if let Ok(path) = cylinder_path(track, cap).build() {
                        window.paint_path(path, color);
                    }
                }
            }
        }
    }

    /// The near edge of a cylinder's high-end ellipse — the line that turns
    /// a flat silhouette into a tube seen from slightly above. Other shapes
    /// have no rim.
    fn paint_rim(&self, color: Hsla, window: &mut Window) {
        for piece in &self.pieces {
            let Piece::Cylinder { track, cap } = *piece else {
                continue;
            };
            let (length, half) = (track.length(), track.breadth() / 2.0);
            let center = (length - cap, half);
            let zero = px(0.0);
            let mut path = PathBuilder::stroke(px(RIM_PX));
            path.move_to(track.point(center.0, zero));
            quarter(&mut path, track, center, (zero, -half), (-cap, zero));
            quarter(&mut path, track, center, (-cap, zero), (zero, half));
            if let Ok(path) = path.build() {
                window.paint_path(path, color);
            }
        }
    }
}

/// A tube with half-elliptical ends, traced in the track's frame so it lies
/// on its side for a horizontal meter.
fn cylinder_path(track: Track, cap: Pixels) -> PathBuilder {
    let (length, half) = (track.length(), track.breadth() / 2.0);
    let low = (cap, half);
    let high = (length - cap, half);
    let zero = px(0.0);
    let mut path = PathBuilder::fill();
    path.move_to(track.point(cap, zero));
    path.line_to(track.point(length - cap, zero));
    quarter(&mut path, track, high, (zero, -half), (cap, zero));
    quarter(&mut path, track, high, (cap, zero), (zero, half));
    path.line_to(track.point(cap, half * 2.0));
    quarter(&mut path, track, low, (zero, half), (-cap, zero));
    quarter(&mut path, track, low, (-cap, zero), (zero, -half));
    path.close();
    path
}

/// A quarter ellipse about `center` from `center + from` to `center + to`,
/// with every offset an `(along, across)` pair.
///
/// Built from a cubic rather than an SVG arc: the track's frame flips the
/// vertical axis, which would invert an arc's sweep flag, but maps Bézier
/// control points exactly.
fn quarter(
    path: &mut PathBuilder,
    track: Track,
    center: (Pixels, Pixels),
    from: (Pixels, Pixels),
    to: (Pixels, Pixels),
) {
    let at = |along: Pixels, across: Pixels| track.point(center.0 + along, center.1 + across);
    path.cubic_bezier_to(
        at(to.0, to.1),
        at(from.0 + to.0 * QUARTER_ARC, from.1 + to.1 * QUARTER_ARC),
        at(to.0 + from.0 * QUARTER_ARC, to.1 + from.1 * QUARTER_ARC),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_needle_points_at_the_value_on_a_bipolar_scale() {
        assert_eq!(scale_position(50.0, -100.0, 100.0), Some(0.75));
        assert_eq!(scale_position(-100.0, -100.0, 100.0), Some(0.0));
        assert_eq!(scale_position(500.0, -100.0, 100.0), Some(1.0));
        assert_eq!(scale_position(1.0, 5.0, 5.0), None);
    }

    #[test]
    fn unipolar_fills_from_the_low_end() {
        assert_eq!(fill_span(0.0, 0.0, 100.0), (0.0, 0.0));
        assert_eq!(fill_span(25.0, 0.0, 100.0), (0.0, 0.25));
        assert_eq!(fill_span(100.0, 0.0, 100.0), (0.0, 1.0));
    }

    #[test]
    fn bipolar_fills_from_zero_in_both_directions() {
        assert_eq!(fill_span(0.0, -1.0, 1.0), (0.5, 0.5));
        assert_eq!(fill_span(0.5, -1.0, 1.0), (0.5, 0.75));
        assert_eq!(fill_span(-0.5, -1.0, 1.0), (0.25, 0.5));
        assert_eq!(fill_span(-1.0, -1.0, 1.0), (0.0, 0.5));
    }

    #[test]
    fn values_clamp_to_the_scale() {
        assert_eq!(fill_span(500.0, 0.0, 100.0), (0.0, 1.0));
        assert_eq!(fill_span(-500.0, 0.0, 100.0), (0.0, 0.0));
        assert_eq!(fill_span(9.0, -1.0, 1.0), (0.5, 1.0));
    }

    #[test]
    fn degenerate_scale_paints_nothing() {
        assert_eq!(fill_span(1.0, 5.0, 5.0), (0.0, 0.0));
        assert_eq!(limit_position(1.0, 5.0, 5.0), None);
    }

    fn area(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn a_bar_paints_the_same_rounded_track_it_always_did() {
        let b = area(10.0, 20.0, 30.0, 200.0);
        let outline = Outline::new(MeterShape::Bar, b, true);
        assert_eq!(outline.scale, b);
        assert_eq!(
            outline.pieces.as_slice(),
            &[Piece::Rounded {
                bounds: b,
                radius: px(TRACK_RADIUS)
            }]
        );
    }

    #[test]
    fn a_sphere_stays_round_and_centered() {
        for (b, vertical) in [
            (area(0.0, 0.0, 40.0, 200.0), true),
            (area(0.0, 0.0, 200.0, 40.0), false),
        ] {
            let outline = Outline::new(MeterShape::Sphere, b, vertical);
            let circle = outline.scale;
            assert_eq!(circle.size, size(px(40.0), px(40.0)));
            assert_eq!(circle.center(), b.center());
            assert_eq!(
                outline.pieces.as_slice(),
                &[Piece::Rounded {
                    bounds: circle,
                    radius: px(20.0)
                }]
            );
        }
    }

    #[test]
    fn a_tank_rounds_its_ends_fully() {
        let outline = Outline::new(MeterShape::Tank, area(0.0, 0.0, 200.0, 30.0), false);
        let [Piece::Rounded { radius, .. }] = outline.pieces.as_slice() else {
            panic!("one rounded piece");
        };
        assert_eq!(*radius, px(15.0));
    }

    #[test]
    fn a_battery_terminal_sits_past_the_scale_at_the_high_end() {
        let up = Outline::new(MeterShape::Battery, area(0.0, 0.0, 40.0, 100.0), true);
        assert_eq!(up.scale, area(0.0, 8.0, 40.0, 92.0));
        let Piece::Rounded { bounds: nub, .. } = up.pieces[1] else {
            panic!("rounded terminal");
        };
        assert_eq!(nub, area(11.0, 0.0, 18.0, 8.0));

        let side = Outline::new(MeterShape::Battery, area(0.0, 0.0, 100.0, 40.0), false);
        assert_eq!(side.scale, area(0.0, 0.0, 92.0, 40.0));
        let Piece::Rounded { bounds: nub, .. } = side.pieces[1] else {
            panic!("rounded terminal");
        };
        assert_eq!(nub, area(92.0, 11.0, 8.0, 18.0));
    }

    #[test]
    fn a_cylinder_cap_never_eats_the_tube() {
        let squat = Outline::new(MeterShape::Cylinder, area(0.0, 0.0, 100.0, 40.0), true);
        let [Piece::Cylinder { cap, .. }] = squat.pieces.as_slice() else {
            panic!("one cylinder");
        };
        assert_eq!(*cap, px(10.0));
        assert!(
            cylinder_path(
                Track {
                    bounds: squat.scale,
                    vertical: true
                },
                *cap
            )
            .build()
            .is_ok()
        );
    }

    #[test]
    fn the_track_frame_lies_a_shape_on_its_side() {
        let up = Track {
            bounds: area(10.0, 10.0, 20.0, 100.0),
            vertical: true,
        };
        assert_eq!(up.point(px(0.0), px(0.0)), point(px(10.0), px(110.0)));
        assert_eq!(up.point(px(100.0), px(20.0)), point(px(30.0), px(10.0)));
        let side = Track {
            bounds: area(10.0, 10.0, 100.0, 20.0),
            vertical: false,
        };
        assert_eq!(side.point(px(0.0), px(0.0)), point(px(10.0), px(10.0)));
        assert_eq!(side.point(px(100.0), px(20.0)), point(px(110.0), px(30.0)));
    }

    #[test]
    fn limits_outside_the_scale_are_dropped() {
        assert_eq!(limit_position(0.5, 0.0, 1.0), Some(0.5));
        assert_eq!(limit_position(2.0, 0.0, 1.0), None);
        assert_eq!(limit_position(-2.0, 0.0, 1.0), None);
    }
}
