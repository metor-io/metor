//! Circular arc gauge.
//!
//! The round sibling of [`Meter`](super::Meter) — same binding, same
//! alarm-derived limits, same bipolar fill rule (it reuses
//! [`fill_span`](super::meter::fill_span) outright), drawn as a sweep instead
//! of a bar. A dial reads faster than a bar when the operator cares about
//! *where in the range* a value sits rather than how much of it is used, and
//! it packs a wide range into a square tile.
//!
//! The look follows cockpit dials: a thin notched arc over part of an
//! invisible circle, turned to whichever side of that circle suits the
//! layout, with the readout set large in the circle's middle. The circle, not
//! the arc, is what the readout belongs to, so a short arc tucked into one
//! corner still frames a centered value.
//!
//! Arcs are drawn as sampled polylines rather than through
//! [`PathBuilder::arc_to`]. The SVG arc flags (`large_arc`/`sweep`) are easy
//! to get subtly wrong across the 180° boundary, and at [`ARC_STEP_DEG`] the
//! polyline is indistinguishable from a true arc.

use std::f32::consts::FRAC_PI_2;
use std::sync::Arc;

use gpui::{
    App, Bounds, Context, Font, Hsla, IntoElement, PathBuilder, Pixels, Point, SharedString,
    TextAlign, TextRun, Window, canvas, div, point, prelude::*, px,
};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use super::binding::{
    self, ElementRef, LiveBinding, ScalarInstrument, component_meta, default_label, stream_scalar,
};
use super::format_number;
use super::meter::{fill_span, scale_position};
use crate::inspect;
use crate::theme::{theme, ui_px, ui_scale};

/// How the value is shown along the dial.
#[derive(Clone, Copy, Debug, Default, PartialEq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum GaugeStyle {
    /// A thick arc filled from the scale's origin to the value.
    #[default]
    Arc,
    /// A pointer from the hub to the value.
    Needle,
}

/// Angular resolution of a sampled arc.
const ARC_STEP_DEG: f64 = 2.0;
/// Thickness of the dial's track and value arc, before the radius caps it.
const ARC_WIDTH: f32 = 7.0;
/// Largest share of the radius the track may take, so a small dial stays a
/// thin ring instead of a disc.
const ARC_WIDTH_MAX: f32 = 0.1;
/// Fraction of the radius a limit mark extends inward from the track.
const TICK_REACH: f32 = 0.18;
/// Fraction of the radius a major notch extends inward from the track.
const MAJOR_NOTCH: f32 = 0.09;
/// Fraction of the radius a minor notch extends inward from the track.
const MINOR_NOTCH: f32 = 0.05;
/// Deepest any arc decoration reaches toward the hub, in radii. The fit keeps
/// the arc's ends at this depth on screen as well as its outer edge.
const INNER_REACH: f32 = 1.0 - ARC_WIDTH_MAX - TICK_REACH;
/// Half-extent of the readout block around the hub, in radii. Major notches
/// stop at `1 - ARC_WIDTH_MAX - MAJOR_NOTCH`, outside this box's corners.
const READOUT_HALF: (f32, f32) = (0.62, 0.4);
/// Readout lines as (vertical offset from the hub, font size), in radii.
const LABEL_LINE: (f32, f32) = (-0.3, 0.12);
const VALUE_LINE: (f32, f32) = (0.0, 0.34);
const UNIT_LINE: (f32, f32) = (0.28, 0.12);
/// Smallest readout text, in ui pixels, however small the dial gets.
const MIN_TEXT: f32 = 7.0;
/// Default total sweep — wide enough to read a range, open enough at the
/// bottom to leave room for the value.
const DEFAULT_SWEEP_DEG: f64 = 240.0;
/// Default major divisions: a notch every fifth of the scale.
const DEFAULT_DIVISIONS: usize = 5;
/// Default minor notches between each pair of majors.
const DEFAULT_MINOR_TICKS: usize = 1;

/// Persisted shape of a [`Gauge`], shared by the tile and dashboard surfaces.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct GaugeConfig {
    pub component: String,
    pub element: usize,
    pub label: Option<String>,
    pub min: f64,
    pub max: f64,
    pub unit: Option<String>,
    pub sweep_degrees: f64,
    /// Where the middle of the arc points, clockwise from straight up. `0`
    /// keeps the dial symmetric about vertical, as dials saved before this
    /// field existed were.
    pub rotation_degrees: f64,
    /// Major notches split the scale into this many equal parts; `0` draws
    /// none, minor notches included.
    pub divisions: usize,
    pub minor_ticks: usize,
    pub style: GaugeStyle,
    pub color: Option<Hsla>,
    pub hide_value: bool,
    pub hide_limits: bool,
}

impl Default for GaugeConfig {
    fn default() -> Self {
        Self {
            component: String::new(),
            element: 0,
            label: None,
            min: 0.0,
            max: 1.0,
            unit: None,
            sweep_degrees: DEFAULT_SWEEP_DEG,
            rotation_degrees: 0.0,
            divisions: DEFAULT_DIVISIONS,
            minor_ticks: DEFAULT_MINOR_TICKS,
            style: GaugeStyle::default(),
            color: None,
            hide_value: false,
            hide_limits: false,
        }
    }
}

/// Single-element dial.
#[derive(facet::Facet)]
pub struct Gauge {
    /// What this dial reads. Editable: picking another component or element
    /// in the inspector rebinds the stream on the next frame. Declared first
    /// so the binding heads the inspector page — fields are walked in order.
    pub source: crate::data_binding::Binding,
    pub element: usize,
    pub label: SharedString,
    pub min: f64,
    pub max: f64,
    pub unit: SharedString,
    #[facet(inspect::label = "Sweep")]
    #[facet(inspect::range(min = "30", max = "360"))]
    pub sweep_degrees: f64,
    #[facet(inspect::label = "Rotation")]
    #[facet(inspect::range(min = "-180", max = "180"))]
    pub rotation_degrees: f64,
    #[facet(inspect::range(min = "0", max = "20"))]
    pub divisions: usize,
    #[facet(inspect::label = "Minor ticks")]
    #[facet(inspect::range(min = "0", max = "9"))]
    pub minor_ticks: usize,
    pub style: GaugeStyle,
    pub color: Hsla,
    pub show_value: bool,
    pub show_limits: bool,
    #[facet(opaque)]
    live: LiveBinding<f64>,
}

impl ScalarInstrument for Gauge {
    fn scalar(&mut self) -> (&mut SharedString, &mut SharedString, &mut LiveBinding<f64>) {
        (&mut self.label, &mut self.unit, &mut self.live)
    }
}

impl Gauge {
    pub fn from_config(cfg: &GaugeConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
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
            sweep_degrees: cfg.sweep_degrees,
            rotation_degrees: cfg.rotation_degrees,
            divisions: cfg.divisions,
            minor_ticks: cfg.minor_ticks,
            style: cfg.style,
            color: cfg.color.unwrap_or_else(|| theme(cx).control_active),
            show_value: !cfg.hide_value,
            show_limits: !cfg.hide_limits,
            source,
            element: cfg.element,
            live,
        }
    }

    /// Where this dial currently reads from.
    fn at(&self) -> ElementRef {
        ElementRef::new(self.source.id(), self.element)
    }

    /// Restart the stream when the inspector has re-pointed the dial. Label
    /// and unit described the old component, so they are re-derived.
    fn rebind(&mut self, cx: &mut Context<Self>) {
        if let Some((at, meta)) = self.live.moved(&mut self.source, self.element, cx) {
            self.label = default_label(&meta.element_names, at.element, &meta.name);
            self.unit = meta.unit;
            stream_scalar(&mut self.live, at, false, false, cx);
        }
    }

    pub fn to_config(&self) -> GaugeConfig {
        GaugeConfig {
            // An expression's component is named by a content hash, so what
            // round-trips is the text of whatever is bound now.
            component: self.source.text(&self.live.db),
            element: self.element,
            label: Some(self.label.to_string()),
            min: self.min,
            max: self.max,
            unit: Some(self.unit.to_string()),
            sweep_degrees: self.sweep_degrees,
            rotation_degrees: self.rotation_degrees,
            divisions: self.divisions,
            minor_ticks: self.minor_ticks,
            style: self.style,
            color: Some(self.color),
            hide_value: !self.show_value,
            hide_limits: !self.show_limits,
        }
    }

    /// The text set in the middle of the circle this frame.
    fn readout(&self) -> Readout {
        let label = (!self.label.is_empty()).then(|| self.label.to_uppercase().into());
        if !self.show_value {
            return Readout {
                label,
                value: None,
                unit: None,
            };
        }
        let value = match self.live.value {
            Some(v) => format_number(v),
            None => "—".to_string(),
        };
        Readout {
            label,
            value: Some(value.into()),
            unit: (!self.unit.is_empty()).then(|| self.unit.clone()),
        }
    }
}

/// The lines of a dial's readout: label in small capitals over the value,
/// unit beneath it.
struct Readout {
    label: Option<SharedString>,
    value: Option<SharedString>,
    unit: Option<SharedString>,
}

impl Readout {
    fn is_empty(&self) -> bool {
        self.label.is_none() && self.value.is_none() && self.unit.is_none()
    }

    /// Set each line at its slot around the hub, sized to the dial and shrunk
    /// to the readout box when it would spill past it.
    fn paint(&self, dial: &Dial, color: Hsla, dim: Hsla, window: &mut Window, cx: &mut App) {
        let font = window.text_style().font();
        let min_size = MIN_TEXT * ui_scale(window);
        let lines = [
            (&self.label, LABEL_LINE, dim),
            (&self.value, VALUE_LINE, color),
            (&self.unit, UNIT_LINE, dim),
        ];
        for (text, (offset, size), color) in lines {
            let Some(text) = text else { continue };
            let center = point(dial.center.x, dial.center.y + px(offset * dial.radius));
            let fit = TextFit {
                size: (size * dial.radius).max(min_size),
                max_width: 2.0 * READOUT_HALF.0 * dial.radius,
                min_size,
            };
            paint_centered(text, fit, center, font.clone(), color, window, cx);
        }
    }
}

/// How large a readout line wants to be and how far it may shrink to fit.
struct TextFit {
    size: f32,
    max_width: f32,
    min_size: f32,
}

/// Shape `text` and paint it centered on `center`, reshaping smaller once if
/// it is wider than the fit allows.
fn paint_centered(
    text: &SharedString,
    fit: TextFit,
    center: Point<Pixels>,
    font: Font,
    color: Hsla,
    window: &mut Window,
    cx: &mut App,
) {
    let run = TextRun {
        len: text.len(),
        font,
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let mut size = fit.size;
    let mut shaped =
        window
            .text_system()
            .shape_line(text.clone(), px(size), std::slice::from_ref(&run), None);
    let width = f32::from(shaped.width);
    if width > fit.max_width && size > fit.min_size {
        size = (size * fit.max_width / width).max(fit.min_size);
        shaped = window
            .text_system()
            .shape_line(text.clone(), px(size), &[run], None);
    }
    let origin = point(center.x - shaped.width / 2.0, center.y - px(size / 2.0));
    let _ = shaped.paint(origin, px(size), TextAlign::Left, None, window, cx);
}

/// What has to fit at the hub besides the arc itself.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hub {
    /// Nothing: only the arc and its notches take space.
    Empty,
    /// The needle's pivot, a point.
    Pivot,
    /// The readout block, [`READOUT_HALF`] around the hub.
    Readout,
}

/// A dial's resolved geometry for one frame: where the hub sits, how far the
/// scale reaches, how wide it sweeps and which way it faces.
#[derive(Clone, Copy)]
struct Dial {
    center: Point<Pixels>,
    radius: f32,
    sweep: f32,
    rotation: f32,
}

/// Axis-aligned extent in radii from the hub, screen-oriented (y down).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Extent {
    min: (f32, f32),
    max: (f32, f32),
}

impl Extent {
    fn include(&mut self, (x, y): (f32, f32)) {
        self.min = (self.min.0.min(x), self.min.1.min(y));
        self.max = (self.max.0.max(x), self.max.1.max(y));
    }

    fn size(&self) -> (f32, f32) {
        (self.max.0 - self.min.0, self.max.1 - self.min.1)
    }
}

/// Unit-circle point at `angle` radians clockwise from up, `reach` radii out.
fn polar(angle: f32, reach: f32) -> (f32, f32) {
    (reach * angle.sin(), -reach * angle.cos())
}

impl Dial {
    /// Fit a dial of `sweep` radians, its middle turned `rotation` radians
    /// clockwise from up, into `bounds`.
    ///
    /// Sized against what is actually drawn rather than the tile's inscribed
    /// circle: a 240° dial is wider than it is tall, and a 90° arc in one
    /// corner needs only a corner of the circle — plus the hub, when the
    /// readout or needle lives there.
    fn fit(bounds: Bounds<Pixels>, sweep: f32, rotation: f32, hub: Hub) -> Self {
        let extent = Self::extent(sweep, rotation, hub);
        let (ew, eh) = extent.size();
        let w = f32::from(bounds.size.width);
        let h = f32::from(bounds.size.height);
        let radius = (w / ew.max(f32::EPSILON))
            .min(h / eh.max(f32::EPSILON))
            .max(0.0);
        let center = point(
            bounds.origin.x + px((w - ew * radius) / 2.0 - extent.min.0 * radius),
            bounds.origin.y + px((h - eh * radius) / 2.0 - extent.min.1 * radius),
        );
        Self {
            center,
            radius,
            sweep,
            rotation,
        }
    }

    /// Everything a unit dial paints: the arc's outer edge (its ends, and any
    /// compass point it passes through, where it bulges furthest), how deep
    /// its ends reach inward, and whatever sits at the hub.
    fn extent(sweep: f32, rotation: f32, hub: Hub) -> Extent {
        let sweep = sweep.clamp(0.0, std::f32::consts::TAU);
        let (lo, hi) = (rotation - sweep / 2.0, rotation + sweep / 2.0);
        let start = polar(lo, 1.0);
        let mut extent = Extent {
            min: start,
            max: start,
        };
        for angle in [lo, hi] {
            extent.include(polar(angle, 1.0));
            extent.include(polar(angle, INNER_REACH));
        }
        let first = (lo / FRAC_PI_2).ceil() as i32;
        let last = (hi / FRAC_PI_2).floor() as i32;
        for quarter in first..=last {
            extent.include(polar(quarter as f32 * FRAC_PI_2, 1.0));
        }
        match hub {
            Hub::Empty => {}
            Hub::Pivot => extent.include((0.0, 0.0)),
            Hub::Readout => {
                let (hw, hh) = READOUT_HALF;
                extent.include((-hw, -hh));
                extent.include((hw, hh));
            }
        }
        extent
    }

    /// Angle of a scale fraction, in radians clockwise from straight up.
    ///
    /// `t = 0.5` points along the rotation, with the ends `sweep/2` either
    /// side, so an unrotated dial is symmetric about vertical.
    fn angle_at(&self, t: f32) -> f32 {
        self.rotation + (t - 0.5) * self.sweep
    }

    /// The point at scale fraction `t`, `radius` out from the hub. Screen
    /// coordinates, so "up" is negative y.
    fn at(&self, t: f32, radius: f32) -> Point<Pixels> {
        let (x, y) = polar(self.angle_at(t), radius);
        point(self.center.x + px(x), self.center.y + px(y))
    }

    /// Stroke an arc between two scale fractions as a sampled polyline.
    fn paint_arc(
        &self,
        radius: f32,
        from: f32,
        to: f32,
        width: Pixels,
        color: Hsla,
        window: &mut Window,
    ) {
        if to <= from {
            return;
        }
        let steps = arc_steps(self.sweep, to - from);
        let mut path = PathBuilder::stroke(width);
        for i in 0..=steps {
            let t = from + (to - from) * (i as f32 / steps as f32);
            let p = self.at(t, radius);
            if i == 0 {
                path.move_to(p);
            } else {
                path.line_to(p);
            }
        }
        if let Ok(p) = path.build() {
            window.paint_path(p, color);
        }
    }

    /// Stroke a straight run along the radius at scale fraction `t`.
    fn paint_spoke(
        &self,
        t: f32,
        from: f32,
        to: f32,
        width: Pixels,
        color: Hsla,
        window: &mut Window,
    ) {
        let mut path = PathBuilder::stroke(width);
        path.move_to(self.at(t, from));
        path.line_to(self.at(t, to));
        if let Ok(p) = path.build() {
            window.paint_path(p, color);
        }
    }
}

/// Segment count for an arc covering `fraction` of `sweep_rad`, at least two
/// so a hairline span still draws.
fn arc_steps(sweep_rad: f32, fraction: f32) -> usize {
    let degrees = (sweep_rad * fraction).to_degrees() as f64;
    ((degrees / ARC_STEP_DEG).ceil() as usize).max(2)
}

/// Scale fractions of the gradation notches, each flagged `true` for a major.
///
/// `divisions` majors-to-majors spans, `minor` notches evenly inside each.
/// Both ends of the scale get a major, so a full circle doubles up at the
/// seam — harmless, since the two strokes coincide.
fn notches(divisions: usize, minor: usize) -> impl Iterator<Item = (f32, bool)> {
    let per_division = minor + 1;
    let total = divisions * per_division;
    (0..=total)
        .filter(move |_| divisions > 0)
        .map(move |i| (i as f32 / total as f32, i % per_division == 0))
}

impl Render for Gauge {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rebind(cx);
        let theme = theme(cx);
        let at = self.at();
        let sweep_rad = (self.sweep_degrees.clamp(1.0, 360.0) as f32).to_radians();
        let rotation_rad = (self.rotation_degrees as f32).to_radians();
        let (min, max) = (self.min, self.max);
        // The value arc takes the alarm's color while one is raised, so the
        // dial says "out of family" even where the wash is easy to miss.
        let color = binding::active_severity(at, cx)
            .map(|s| theme.alarm_color(crate::alarms::severity_index(s)))
            .unwrap_or(self.color);
        let style = self.style;
        let track_color = theme.border_primary;
        let major_color = theme.text_secondary;
        let minor_color = theme.text_tertiary;
        let hub_color = theme.text_primary;
        let value_color = theme.text_primary;
        let caption_color = theme.text_secondary;
        let tint = binding::alarm_tint(at, cx);
        let span = self.live.value.map(|v| fill_span(v, min, max));
        let needle = self.live.value.and_then(|v| scale_position(v, min, max));
        let marks: Vec<(f32, Hsla)> = if self.show_limits {
            binding::limit_marks(at, cx)
                .into_iter()
                .filter_map(|(v, c)| super::meter::limit_position(v, min, max).map(|t| (t, c)))
                .collect()
        } else {
            Vec::new()
        };
        let notches: Vec<(f32, bool)> = notches(self.divisions, self.minor_ticks).collect();
        let readout = self.readout();
        let hub = if !readout.is_empty() {
            Hub::Readout
        } else if style == GaugeStyle::Needle {
            Hub::Pivot
        } else {
            Hub::Empty
        };

        let dial = canvas(
            move |bounds, _window, _cx| bounds,
            move |_, bounds, window, cx| {
                if let Some(tint) = tint {
                    window.paint_quad(gpui::fill(bounds, tint));
                }
                let dial = Dial::fit(bounds, sweep_rad, rotation_rad, hub);
                if dial.radius <= 0.0 {
                    return;
                }
                let k = ui_scale(window);
                let arc_w = (ARC_WIDTH * k).min(dial.radius * ARC_WIDTH_MAX);
                let arc_r = dial.radius - arc_w / 2.0;
                let inner = dial.radius - arc_w;
                dial.paint_arc(arc_r, 0.0, 1.0, px(arc_w), track_color, window);

                for &(t, major) in &notches {
                    let (reach, color, width) = if major {
                        (MAJOR_NOTCH, major_color, 1.5 * k)
                    } else {
                        (MINOR_NOTCH, minor_color, k)
                    };
                    let to = inner - dial.radius * reach;
                    dial.paint_spoke(t, inner, to, px(width), color, window);
                }

                match style {
                    GaugeStyle::Arc => {
                        if let Some((start, end)) = span {
                            dial.paint_arc(arc_r, start, end, px(arc_w), color, window);
                        }
                    }
                    GaugeStyle::Needle => {
                        if let Some(t) = needle {
                            dial.paint_spoke(t, 0.0, arc_r - arc_w, px(2.0 * k), color, window);
                        }
                    }
                }

                for (t, mark_color) in &marks {
                    dial.paint_spoke(
                        *t,
                        dial.radius,
                        arc_r - dial.radius * TICK_REACH,
                        px(2.0 * k),
                        *mark_color,
                        window,
                    );
                }

                if style == GaugeStyle::Needle {
                    let r = px(3.0 * k);
                    let hub = Bounds::new(
                        point(dial.center.x - r, dial.center.y - r),
                        gpui::size(r * 2.0, r * 2.0),
                    );
                    let mut quad = gpui::fill(hub, hub_color);
                    quad.corner_radii = gpui::Corners::all(r);
                    window.paint_quad(quad);
                }

                readout.paint(&dial, value_color, caption_color, window, cx);
            },
        );

        div().size_full().p(ui_px(4.0)).child(dial.size_full())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SWEEP_240: f32 = 240.0 * std::f32::consts::PI / 180.0;

    /// A dial at the origin, so `at()` returns plain offsets from the hub.
    fn hub_dial(sweep: f32, rotation: f32) -> Dial {
        Dial {
            center: point(px(0.0), px(0.0)),
            radius: 10.0,
            sweep,
            rotation,
        }
    }

    fn close(a: Pixels, b: f32) -> bool {
        (f32::from(a) - b).abs() < 1e-3
    }

    #[test]
    fn scale_ends_are_symmetric_about_vertical() {
        let d = hub_dial(SWEEP_240, 0.0);
        let (lo, hi) = (d.angle_at(0.0), d.angle_at(1.0));
        assert!((lo + hi).abs() < 1e-5, "{lo} and {hi} should mirror");
        assert!(d.angle_at(0.5).abs() < 1e-6);
    }

    #[test]
    fn midscale_points_straight_up() {
        let d = Dial {
            center: point(px(50.0), px(50.0)),
            radius: 10.0,
            sweep: SWEEP_240,
            rotation: 0.0,
        };
        let p = d.at(0.5, 10.0);
        assert!(close(p.x, 50.0) && close(p.y, 40.0));
    }

    #[test]
    fn scale_ends_fall_below_the_hub_for_a_wide_sweep() {
        let d = hub_dial(SWEEP_240, 0.0);
        let (lo, hi) = (d.at(0.0, 10.0), d.at(1.0, 10.0));
        // 240° opens downward: both ends sit below centre, mirrored in x.
        assert!(f32::from(lo.y) > 0.0 && f32::from(hi.y) > 0.0);
        assert!(f32::from(lo.x) < 0.0 && f32::from(hi.x) > 0.0);
        assert!((f32::from(lo.x) + f32::from(hi.x)).abs() < 1e-4);
    }

    #[test]
    fn a_half_sweep_is_a_semicircle_above_the_hub() {
        let d = hub_dial(std::f32::consts::PI, 0.0);
        let (lo, hi) = (d.at(0.0, 10.0), d.at(1.0, 10.0));
        assert!(close(lo.y, 0.0) && close(hi.y, 0.0));
        assert!(close(lo.x, -10.0) && close(hi.x, 10.0));
    }

    #[test]
    fn rotation_turns_midscale_clockwise() {
        // Facing lower-left: the middle of the scale points down and left.
        let d = hub_dial(FRAC_PI_2, (-135.0_f32).to_radians());
        let mid = d.at(0.5, 10.0);
        let r = 10.0 / 2.0_f32.sqrt();
        assert!(close(mid.x, -r) && close(mid.y, r), "{mid:?}");
        // A 90° arc around lower-left runs from straight down to straight left.
        let (lo, hi) = (d.at(0.0, 10.0), d.at(1.0, 10.0));
        assert!(close(lo.x, 0.0) && close(lo.y, 10.0), "{lo:?}");
        assert!(close(hi.x, -10.0) && close(hi.y, 0.0), "{hi:?}");
    }

    #[test]
    fn old_dials_keep_their_extent() {
        // The default 240° dial spans the full width and reaches half a
        // radius below the hub; the readout box fits inside that.
        let e = Dial::extent(SWEEP_240, 0.0, Hub::Readout);
        let near = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert!(near(e.min.0, -1.0) && near(e.max.0, 1.0), "{e:?}");
        assert!(near(e.min.1, -1.0) && near(e.max.1, 0.5), "{e:?}");
    }

    #[test]
    fn a_short_arc_still_makes_room_for_the_readout() {
        let e = Dial::extent(FRAC_PI_2, 0.0, Hub::Readout);
        assert!(e.max.1 >= READOUT_HALF.1, "{e:?}");
        assert!(e.min.0 <= -READOUT_HALF.0 && e.max.0 >= READOUT_HALF.0);
        let bare = Dial::extent(FRAC_PI_2, 0.0, Hub::Empty);
        assert!(bare.max.1 < 0.0, "an empty hub needs no room: {bare:?}");
    }

    #[test]
    fn a_fitted_dial_stays_inside_its_tile() {
        let (w, h) = (160.0, 120.0);
        let bounds = Bounds::new(point(px(10.0), px(20.0)), gpui::size(px(w), px(h)));
        let inside = |p: Point<Pixels>| {
            let (x, y) = (f32::from(p.x), f32::from(p.y));
            (9.99..=10.01 + w).contains(&x) && (19.99..=20.01 + h).contains(&y)
        };
        for sweep_deg in [30.0_f32, 90.0, 140.0, 180.0, 240.0, 300.0, 360.0] {
            for rot_deg in [0.0_f32, 45.0, -90.0, -135.0, 180.0, 270.0] {
                for hub in [Hub::Empty, Hub::Pivot, Hub::Readout] {
                    let d = Dial::fit(bounds, sweep_deg.to_radians(), rot_deg.to_radians(), hub);
                    let case = format!("sweep {sweep_deg} rot {rot_deg} {hub:?}");
                    assert!(d.radius > 0.0, "{case}");
                    for i in 0..=100 {
                        let t = i as f32 / 100.0;
                        let outer = d.at(t, d.radius);
                        assert!(inside(outer), "{case} outer {outer:?}");
                    }
                    for t in [0.0, 1.0] {
                        let p = d.at(t, d.radius * INNER_REACH);
                        assert!(inside(p), "{case} inner {p:?}");
                    }
                    if hub == Hub::Readout {
                        let (hw, hh) = READOUT_HALF;
                        for (sx, sy) in [(-1.0, -1.0), (1.0, 1.0)] {
                            let corner = point(
                                d.center.x + px(sx * hw * d.radius),
                                d.center.y + px(sy * hh * d.radius),
                            );
                            assert!(inside(corner), "{case} readout {corner:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_fitted_dial_fills_one_axis() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), gpui::size(px(200.0), px(100.0)));
        let d = Dial::fit(bounds, FRAC_PI_2, (-135.0_f32).to_radians(), Hub::Readout);
        let e = Dial::extent(FRAC_PI_2, (-135.0_f32).to_radians(), Hub::Readout);
        let (ew, eh) = e.size();
        let used = (ew * d.radius, eh * d.radius);
        assert!((used.0 - 200.0).abs() < 1e-3 || (used.1 - 100.0).abs() < 1e-3);
    }

    #[test]
    fn notches_mark_divisions_and_subdivisions() {
        let n: Vec<_> = notches(4, 1).collect();
        assert_eq!(n.len(), 9);
        assert_eq!(n.iter().filter(|(_, major)| *major).count(), 5);
        assert_eq!(n[0], (0.0, true));
        assert_eq!(n[1], (0.125, false));
        assert_eq!(n[2], (0.25, true));
        assert_eq!(n[8], (1.0, true));

        let majors: Vec<_> = notches(5, 0).collect();
        assert_eq!(majors.len(), 6);
        assert!(majors.iter().all(|(_, major)| *major));
    }

    #[test]
    fn zero_divisions_turns_notches_off() {
        assert_eq!(notches(0, 3).count(), 0);
    }

    #[test]
    fn arc_sampling_scales_with_the_span() {
        assert!(arc_steps(SWEEP_240, 1.0) > arc_steps(SWEEP_240, 0.1));
        assert_eq!(arc_steps(SWEEP_240, 0.0), 2);
    }

    #[test]
    fn old_configs_parse_with_defaults() {
        let cfg: GaugeConfig =
            serde_json::from_str(r#"{"component":"a.b","sweep_degrees":200.0}"#).unwrap();
        assert_eq!(cfg.sweep_degrees, 200.0);
        assert_eq!(cfg.rotation_degrees, 0.0);
        assert_eq!(cfg.divisions, DEFAULT_DIVISIONS);
        assert_eq!(cfg.minor_ticks, DEFAULT_MINOR_TICKS);
    }
}
