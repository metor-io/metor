//! Circular arc gauge.
//!
//! The round sibling of [`Meter`](super::Meter) — same binding, same
//! alarm-derived limits, same bipolar fill rule (it reuses
//! [`fill_span`](super::meter::fill_span) outright), drawn as a sweep instead
//! of a bar. A dial reads faster than a bar when the operator cares about
//! *where in the range* a value sits rather than how much of it is used, and
//! it packs a wide range into a square tile.
//!
//! The look follows the Crew Dragon displays: one thick band over part of
//! an invisible circle, turned to whichever side of that circle suits the
//! layout, with the readout set large in the circle's middle. Gaps cut the
//! band at each division, so it reads as a bar of segments; the span up to
//! the value is lit in the dial's color and the rest is the same color held
//! back to the low alpha the panel's buttons and chips use for an accent's
//! background. Faint dashes inside the band repeat the scale, and limits
//! stand out as colored dashes among them. The circle, not the arc, is what
//! the readout belongs to, so a short arc tucked into one corner still
//! frames a centered value.
//!
//! Arcs are sampled into polylines rather than built with
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
use crate::theme::{Theme, theme, ui_px, ui_scale};

/// How the value is shown along the dial.
#[derive(Clone, Copy, Debug, Default, PartialEq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum GaugeStyle {
    /// The band lit from the scale's origin to the value.
    #[default]
    Arc,
    /// A pointer from the hub to the value.
    Needle,
}

/// Angular resolution of a sampled arc.
const ARC_STEP_DEG: f64 = 2.0;
/// Thickness of the band, as a share of the radius.
const FILL_SHARE: f32 = 0.055;
/// Thinnest the band gets, in ui pixels, however small the dial.
const FILL_MIN: f32 = 2.0;
/// Alpha of the band's unlit segments: the accent-background alpha the
/// panel's buttons and chips use, so the track reads as the same material.
const TRACK_ALPHA: f32 = 0.18;
/// Alpha of the scale dashes. The gaps already mark divisions; the dashes
/// only repeat them, so they stay barely there.
const DASH_ALPHA: f32 = 0.3;
/// Narrowest division gap, in ui pixels: a cut thinner than this stops
/// reading as a gap on a small dial.
const GAP_MIN: f32 = 2.0;
/// Division gap as a share of the band's thickness, so a large dial's cuts
/// keep their proportion to the band.
const GAP_SHARE: f32 = 0.5;
/// Largest share of a division a gap may take, so dense divisions on a
/// short arc still leave more band than gap.
const GAP_MAX_SHARE: f32 = 0.3;
/// Clearance between the band's inner edge and the scale dashes, in radii —
/// wide enough to read as space, so the dashes float inside the band
/// rather than hang off it.
const DASH_INSET: f32 = 0.04;
/// Length of a division dash, in radii.
const DIVISION_DASH: f32 = 0.03;
/// Length of a subdivision dash, in radii.
const MINOR_DASH: f32 = 0.015;
/// Clearance between the band and a limit dash, in radii. Tighter than the
/// scale's, so a limit reads as pointing at the band.
const LIMIT_INSET: f32 = 0.015;
/// Length of a limit dash, in radii.
const LIMIT_DASH: f32 = 0.07;
/// Subdivisions closer than this many ui pixels apart are left off: past
/// that density they read as texture rather than a scale.
const MINOR_MIN_SPACING: f32 = 6.0;
/// Deepest any arc decoration reaches toward the hub, in radii. The fit keeps
/// the arc's ends at this depth on screen as well as its outer edge.
const INNER_REACH: f32 = 1.0 - FILL_SHARE - LIMIT_INSET - LIMIT_DASH;
/// Half-extent of the readout block around the hub, in radii. Its corners
/// (about 0.74 out) clear the band and the dashes inside it.
const READOUT_HALF: (f32, f32) = (0.62, 0.4);
/// Readout lines as (vertical offset from the hub, font size), in radii.
const LABEL_LINE: (f32, f32) = (-0.3, 0.12);
const VALUE_LINE: (f32, f32) = (0.0, 0.34);
const UNIT_LINE: (f32, f32) = (0.3, 0.16);
/// Smallest readout text, in ui pixels, however small the dial gets.
const MIN_TEXT: f32 = 7.0;
/// Default total sweep — from half past seven over the top to half past
/// four, open at the bottom to leave room for the value.
const DEFAULT_SWEEP_DEG: f64 = 270.0;
/// Default divisions: sixths, which on the default sweep land a division at
/// nine and three o'clock.
const DEFAULT_DIVISIONS: usize = 6;
/// Default subdivisions inside each division: none, as on the cockpit dials.
const DEFAULT_MINOR_TICKS: usize = 0;

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
    /// The scale's equal parts: gaps in the lit band, dashes along the unlit
    /// track. `0` leaves the band whole and draws no subdivisions either.
    pub divisions: usize,
    /// Short dashes splitting each division evenly along the unlit track.
    /// Dropped where they would crowd together.
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
        let mut font = window.text_style().font();
        font.weight = gpui::FontWeight::LIGHT;
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
    /// Nothing: only the track and its dashes take space.
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

/// A run of the scale between two fractions, `lo <= hi`.
type Span = (f32, f32);

/// The stretches of lit band the division gaps leave, each gap `gap` wide in
/// scale fractions and centred on its division point.
///
/// The scale's ends are flush — the band simply stops there — unless the
/// dial is `closed` into a full circle, where the ends meet at a division
/// point and get half a gap each so the seam reads like any other cut.
fn segments(divisions: usize, gap: f32, closed: bool) -> Vec<Span> {
    let n = divisions.max(1);
    let half = if divisions == 0 { 0.0 } else { gap / 2.0 };
    (0..n)
        .map(|i| {
            let lo = i as f32 / n as f32;
            let hi = (i + 1) as f32 / n as f32;
            let lo_cut = if i > 0 || closed { half } else { 0.0 };
            let hi_cut = if i + 1 < n || closed { half } else { 0.0 };
            (lo + lo_cut, hi - hi_cut)
        })
        .collect()
}

/// The parts of the scale the dim track covers: everything but the lit span,
/// so a lit segment never sits over a translucent one.
fn unlit(lit: Option<Span>) -> Vec<Span> {
    let Some((a, b)) = lit else {
        return vec![(0.0, 1.0)];
    };
    [(0.0, a), (b, 1.0)]
        .into_iter()
        .filter(|(lo, hi)| hi > lo)
        .collect()
}

/// Scale fractions of the dashes along the track, each flagged `true` for a
/// division. The scale's ends get none — the track stopping marks them.
fn dashes(divisions: usize, minor: usize) -> impl Iterator<Item = (f32, bool)> {
    let per_division = minor + 1;
    let total = divisions * per_division;
    (1..total).map(move |i| (i as f32 / total as f32, i % per_division == 0))
}

/// A dial's track and lit band resolved for one frame, in pixels: where the
/// gaps and dashes fall.
struct Band {
    /// Radius of the track line, and the centreline of the lit band.
    track: f32,
    width: f32,
    segments: Vec<Span>,
    dashes: Vec<(f32, bool)>,
}

impl Band {
    /// Size the band for `dial`. Gap width is fixed in pixels and converted to scale fractions along the track, so they
    /// read the same on a short arc as on a long one.
    fn new(dial: &Dial, divisions: usize, minor: usize, ui_scale: f32) -> Self {
        let width = (dial.radius * FILL_SHARE).max(FILL_MIN * ui_scale);
        let track = dial.radius - width / 2.0;
        let length = track * dial.sweep;
        let closed = dial.sweep >= std::f32::consts::TAU - 1e-4;
        let mut gap = 0.0;
        let mut minor = minor;
        if divisions > 0 && length > 0.0 {
            let gap_px = (GAP_MIN * ui_scale).max(GAP_SHARE * width);
            gap = (gap_px / length).min(GAP_MAX_SHARE / divisions as f32);
            let spacing = length / (divisions * (minor + 1)) as f32;
            if spacing < MINOR_MIN_SPACING * ui_scale {
                minor = 0;
            }
        }
        Self {
            track,
            width,
            segments: segments(divisions, gap, closed),
            dashes: dashes(divisions, minor).collect(),
        }
    }

    fn inner(&self) -> f32 {
        self.track - self.width / 2.0
    }

    /// Where a scale dash starts, a clear step inside the band.
    fn dash_root(&self, dial: &Dial) -> f32 {
        self.inner() - dial.radius * DASH_INSET
    }

    /// Where a limit dash starts, close under the band.
    fn limit_root(&self, dial: &Dial) -> f32 {
        self.inner() - dial.radius * LIMIT_INSET
    }

    /// The parts of the band's segments between scale fractions `from` and
    /// `to` — what painting that span covers, gaps left out.
    fn pieces(&self, from: f32, to: f32) -> impl Iterator<Item = Span> + '_ {
        self.segments
            .iter()
            .map(move |&(lo, hi)| (lo.max(from), hi.min(to)))
            .filter(|(a, b)| b > a)
    }

    /// Fill the band between scale fractions `from` and `to`, one closed
    /// outline per segment so each end is a square radial cut.
    fn paint_span(&self, dial: &Dial, from: f32, to: f32, color: Hsla, window: &mut Window) {
        let (outer, inner) = (self.track + self.width / 2.0, self.inner());
        for (a, b) in self.pieces(from, to) {
            let steps = arc_steps(dial.sweep, b - a);
            let mut path = PathBuilder::fill();
            path.move_to(dial.at(a, outer));
            for i in 1..=steps {
                path.line_to(dial.at(a + (b - a) * (i as f32 / steps as f32), outer));
            }
            for i in 0..=steps {
                path.line_to(dial.at(b - (b - a) * (i as f32 / steps as f32), inner));
            }
            path.close();
            if let Ok(p) = path.build() {
                window.paint_path(p, color);
            }
        }
    }
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
        // The track keeps the dial's own color while the fill turns with an
        // alarm: the scale stays put, and only the lit span — the part that
        // means "the value" — changes, so raising an alarm doesn't repaint
        // the whole ring.
        let track_color = Theme::dim(self.color, TRACK_ALPHA * self.color.a);
        let dash_color = Theme::dim(theme.text_secondary, DASH_ALPHA);
        let hub_color = theme.text_primary;
        let text_color = theme.text_primary;
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
        let (divisions, minor) = (self.divisions, self.minor_ticks);
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
                let band = Band::new(&dial, divisions, minor, k);
                let lit = match style {
                    GaugeStyle::Arc => span.filter(|(a, b)| b > a),
                    GaugeStyle::Needle => None,
                };
                for (a, b) in unlit(lit) {
                    band.paint_span(&dial, a, b, track_color, window);
                }
                let root = band.dash_root(&dial);
                for &(t, division) in &band.dashes {
                    let reach = if division { DIVISION_DASH } else { MINOR_DASH };
                    let tip = root - dial.radius * reach;
                    dial.paint_spoke(t, root, tip, px(k), dash_color, window);
                }

                match style {
                    GaugeStyle::Arc => {
                        if let Some((start, end)) = lit {
                            band.paint_span(&dial, start, end, color, window);
                        }
                    }
                    GaugeStyle::Needle => {
                        if let Some(t) = needle {
                            let tip = band.inner();
                            dial.paint_spoke(t, 0.0, tip, px(2.0 * k), color, window);
                        }
                    }
                }

                let limit_root = band.limit_root(&dial);
                for (t, mark_color) in &marks {
                    let tip = limit_root - dial.radius * LIMIT_DASH;
                    dial.paint_spoke(*t, limit_root, tip, px(1.5 * k), *mark_color, window);
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

                readout.paint(&dial, text_color, text_color, window, cx);
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

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn gaps_cut_the_band_at_each_division() {
        let s = segments(4, 0.02, false);
        assert_eq!(s.len(), 4);
        assert!(near(s[0].0, 0.0) && near(s[3].1, 1.0), "ends flush: {s:?}");
        for (i, pair) in s.windows(2).enumerate() {
            let (left, right) = (pair[0].1, pair[1].0);
            let division = (i + 1) as f32 / 4.0;
            assert!(near(right - left, 0.02), "gap width {pair:?}");
            assert!(near((left + right) / 2.0, division), "gap centred {pair:?}");
        }
    }

    #[test]
    fn a_full_circle_cuts_its_seam_too() {
        let s = segments(3, 0.02, true);
        assert!(near(s[0].0, 0.01) && near(s[2].1, 0.99), "{s:?}");
    }

    #[test]
    fn zero_divisions_leaves_the_band_whole() {
        assert_eq!(segments(0, 0.02, false), vec![(0.0, 1.0)]);
        assert_eq!(segments(0, 0.02, true), vec![(0.0, 1.0)]);
        assert_eq!(dashes(0, 3).count(), 0);
    }

    #[test]
    fn dashes_mark_interior_divisions_and_subdivisions() {
        let d: Vec<_> = dashes(4, 1).collect();
        assert_eq!(d.len(), 7, "no dash at either end: {d:?}");
        assert_eq!(d[0], (0.125, false));
        assert_eq!(d[1], (0.25, true));
        assert_eq!(d.iter().filter(|(_, division)| *division).count(), 3);
        assert!(dashes(6, 0).all(|(_, division)| division));
    }

    fn dial_of(radius: f32, sweep: f32) -> Dial {
        Dial {
            radius,
            ..hub_dial(sweep, 0.0)
        }
    }

    const SWEEP_270: f32 = 270.0 * std::f32::consts::PI / 180.0;

    #[test]
    fn the_band_is_a_thick_stroke_centred_on_the_track() {
        let d = dial_of(200.0, SWEEP_270);
        let band = Band::new(&d, 6, 0, 1.0);
        assert!(near(band.width, 200.0 * FILL_SHARE));
        assert!(
            near(band.track + band.width / 2.0, d.radius),
            "outer edge on the rim"
        );
        let tiny = Band::new(&dial_of(10.0, SWEEP_270), 6, 0, 1.0);
        assert!(near(tiny.width, FILL_MIN));
    }

    #[test]
    fn gaps_hold_their_pixel_width_and_never_swamp_a_division() {
        let d = dial_of(40.0, FRAC_PI_2);
        let band = Band::new(&d, 5, 0, 1.0);
        let length = band.track * d.sweep;
        let gap = (band.segments[1].0 - band.segments[0].1) * length;
        assert!((gap - GAP_MIN).abs() < 1e-3, "{gap}");

        let dense = Band::new(&dial_of(30.0, FRAC_PI_2), 20, 9, 1.0);
        for &(lo, hi) in &dense.segments {
            assert!(hi - lo >= (1.0 - GAP_MAX_SHARE) / 20.0 - 1e-5, "{lo}..{hi}");
        }
        assert!(
            dense.dashes.iter().all(|(_, division)| *division),
            "crowded subdivisions are dropped"
        );
    }

    #[test]
    fn the_fill_lights_segments_and_respects_gaps() {
        let band = Band::new(&dial_of(100.0, SWEEP_240), 4, 0, 1.0);
        let lit: Vec<Span> = band.pieces(0.0, 0.6).collect();
        assert_eq!(lit.len(), 3, "{lit:?}");
        assert_eq!(lit[0], band.segments[0]);
        assert_eq!(lit[1], band.segments[1]);
        assert!(near(lit[2].0, band.segments[2].0) && near(lit[2].1, 0.6));
        let gap_mid = (band.segments[0].1 + band.segments[1].0) / 2.0;
        assert_eq!(band.pieces(0.0, gap_mid).count(), 1);
        let mid: Vec<Span> = band.pieces(0.5, 0.6).collect();
        assert!(mid.len() == 1 && near(mid[0].1, 0.6));
    }

    #[test]
    fn the_dim_track_is_the_rest_of_the_segmented_band() {
        let band = Band::new(&dial_of(100.0, SWEEP_270), 6, 0, 1.0);
        assert_eq!(unlit(Some((0.0, 0.4))), vec![(0.4, 1.0)]);
        assert_eq!(unlit(None), vec![(0.0, 1.0)]);
        assert_eq!(unlit(Some((0.5, 0.6))), vec![(0.0, 0.5), (0.6, 1.0)]);
        assert!(unlit(Some((0.0, 1.0))).is_empty());

        // Lit and dim pieces together tile the segments exactly: the track
        // carries the same gaps, and meets the fill mid-segment with no seam.
        let lit = (0.0, 0.4);
        let mut all: Vec<Span> = band.pieces(lit.0, lit.1).collect();
        for (a, b) in unlit(Some(lit)) {
            all.extend(band.pieces(a, b));
        }
        all.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut merged: Vec<Span> = Vec::new();
        for (a, b) in all {
            match merged.last_mut() {
                Some(last) if near(last.1, a) => last.1 = b,
                _ => merged.push((a, b)),
            }
        }
        assert_eq!(merged.len(), band.segments.len());
        for (m, s) in merged.iter().zip(&band.segments) {
            assert!(near(m.0, s.0) && near(m.1, s.1), "{m:?} vs {s:?}");
        }
    }

    #[test]
    fn dashes_float_inside_the_band_and_clear_the_readout() {
        let d = dial_of(100.0, SWEEP_270);
        let band = Band::new(&d, 6, 0, 1.0);
        let root = band.dash_root(&d);
        assert!(
            band.inner() - root >= band.width / 2.0,
            "a visible step between band and dashes"
        );
        assert!(band.limit_root(&d) > root, "limits sit closer to the band");
        let deepest = (band.limit_root(&d) - d.radius * LIMIT_DASH) / d.radius;
        assert!(deepest >= INNER_REACH - 1e-5);
        assert!((root - d.radius * DIVISION_DASH) / d.radius >= INNER_REACH - 1e-5);
        let (hw, hh) = READOUT_HALF;
        assert!(
            deepest > (hw * hw + hh * hh).sqrt(),
            "readout corners stay clear"
        );
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
        assert_eq!(GaugeConfig::default().sweep_degrees, 270.0);
        assert_eq!(cfg.minor_ticks, DEFAULT_MINOR_TICKS);
    }
}
