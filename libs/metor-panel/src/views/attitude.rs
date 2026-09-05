//! Attitude ball with a body-vector compass.
//!
//! Reads a four-element quaternion component — the body←reference rotation,
//! `q_b_eci` or an estimator's `q_hat_b_eci` — and draws the classic
//! attitude director: a sky/ground ball pitched and rolled under a fixed
//! vehicle reticle, a roll scale, and a numeric roll/pitch/yaw readout.
//!
//! What makes it an *ADCS* instrument rather than an avionics clone is the
//! marker overlay. Pointing questions are almost never "what is my roll" but
//! "where is the sun / the field / nadir relative to me", so any number of
//! body-frame 3-vector components can be plotted on the same ball. Markers
//! ahead of the vehicle draw solid; ones behind draw hollow on the rim in
//! their bearing, because "the sun is behind you and to the left" is an
//! answer and a blank ball is not.
//!
//! Frames: the ball uses the aeronautical convention — the viewer looks along
//! body +X, screen right is +Y, screen down is +Z. Attitude is relative to
//! whatever frame the quaternion is expressed against, so on a target
//! publishing `q_b_eci` the ball reads against ECI, not a local horizon.
//!
//! The sky and ground are built as explicit polygons — the chord where the
//! horizon crosses the ball plus the arc on that side — rather than by
//! clipping a rectangle to a circle. Exact, no clip region needed, and the
//! geometry is a pure function worth testing.

use std::sync::Arc;

use gpui::{
    Bounds, Context, Entity, Hsla, IntoElement, PathBuilder, Pixels, Point, SharedString, Window,
    canvas, div, point, prelude::*, px,
};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::binding::{
    self, ElementRef, component_meta, spawn_elements_stream, spawn_meta_resolver,
};
use crate::theme::{Theme, theme};

/// Pitch angle that moves the horizon by a full ball radius.
const PITCH_FULL_SCALE: f32 = std::f32::consts::FRAC_PI_2;
/// Pitch ladder rung spacing. Coarse on purpose: a rung every 10° packs the
/// ball with more lines than anyone reads off it, and the horizon plus the
/// numeric readout already carry the precision.
const LADDER_STEP_DEG: f32 = 20.0;
/// Rung half-width as a fraction of the ball radius.
const LADDER_HALF_WIDTH: f32 = 0.28;
/// Points sampled along a ball arc.
const ARC_SAMPLES: usize = 48;
/// Marker dot radius.
const MARKER_PX: f32 = 3.5;
/// Alpha of the sky and ground fills. The ball is a backdrop for the markers
/// and the reticle, not the subject, so the hue carries at the border and
/// stays washed out across the interior.
const FILL_ALPHA: f32 = 0.18;
/// Alpha of a pitch ladder rung — between the fill it sits on and the
/// full-strength border, so it is legible without competing with either.
const LADDER_ALPHA: f32 = 0.55;

/// Persisted shape of one plotted body-frame direction.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct VectorMarkerConfig {
    pub component: String,
    pub label: String,
    pub color: Option<Hsla>,
}

/// Persisted shape of an [`AttitudeIndicator`], shared by the tile and
/// dashboard surfaces.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct AttitudeConfig {
    /// Quaternion component, four elements in `[x, y, z, w]` order.
    pub component: String,
    /// Index of the quaternion's first element, for a frame that carries it
    /// after other fields.
    pub element_offset: usize,
    pub label: Option<String>,
    pub vectors: Vec<VectorMarkerConfig>,
    pub hide_readout: bool,
}

/// One body-frame direction plotted on the ball.
#[derive(facet::Facet)]
pub struct VectorMarker {
    /// The 3-vector this marker plots. Editable, like the ball's own binding.
    pub component_id: ComponentId,
    pub label: SharedString,
    pub color: Option<Hsla>,
    /// Fallback name for a component nothing has registered.
    #[facet(skip)]
    component: SharedString,
    /// What `_task` is streaming, compared against `component_id` each frame.
    #[facet(opaque)]
    bound: Option<ComponentId>,
    /// Latest sample, unnormalized; `None` until the component produces one.
    #[facet(skip)]
    value: Option<[f32; 3]>,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _expression: Option<crate::dynamic::expressions::Expression>,
    #[facet(opaque)]
    _task: gpui::Task<()>,
}

impl VectorMarker {
    fn from_config(cfg: &VectorMarkerConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let bound = crate::dynamic::expressions::bind(&cfg.component, &db, cx).ok();
        let component_id = bound
            .as_ref()
            .map(|bound| bound.id)
            .unwrap_or_else(|| ComponentId::new(&cfg.component));
        let expression = bound.and_then(|bound| bound.expression);
        let task = spawn_marker_stream(&db, component_id, cx);
        Self {
            component_id,
            label: SharedString::from(cfg.label.clone()),
            color: cfg.color,
            component: SharedString::from(cfg.component.clone()),
            bound: Some(component_id),
            value: None,
            db,
            _expression: expression,
            _task: task,
        }
    }

    fn to_config(&self) -> VectorMarkerConfig {
        VectorMarkerConfig {
            // An expression's component is named by a content hash, so what
            // round-trips is the text that made it.
            component: crate::dynamic::expressions::binding_text(&self.db, self.component_id)
                .or_else(|| {
                    binding::component_name(&self.db, self.component_id).map(|n| n.to_string())
                })
                .unwrap_or_else(|| self.component.to_string()),
            label: self.label.to_string(),
            color: self.color,
        }
    }

    /// Restart the stream when the inspector has re-pointed the marker.
    /// Driven from the ball's render, since a marker is data rather than a
    /// view and never gets a render pass of its own.
    pub(crate) fn rebind(&mut self, cx: &mut Context<Self>) {
        if self.bound == Some(self.component_id) {
            return;
        }
        self.bound = Some(self.component_id);
        self._expression = crate::dynamic::expressions::running(self.component_id, cx);
        self.component = component_meta(&self.db, self.component_id).name;
        self.value = None;
        self._task = spawn_marker_stream(&self.db, self.component_id, cx);
    }

    /// An empty marker, as the inspector's "add" affordance creates it. It
    /// binds to nothing until a component is picked, which the next render
    /// then acts on.
    pub fn empty(db: Arc<DB>) -> Self {
        Self {
            component_id: ComponentId(0),
            label: SharedString::new_static(""),
            color: None,
            component: SharedString::new_static(""),
            bound: Some(ComponentId(0)),
            value: None,
            db,
            _expression: None,
            _task: gpui::Task::ready(()),
        }
    }
}

fn spawn_marker_stream(
    db: &Arc<DB>,
    component: ComponentId,
    cx: &mut Context<VectorMarker>,
) -> gpui::Task<()> {
    spawn_elements_stream(db.clone(), component, 3, cx, |marker, v, cx| {
        marker.value = Some([v[0] as f32, v[1] as f32, v[2] as f32]);
        cx.notify();
    })
}

/// Quaternion-bound attitude ball.
#[derive(facet::Facet)]
pub struct AttitudeIndicator {
    /// The quaternion this ball reads, and where in the frame it starts.
    /// Editable: picking another component rebinds on the next frame.
    pub component_id: ComponentId,
    pub element_offset: usize,
    pub label: SharedString,
    pub show_readout: bool,
    pub vectors: Vec<Entity<VectorMarker>>,
    /// Fallback name for a component nothing has registered.
    #[facet(skip)]
    component: SharedString,
    /// What `_task` is streaming, compared against the editable fields.
    #[facet(opaque)]
    bound: Option<ElementRef>,
    /// Latest attitude as `[x, y, z, w]`.
    #[facet(skip)]
    quat: Option<[f32; 4]>,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _expression: Option<crate::dynamic::expressions::Expression>,
    #[facet(opaque)]
    _task: gpui::Task<()>,
    #[facet(opaque)]
    _resolver_task: gpui::Task<()>,
}

impl AttitudeIndicator {
    pub fn from_config(cfg: &AttitudeConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let bound = crate::dynamic::expressions::bind(&cfg.component, &db, cx).ok();
        let component_id = bound
            .as_ref()
            .map(|bound| bound.id)
            .unwrap_or_else(|| ComponentId::new(&cfg.component));
        let expression = bound.and_then(|bound| bound.expression);
        let meta = component_meta(&db, component_id);
        let offset = cfg.element_offset;

        let task = spawn_quat_stream(&db, component_id, offset, cx);

        let keep_label = cfg.label.is_some();
        let resolver_task =
            spawn_meta_resolver(db.clone(), component_id, cx, move |view, meta, cx| {
                if !keep_label {
                    view.label = meta.name;
                }
                cx.notify();
            });

        let vectors = cfg
            .vectors
            .iter()
            .map(|v| {
                let v = v.clone();
                let db = db.clone();
                cx.new(|cx| VectorMarker::from_config(&v, db, cx))
            })
            .collect();

        Self {
            label: cfg
                .label
                .clone()
                .map(SharedString::from)
                .unwrap_or(meta.name),
            show_readout: !cfg.hide_readout,
            vectors,
            component: SharedString::from(cfg.component.clone()),
            component_id,
            element_offset: offset,
            bound: Some(ElementRef::new(component_id, offset)),
            quat: None,
            db,
            _expression: expression,
            _task: task,
            _resolver_task: resolver_task,
        }
    }

    /// Restart the quaternion stream when the inspector has re-pointed the
    /// ball, and let each marker do the same for itself.
    pub(crate) fn rebind(&mut self, cx: &mut Context<Self>) {
        for marker in self.vectors.clone() {
            marker.update(cx, |m, cx| m.rebind(cx));
        }

        let want = ElementRef::new(self.component_id, self.element_offset);
        if !binding::rebound(want, &mut self.bound) {
            return;
        }
        self._expression = crate::dynamic::expressions::running(want.component, cx);
        let offset = want.element;
        let meta = component_meta(&self.db, want.component);
        self.label = meta.name.clone();
        self.component = meta.name;
        self.quat = None;
        self._task = spawn_quat_stream(&self.db, want.component, offset, cx);
        self._resolver_task =
            spawn_meta_resolver(self.db.clone(), want.component, cx, |view, meta, cx| {
                view.label = meta.name;
                cx.notify();
            });
    }

    pub fn to_config(&self, cx: &gpui::App) -> AttitudeConfig {
        AttitudeConfig {
            component: crate::dynamic::expressions::binding_text(&self.db, self.component_id)
                .or_else(|| {
                    binding::component_name(&self.db, self.component_id)
                        .map(|name| name.to_string())
                })
                .unwrap_or_else(|| self.component.to_string()),
            element_offset: self.element_offset,
            label: Some(self.label.to_string()),
            vectors: self
                .vectors
                .iter()
                .map(|v| v.read(cx).to_config())
                .collect(),
            hide_readout: !self.show_readout,
        }
    }

    pub fn component(&self) -> &SharedString {
        &self.component
    }
}

fn spawn_quat_stream(
    db: &Arc<DB>,
    component: ComponentId,
    offset: usize,
    cx: &mut Context<AttitudeIndicator>,
) -> gpui::Task<()> {
    spawn_elements_stream(db.clone(), component, offset + 4, cx, move |view, v, cx| {
        view.quat = Some([
            v[offset] as f32,
            v[offset + 1] as f32,
            v[offset + 2] as f32,
            v[offset + 3] as f32,
        ]);
        cx.notify();
    })
}

/// Roll, pitch and yaw in radians from a `[x, y, z, w]` body←reference
/// quaternion, as the usual 3-2-1 (yaw-pitch-roll) sequence.
///
/// Pitch clamps before the `asin` so a slightly non-unit quaternion — which
/// is what arrives from an estimator between normalizations — yields ±90°
/// instead of `NaN`.
pub(crate) fn euler_zyx(q: [f32; 4]) -> (f32, f32, f32) {
    let [x, y, z, w] = q;
    let roll = (2.0 * (w * x + y * z)).atan2(1.0 - 2.0 * (x * x + y * y));
    let pitch = (2.0 * (w * y - z * x)).clamp(-1.0, 1.0).asin();
    let yaw = (2.0 * (w * z + x * y)).atan2(1.0 - 2.0 * (y * y + z * z));
    (roll, pitch, yaw)
}

/// Rotate a ball-local offset by `roll` and place it against `center`.
///
/// Screen coordinates are y-down, so this formula turns a positive roll —
/// right wing down — into the horizon tilting counterclockwise, which is what
/// a right bank looks like from the cockpit.
fn place(center: Point<Pixels>, offset: (f32, f32), roll: f32) -> Point<Pixels> {
    let (s, c) = roll.sin_cos();
    point(
        center.x + px(offset.0 * c - offset.1 * s),
        center.y + px(offset.0 * s + offset.1 * c),
    )
}

/// Half-chord length where the horizontal line `y = d` crosses a circle of
/// `radius`, or `None` when the line misses it entirely.
fn chord_half_width(radius: f32, d: f32) -> Option<f32> {
    (d.abs() < radius).then(|| (radius * radius - d * d).sqrt())
}

/// Ball-local outline of the disc region on one side of the horizon line
/// `y = d`, walking the chord and then the arc.
///
/// Returns an empty outline when that side is entirely off the ball, and the
/// full circle when it covers all of it — so a vehicle pitched past vertical
/// shows solid ground or solid sky rather than a glitch.
fn half_disc(radius: f32, d: f32, ground: bool) -> Vec<(f32, f32)> {
    // Circle sampled as (r cos t, r sin t); y grows downward, so the ground
    // (larger y) is where sin t > d / r.
    let (from, to) = match chord_half_width(radius, d) {
        Some(_) => {
            let alpha = (d / radius).asin();
            if ground {
                (alpha, std::f32::consts::PI - alpha)
            } else {
                (
                    std::f32::consts::PI - alpha,
                    2.0 * std::f32::consts::PI + alpha,
                )
            }
        }
        None => {
            let covers = if ground { d <= -radius } else { d >= radius };
            if !covers {
                return Vec::new();
            }
            (0.0, 2.0 * std::f32::consts::PI)
        }
    };

    (0..=ARC_SAMPLES)
        .map(|i| {
            let t = from + (to - from) * (i as f32 / ARC_SAMPLES as f32);
            (radius * t.cos(), radius * t.sin())
        })
        .collect()
}

/// Where a body-frame direction lands on the ball.
///
/// The viewer looks along body +X, so a marker's screen offset is its `(y, z)`
/// components. Directions behind the vehicle (`x < 0`) have no position on
/// the near face; they are reported clamped to the rim in their bearing so
/// the caller can draw them hollow rather than drop them.
fn project_marker(v: [f32; 3], radius: f32) -> Option<((f32, f32), bool)> {
    let norm = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if !norm.is_finite() || norm < f32::EPSILON {
        return None;
    }
    let (x, y, z) = (v[0] / norm, v[1] / norm, v[2] / norm);
    let ahead = x >= 0.0;
    let planar = (y * y + z * z).sqrt();
    if ahead {
        Some(((y * radius, z * radius), true))
    } else if planar < f32::EPSILON {
        // Exactly astern: no bearing to point at, so pin it to the rim
        // straight down rather than divide by zero.
        Some(((0.0, radius), false))
    } else {
        Some((((y / planar) * radius, (z / planar) * radius), false))
    }
}

fn stroke_polyline(points: &[Point<Pixels>], width: Pixels, color: Hsla, window: &mut Window) {
    if points.len() < 2 {
        return;
    }
    let mut path = PathBuilder::stroke(width);
    path.move_to(points[0]);
    for p in &points[1..] {
        path.line_to(*p);
    }
    if let Ok(p) = path.build() {
        window.paint_path(p, color);
    }
}

/// Stroke a closed outline around `points`, joining the last back to the
/// first — the border half of the pill treatment.
fn outline(points: &[Point<Pixels>], width: Pixels, color: Hsla, window: &mut Window) {
    if points.len() < 3 {
        return;
    }
    let mut closed: Vec<Point<Pixels>> = points.to_vec();
    closed.push(points[0]);
    stroke_polyline(&closed, width, color, window);
}

fn fill_polygon(points: &[Point<Pixels>], color: Hsla, window: &mut Window) {
    if points.len() < 3 {
        return;
    }
    let mut path = PathBuilder::fill();
    path.add_polygon(points, true);
    if let Ok(p) = path.build() {
        window.paint_path(p, color);
    }
}

/// A marker resolved for one frame.
struct Marker {
    value: [f32; 3],
    color: Hsla,
    label: SharedString,
}

impl Render for AttitudeIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.rebind(cx);
        let theme = theme(cx);
        let (roll, pitch, yaw) = self.quat.map(euler_zyx).unwrap_or((0.0, 0.0, 0.0));
        let has_fix = self.quat.is_some();

        // Full-strength hues: the paint pass dims them for the fills and
        // keeps them solid for the borders.
        let sky = theme.horizon_sky;
        let ground = theme.horizon_ground;
        let reticle = theme.text_primary;
        let rim = theme.border_primary;
        let backdrop = theme.bg_secondary;
        let palette = theme.line_colors;

        let markers: Vec<Marker> = self
            .vectors
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let m = e.read(cx);
                Some(Marker {
                    value: m.value?,
                    color: m.color.unwrap_or(palette[i % palette.len()]),
                    label: m.label.clone(),
                })
            })
            .collect();

        // Split before the canvas takes ownership: the legend needs the
        // labels, the paint pass only needs positions and colours.
        let legend: SmallVec<[(Hsla, SharedString); 4]> = markers
            .iter()
            .filter(|m| !m.label.is_empty())
            .map(|m| (m.color, m.label.clone()))
            .collect();
        let plot: Vec<([f32; 3], Hsla)> = markers.iter().map(|m| (m.value, m.color)).collect();

        let ball = canvas(
            move |bounds, _window, _cx| bounds,
            move |_, bounds: Bounds<Pixels>, window, _cx| {
                let radius =
                    (f32::from(bounds.size.width).min(f32::from(bounds.size.height)) / 2.0) - 1.0;
                if radius <= 0.0 {
                    return;
                }
                let center = point(
                    bounds.origin.x + bounds.size.width / 2.0,
                    bounds.origin.y + bounds.size.height / 2.0,
                );

                // Unfixed shows a neutral disc: a level ball would read as a
                // valid attitude when there is none.
                if !has_fix {
                    let disc: Vec<Point<Pixels>> = half_disc(radius, radius, false)
                        .into_iter()
                        .map(|o| place(center, o, 0.0))
                        .collect();
                    fill_polygon(&disc, backdrop, window);
                    outline(&disc, px(1.0), rim, window);
                    return;
                }

                // Each half is drawn the way the plot legend draws a pill:
                // the hue at low alpha inside, the same hue at full strength
                // around the edge. The border traces the arc *and* the
                // horizon chord, so the split is crisp without a separate
                // line over the top of it.
                let d = (pitch / PITCH_FULL_SCALE) * radius;
                for (side_ground, color) in [(false, sky), (true, ground)] {
                    let poly: Vec<Point<Pixels>> = half_disc(radius, d, side_ground)
                        .into_iter()
                        .map(|o| place(center, o, roll))
                        .collect();
                    fill_polygon(&poly, Theme::dim(color, FILL_ALPHA), window);
                    outline(&poly, px(1.5), color, window);
                }

                // Pitch ladder: rungs at fixed angles, each trimmed to the
                // ball so none pokes out past the rim. A rung takes the hue
                // of the half it sits in rather than a neutral grey, so the
                // scale reads as part of the sky or the ground instead of as
                // a separate layer laid over both.
                let step_px = (LADDER_STEP_DEG.to_radians() / PITCH_FULL_SCALE) * radius;
                let mut k = 1;
                loop {
                    let rung_offset = k as f32 * step_px;
                    if rung_offset > radius + step_px {
                        break;
                    }
                    for (sign, hue) in [(-1.0_f32, sky), (1.0, ground)] {
                        let y = d + sign * rung_offset;
                        let Some(h) = chord_half_width(radius, y) else {
                            continue;
                        };
                        let half = (radius * LADDER_HALF_WIDTH).min(h);
                        stroke_polyline(
                            &[
                                place(center, (-half, y), roll),
                                place(center, (half, y), roll),
                            ],
                            px(1.0),
                            Theme::dim(hue, LADDER_ALPHA),
                            window,
                        );
                    }
                    k += 1;
                }

                for (v, color) in plot.iter().copied() {
                    let Some((offset, ahead)) = project_marker(v, radius) else {
                        continue;
                    };
                    // Markers sit in the body frame, so they do not rotate
                    // with the ball.
                    let p = place(center, offset, 0.0);
                    let dot = Bounds::new(
                        point(p.x - px(MARKER_PX), p.y - px(MARKER_PX)),
                        gpui::size(px(MARKER_PX * 2.0), px(MARKER_PX * 2.0)),
                    );
                    if ahead {
                        let mut quad = gpui::fill(dot, color);
                        quad.corner_radii = gpui::Corners::all(px(MARKER_PX));
                        window.paint_quad(quad);
                    } else {
                        let mut quad = gpui::fill(dot, gpui::transparent_black());
                        quad.corner_radii = gpui::Corners::all(px(MARKER_PX));
                        quad.border_widths = gpui::Edges::all(px(1.5));
                        quad.border_color = color;
                        window.paint_quad(quad);
                    }
                }

                // Fixed vehicle reticle: wings and a centre dot, in screen
                // space, since it represents the vehicle rather than the
                // world.
                let wing = radius * 0.3;
                stroke_polyline(
                    &[
                        place(center, (-wing, 0.0), 0.0),
                        place(center, (-wing * 0.35, 0.0), 0.0),
                    ],
                    px(2.0),
                    reticle,
                    window,
                );
                stroke_polyline(
                    &[
                        place(center, (wing * 0.35, 0.0), 0.0),
                        place(center, (wing, 0.0), 0.0),
                    ],
                    px(2.0),
                    reticle,
                    window,
                );
            },
        );

        let mut tile = div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(3.0))
            .p(px(6.0));

        if !self.label.is_empty() {
            tile = tile.child(
                div()
                    .w_full()
                    .truncate()
                    .text_size(px(10.0))
                    .text_color(theme.text_secondary)
                    .child(self.label.clone()),
            );
        }

        tile = tile.child(div().flex_1().w_full().child(ball.size_full()));

        if self.show_readout {
            let text = if has_fix {
                format!(
                    "R {:>6.1}°  P {:>6.1}°  Y {:>6.1}°",
                    roll.to_degrees(),
                    pitch.to_degrees(),
                    yaw.to_degrees()
                )
            } else {
                "no attitude".to_string()
            };
            tile = tile.child(
                div()
                    .text_size(px(10.0))
                    .text_color(theme.text_primary)
                    .child(SharedString::from(text)),
            );
        }

        // Legend, so a coloured dot on the ball is identifiable.
        if !legend.is_empty() {
            let mut row = div()
                .flex()
                .flex_row()
                .flex_wrap()
                .justify_center()
                .gap(px(6.0))
                .text_size(px(9.0));
            for (color, label) in legend {
                row = row.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(3.0))
                        .child(div().w(px(6.0)).h(px(6.0)).rounded(px(3.0)).bg(color))
                        .child(div().text_color(theme.text_tertiary).child(label)),
                );
            }
            tile = tile.child(row);
        }

        tile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HALF_SQRT2: f32 = std::f32::consts::FRAC_1_SQRT_2;

    #[test]
    fn identity_is_level() {
        let (r, p, y) = euler_zyx([0.0, 0.0, 0.0, 1.0]);
        assert!(r.abs() < 1e-6 && p.abs() < 1e-6 && y.abs() < 1e-6);
    }

    #[test]
    fn quarter_turns_recover_their_axis() {
        let (r, p, y) = euler_zyx([HALF_SQRT2, 0.0, 0.0, HALF_SQRT2]);
        assert!((r.to_degrees() - 90.0).abs() < 1e-3, "roll {r}");
        assert!(p.abs() < 1e-4 && y.abs() < 1e-4);

        // Straight-up pitch is gimbal lock: `asin` has an infinite slope at
        // ±1, so f32 rounding in `2wy` costs a couple of hundredths of a
        // degree here. That is the arithmetic, not the formula.
        let (_, p, _) = euler_zyx([0.0, HALF_SQRT2, 0.0, HALF_SQRT2]);
        assert!((p.to_degrees() - 90.0).abs() < 0.05, "pitch {p}");

        let (r, p, y) = euler_zyx([0.0, 0.0, HALF_SQRT2, HALF_SQRT2]);
        assert!((y.to_degrees() - 90.0).abs() < 1e-3, "yaw {y}");
        assert!(r.abs() < 1e-4 && p.abs() < 1e-4);
    }

    #[test]
    fn pitch_saturates_instead_of_going_nan() {
        // A drifted, non-unit estimate would push asin past 1.
        let (_, p, _) = euler_zyx([0.0, 0.9, 0.0, 0.9]);
        assert!(p.is_finite());
        assert!((p.to_degrees() - 90.0).abs() < 1e-3);
    }

    #[test]
    fn a_right_bank_puts_the_ground_on_the_left() {
        let center = point(px(0.0), px(0.0));
        // Straight down on a level ball is the ground side.
        let down = place(center, (0.0, 10.0), 0.0);
        assert!(f32::from(down.y) > 0.0 && f32::from(down.x).abs() < 1e-4);
        // Rolled 90° right, that same point swings to screen left.
        let rolled = place(center, (0.0, 10.0), std::f32::consts::FRAC_PI_2);
        assert!(f32::from(rolled.x) < -9.9, "{rolled:?}");
        assert!(f32::from(rolled.y).abs() < 1e-3);
    }

    #[test]
    fn nose_up_drops_the_horizon_below_centre() {
        // d is the horizon's ball-local y; positive is downward on screen.
        let pitch = 30.0_f32.to_radians();
        let d = (pitch / PITCH_FULL_SCALE) * 100.0;
        assert!(d > 0.0);
        assert!(chord_half_width(100.0, d).is_some());
    }

    #[test]
    fn the_horizon_chord_shrinks_as_it_nears_the_rim() {
        let wide = chord_half_width(100.0, 0.0).unwrap();
        let narrow = chord_half_width(100.0, 90.0).unwrap();
        assert!((wide - 100.0).abs() < 1e-4);
        assert!(narrow < wide && narrow > 0.0);
        assert_eq!(chord_half_width(100.0, 100.0), None);
        assert_eq!(chord_half_width(100.0, -140.0), None);
    }

    #[test]
    fn a_level_ball_splits_evenly() {
        let sky = half_disc(100.0, 0.0, false);
        let ground = half_disc(100.0, 0.0, true);
        assert!(!sky.is_empty() && !ground.is_empty());
        // Sky sits above centre, ground below.
        assert!(sky.iter().map(|(_, y)| *y).sum::<f32>() < 0.0);
        assert!(ground.iter().map(|(_, y)| *y).sum::<f32>() > 0.0);
    }

    #[test]
    fn pitching_past_vertical_fills_the_ball() {
        // Horizon far below the ball: all sky, no ground.
        assert!(half_disc(100.0, 500.0, false).len() > 3);
        assert!(half_disc(100.0, 500.0, true).is_empty());
        // And the reverse.
        assert!(half_disc(100.0, -500.0, true).len() > 3);
        assert!(half_disc(100.0, -500.0, false).is_empty());
    }

    #[test]
    fn every_ball_point_stays_within_the_radius() {
        for d in [-150.0_f32, -50.0, 0.0, 50.0, 150.0] {
            for ground in [false, true] {
                for (x, y) in half_disc(100.0, d, ground) {
                    assert!(
                        (x * x + y * y).sqrt() <= 100.01,
                        "d {d} ground {ground} point {x},{y}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_marker_dead_ahead_sits_at_the_centre() {
        let (offset, ahead) = project_marker([1.0, 0.0, 0.0], 100.0).unwrap();
        assert!(ahead);
        assert!(offset.0.abs() < 1e-4 && offset.1.abs() < 1e-4);
    }

    #[test]
    fn a_marker_abeam_sits_on_the_rim() {
        let (offset, ahead) = project_marker([0.0, 1.0, 0.0], 100.0).unwrap();
        assert!(ahead);
        assert!((offset.0 - 100.0).abs() < 1e-3 && offset.1.abs() < 1e-4);
    }

    #[test]
    fn a_marker_behind_is_pinned_to_the_rim_in_its_bearing() {
        // Astern and to the right: still plotted right, but flagged behind.
        let (offset, ahead) = project_marker([-1.0, 1.0, 0.0], 100.0).unwrap();
        assert!(!ahead);
        assert!((offset.0 - 100.0).abs() < 1e-3, "{offset:?}");
        assert!(offset.1.abs() < 1e-4);
    }

    #[test]
    fn markers_are_normalized_before_plotting() {
        let (a, _) = project_marker([0.0, 5.0, 0.0], 100.0).unwrap();
        let (b, _) = project_marker([0.0, 0.02, 0.0], 100.0).unwrap();
        assert!((a.0 - b.0).abs() < 1e-3);
    }

    #[test]
    fn a_degenerate_marker_is_dropped() {
        assert!(project_marker([0.0, 0.0, 0.0], 100.0).is_none());
        assert!(project_marker([f32::NAN, 0.0, 0.0], 100.0).is_none());
    }

    #[test]
    fn exactly_astern_still_yields_a_position() {
        let (offset, ahead) = project_marker([-1.0, 0.0, 0.0], 100.0).unwrap();
        assert!(!ahead);
        assert!(offset.0.is_finite() && offset.1.is_finite());
    }
}
