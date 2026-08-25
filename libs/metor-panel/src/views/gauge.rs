//! Circular arc gauge.
//!
//! The round sibling of [`Meter`](super::Meter) — same binding, same
//! alarm-derived limits, same bipolar fill rule (it reuses
//! [`fill_span`](super::meter::fill_span) outright), drawn as a sweep instead
//! of a bar. A dial reads faster than a bar when the operator cares about
//! *where in the range* a value sits rather than how much of it is used, and
//! it packs a wide range into a square tile.
//!
//! Arcs are drawn as sampled polylines rather than through
//! [`PathBuilder::arc_to`]. The SVG arc flags (`large_arc`/`sweep`) are easy
//! to get subtly wrong across the 180° boundary, and at [`ARC_STEP_DEG`] the
//! polyline is indistinguishable from a true arc.

use std::sync::Arc;

use gpui::{
    Bounds, Context, Hsla, IntoElement, PathBuilder, Pixels, Point, SharedString, Window, canvas,
    div, point, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};

use super::binding::{self, ElementRef, component_meta, spawn_meta_resolver, spawn_scalar_stream};
use super::format_number;
use super::meter::fill_span;
use crate::theme::theme;

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
/// Thickness of the dial's track and value arc.
const ARC_WIDTH: f32 = 7.0;
/// Fraction of the radius a limit tick extends inward from the track.
const TICK_REACH: f32 = 0.18;
/// Default total sweep — wide enough to read a range, open enough at the
/// bottom to leave room for the value.
const DEFAULT_SWEEP_DEG: f64 = 240.0;

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
    pub component_id: ComponentId,
    pub element: usize,
    pub label: SharedString,
    pub min: f64,
    pub max: f64,
    pub unit: SharedString,
    pub sweep_degrees: f64,
    pub style: GaugeStyle,
    pub color: Hsla,
    pub show_value: bool,
    pub show_limits: bool,
    /// Fallback name for a component nothing has registered;
    /// [`to_config`](Self::to_config) prefers the live name for the bound id.
    #[facet(skip)]
    component: SharedString,
    /// What `_task` is actually streaming, compared against the editable
    /// fields each frame.
    #[facet(opaque)]
    bound: Option<ElementRef>,
    #[facet(skip)]
    value: Option<f64>,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _expression: Option<crate::dynamic::expressions::Expression>,
    #[facet(opaque)]
    _task: gpui::Task<()>,
    #[facet(opaque)]
    _resolver_task: gpui::Task<()>,
}

impl Gauge {
    pub fn from_config(cfg: &GaugeConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let bound = crate::dynamic::expressions::bind(&cfg.component, &db, cx).ok();
        let component_id = bound
            .as_ref()
            .map(|bound| bound.id)
            .unwrap_or_else(|| ComponentId::new(&cfg.component));
        let expression = bound.and_then(|bound| bound.expression);
        let element = cfg.element;
        let meta = component_meta(&db, component_id);

        let task =
            spawn_scalar_stream(db.clone(), component_id, element, cx, |gauge, value, cx| {
                gauge.value = Some(value);
                cx.notify();
            });

        let keep_label = cfg.label.is_some();
        let keep_unit = cfg.unit.is_some();
        let resolver_task =
            spawn_meta_resolver(db.clone(), component_id, cx, move |gauge, meta, cx| {
                if !keep_label {
                    gauge.label =
                        super::meter::default_label(&meta.element_names, element, &meta.name);
                }
                if !keep_unit {
                    gauge.unit = meta.unit;
                }
                cx.notify();
            });

        Self {
            label: cfg
                .label
                .clone()
                .map(SharedString::from)
                .unwrap_or_else(|| {
                    super::meter::default_label(&meta.element_names, element, &meta.name)
                }),
            min: cfg.min,
            max: cfg.max,
            unit: cfg
                .unit
                .clone()
                .map(SharedString::from)
                .unwrap_or(meta.unit),
            sweep_degrees: cfg.sweep_degrees,
            style: cfg.style,
            color: cfg.color.unwrap_or_else(|| theme(cx).control_active),
            show_value: !cfg.hide_value,
            show_limits: !cfg.hide_limits,
            component: SharedString::from(cfg.component.clone()),
            component_id,
            element,
            bound: Some(ElementRef::new(component_id, element)),
            value: None,
            db,
            _expression: expression,
            _task: task,
            _resolver_task: resolver_task,
        }
    }

    /// Where this dial currently reads from.
    fn at(&self) -> ElementRef {
        ElementRef::new(self.component_id, self.element)
    }

    /// Restart the stream when the inspector has re-pointed the dial. Label
    /// and unit are re-derived: they described the old component, and a dial
    /// labelled `ω x` reading wheel momentum is worse than no label.
    fn rebind(&mut self, cx: &mut Context<Self>) {
        let want = self.at();
        if !binding::rebound(want, &mut self.bound) {
            return;
        }
        self._expression = crate::dynamic::expressions::running(want.component, cx);
        let element = want.element;
        let meta = component_meta(&self.db, want.component);
        self.label = super::meter::default_label(&meta.element_names, element, &meta.name);
        self.unit = meta.unit;
        self.component = meta.name;
        self.value = None;
        self._task = spawn_scalar_stream(
            self.db.clone(),
            want.component,
            element,
            cx,
            |gauge, value, cx| {
                gauge.value = Some(value);
                cx.notify();
            },
        );
        self._resolver_task = spawn_meta_resolver(
            self.db.clone(),
            want.component,
            cx,
            move |gauge, meta, cx| {
                gauge.label = super::meter::default_label(&meta.element_names, element, &meta.name);
                gauge.unit = meta.unit;
                cx.notify();
            },
        );
    }

    pub fn to_config(&self) -> GaugeConfig {
        GaugeConfig {
            // Resolve the name for whatever is bound *now*, so a rebind is
            // what gets saved.
            // An expression's component is named by a content hash and
            // labelled with the text that made it, so what round-trips is the
            // text — a name would rehydrate onto nothing.
            component: crate::dynamic::expressions::binding_text(&self.db, self.component_id)
                .or_else(|| {
                    binding::component_name(&self.db, self.component_id).map(|n| n.to_string())
                })
                .unwrap_or_else(|| self.component.to_string()),
            element: self.element,
            label: Some(self.label.to_string()),
            min: self.min,
            max: self.max,
            unit: Some(self.unit.to_string()),
            sweep_degrees: self.sweep_degrees,
            style: self.style,
            color: Some(self.color),
            hide_value: !self.show_value,
            hide_limits: !self.show_limits,
        }
    }

    pub fn component(&self) -> &SharedString {
        &self.component
    }
}

/// A dial's resolved geometry for one frame: where the hub sits, how far the
/// scale reaches, and how wide it sweeps.
#[derive(Clone, Copy)]
struct Dial {
    center: Point<Pixels>,
    radius: f32,
    sweep: f32,
}

impl Dial {
    /// Fit a dial of `sweep` radians into `bounds`.
    ///
    /// Sized against the arc's true extent rather than the tile's inscribed
    /// circle: a 240° dial is wider than it is tall, and fitting it to a
    /// circle would waste a third of the tile.
    fn fit(bounds: Bounds<Pixels>, sweep: f32) -> Self {
        let half = (sweep / 2.0).clamp(0.0, std::f32::consts::PI);
        // Widest point is the sweep end, unless the sweep passes through the
        // horizontal, where it reaches the full radius.
        let width_factor = 2.0
            * if half >= std::f32::consts::FRAC_PI_2 {
                1.0
            } else {
                half.sin()
            };
        // Top of the arc is always -r; the bottom is wherever the sweep ends.
        let height_factor = 1.0 - half.cos();
        let w = f32::from(bounds.size.width);
        let h = f32::from(bounds.size.height);
        let radius = (w / width_factor.max(f32::EPSILON))
            .min(h / height_factor.max(f32::EPSILON))
            .max(0.0);
        let center = point(
            bounds.origin.x + bounds.size.width / 2.0,
            bounds.origin.y + px(radius) + (bounds.size.height - px(radius * height_factor)) / 2.0,
        );
        Self {
            center,
            radius,
            sweep,
        }
    }

    /// Angle of a scale fraction, in radians clockwise from straight up.
    ///
    /// `t = 0` sits at `-sweep/2` and `t = 1` at `+sweep/2`, so the dial is
    /// always symmetric about vertical however wide its sweep.
    fn angle_at(&self, t: f32) -> f32 {
        (t - 0.5) * self.sweep
    }

    /// The point at scale fraction `t`, `radius` out from the hub. Screen
    /// coordinates, so "up" is negative y.
    fn at(&self, t: f32, radius: f32) -> Point<Pixels> {
        let angle = self.angle_at(t);
        point(
            self.center.x + px(radius * angle.sin()),
            self.center.y - px(radius * angle.cos()),
        )
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

impl Render for Gauge {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rebind(cx);
        let theme = theme(cx);
        let at = self.at();
        let sweep_rad = (self.sweep_degrees.clamp(1.0, 360.0) as f32).to_radians();
        let (min, max) = (self.min, self.max);
        let color = self.color;
        let style = self.style;
        let track_color = theme.border_primary;
        let hub_color = theme.text_primary;
        let tint = binding::alarm_tint(at, cx);
        let span = self.value.map(|v| fill_span(v, min, max));
        let marks: Vec<(f32, Hsla)> = if self.show_limits {
            binding::limit_marks(at, cx)
                .into_iter()
                .filter_map(|(v, c)| super::meter::limit_position(v, min, max).map(|t| (t, c)))
                .collect()
        } else {
            Vec::new()
        };

        let dial = canvas(
            move |bounds, _window, _cx| bounds,
            move |_, bounds, window, _cx| {
                if let Some(tint) = tint {
                    window.paint_quad(gpui::fill(bounds, tint));
                }
                let dial = Dial::fit(bounds, sweep_rad);
                if dial.radius <= 0.0 {
                    return;
                }
                let arc_r = dial.radius - ARC_WIDTH / 2.0;
                dial.paint_arc(arc_r, 0.0, 1.0, px(ARC_WIDTH), track_color, window);

                if let Some((start, end)) = span {
                    match style {
                        GaugeStyle::Arc => {
                            dial.paint_arc(arc_r, start, end, px(ARC_WIDTH), color, window)
                        }
                        GaugeStyle::Needle => {
                            // The needle points at the value, which for a
                            // bipolar scale is whichever end of the span
                            // isn't the zero origin.
                            let t = if start > 0.0 { start } else { end };
                            dial.paint_spoke(t, 0.0, arc_r - ARC_WIDTH, px(2.0), color, window);
                        }
                    }
                }

                for (t, mark_color) in &marks {
                    dial.paint_spoke(
                        *t,
                        arc_r + ARC_WIDTH / 2.0,
                        arc_r - dial.radius * TICK_REACH,
                        px(2.0),
                        *mark_color,
                        window,
                    );
                }

                if style == GaugeStyle::Needle {
                    let hub = Bounds::new(
                        point(dial.center.x - px(3.0), dial.center.y - px(3.0)),
                        gpui::size(px(6.0), px(6.0)),
                    );
                    let mut quad = gpui::fill(hub, hub_color);
                    quad.corner_radii = gpui::Corners::all(px(3.0));
                    window.paint_quad(quad);
                }
            },
        );

        let mut tile = div()
            .size_full()
            .relative()
            .bg(theme.bg_primary)
            .p(px(4.0))
            .child(dial.size_full());

        // Readouts overlay the dial's open bottom rather than taking layout
        // space, so the arc keeps the whole tile.
        let mut readout = div()
            .absolute()
            .left_0()
            .right_0()
            .bottom(px(4.0))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(1.0));

        if self.show_value {
            let text = match self.value {
                Some(v) if self.unit.is_empty() => format_number(v),
                Some(v) => format!("{} {}", format_number(v), self.unit),
                None => "—".to_string(),
            };
            readout = readout.child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::from(text)),
            );
        }
        if !self.label.is_empty() {
            readout = readout.child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_secondary)
                    .truncate()
                    .child(self.label.clone()),
            );
        }

        tile = tile.child(readout);
        tile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SWEEP_240: f32 = 240.0 * std::f32::consts::PI / 180.0;

    /// A dial at the origin, so `at()` returns plain offsets from the hub.
    fn hub_dial(sweep: f32) -> Dial {
        Dial {
            center: point(px(0.0), px(0.0)),
            radius: 10.0,
            sweep,
        }
    }

    #[test]
    fn scale_ends_are_symmetric_about_vertical() {
        let d = hub_dial(SWEEP_240);
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
        };
        let p = d.at(0.5, 10.0);
        assert!((f32::from(p.x) - 50.0).abs() < 1e-4);
        assert!((f32::from(p.y) - 40.0).abs() < 1e-4);
    }

    #[test]
    fn scale_ends_fall_below_the_hub_for_a_wide_sweep() {
        let d = hub_dial(SWEEP_240);
        let (lo, hi) = (d.at(0.0, 10.0), d.at(1.0, 10.0));
        // 240° opens downward: both ends sit below centre, mirrored in x.
        assert!(f32::from(lo.y) > 0.0 && f32::from(hi.y) > 0.0);
        assert!(f32::from(lo.x) < 0.0 && f32::from(hi.x) > 0.0);
        assert!((f32::from(lo.x) + f32::from(hi.x)).abs() < 1e-4);
    }

    #[test]
    fn a_half_sweep_is_a_semicircle_above_the_hub() {
        let d = hub_dial(std::f32::consts::PI);
        let (lo, hi) = (d.at(0.0, 10.0), d.at(1.0, 10.0));
        assert!(f32::from(lo.y).abs() < 1e-4 && f32::from(hi.y).abs() < 1e-4);
        assert!((f32::from(lo.x) + 10.0).abs() < 1e-4);
        assert!((f32::from(hi.x) - 10.0).abs() < 1e-4);
    }

    #[test]
    fn a_fitted_dial_stays_inside_its_tile() {
        let bounds = Bounds::new(point(px(0.0), px(0.0)), gpui::size(px(120.0), px(120.0)));
        for sweep_deg in [90.0_f32, 180.0, 240.0, 300.0, 360.0] {
            let d = Dial::fit(bounds, sweep_deg.to_radians());
            assert!(d.radius > 0.0, "sweep {sweep_deg}");
            for i in 0..=100 {
                let p = d.at(i as f32 / 100.0, d.radius);
                assert!(
                    f32::from(p.x) >= -0.01 && f32::from(p.x) <= 120.01,
                    "sweep {sweep_deg} x {p:?}"
                );
                assert!(
                    f32::from(p.y) >= -0.01 && f32::from(p.y) <= 120.01,
                    "sweep {sweep_deg} y {p:?}"
                );
            }
        }
    }

    #[test]
    fn arc_sampling_scales_with_the_span() {
        assert!(arc_steps(SWEEP_240, 1.0) > arc_steps(SWEEP_240, 0.1));
        assert_eq!(arc_steps(SWEEP_240, 0.0), 2);
    }
}
