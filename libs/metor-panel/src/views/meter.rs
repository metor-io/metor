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

use std::sync::Arc;

use gpui::{
    App, Bounds, Context, Hsla, IntoElement, Pixels, SharedString, Window, canvas, div, fill,
    point, prelude::*, px, size,
};
use metor_db::DB;
use serde::{Deserialize, Serialize};

use super::binding::{self, ElementRef, component_meta, spawn_meta_resolver, spawn_scalar_stream};
use super::format_number;
use crate::theme::theme;

/// Which way the bar grows.
#[derive(Clone, Copy, Debug, Default, PartialEq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum Orientation {
    #[default]
    Vertical,
    Horizontal,
}

/// Thickness of a limit tick drawn across the track.
const TICK_PX: f32 = 1.5;
/// Corner rounding shared by the track and its fill.
const TRACK_RADIUS: f32 = 3.0;

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
    pub color: Hsla,
    pub show_value: bool,
    pub show_limits: bool,
    /// The name the meter was configured with, kept as the fallback for a
    /// component that has never registered — [`to_config`](Self::to_config)
    /// prefers the live name for whatever id is currently bound.
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
    _binding_changes: gpui::Task<()>,
    #[facet(opaque)]
    _task: gpui::Task<()>,
    #[facet(opaque)]
    _resolver_task: gpui::Task<()>,
}

impl Meter {
    pub fn from_config(cfg: &MeterConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let source = crate::data_binding::Binding::from_text(&cfg.component, &db, cx);
        let component_id = source.id();
        let element = cfg.element;
        let at = ElementRef::new(component_id, element);
        let meta = component_meta(&db, component_id);

        let task =
            spawn_scalar_stream(db.clone(), component_id, element, cx, |meter, value, cx| {
                meter.value = value;
                cx.notify();
            });

        // Only metadata-derived fields refresh on late registration; an
        // explicitly configured label or unit is the operator's, not the
        // component's.
        let keep_label = cfg.label.is_some();
        let keep_unit = cfg.unit.is_some();
        let resolver_task =
            spawn_meta_resolver(db.clone(), component_id, cx, move |meter, meta, cx| {
                if !keep_label {
                    meter.label = default_label(&meta.element_names, element, &meta.name);
                }
                if !keep_unit {
                    meter.unit = meta.unit;
                }
                cx.notify();
            });

        Self {
            label: cfg
                .label
                .clone()
                .map(SharedString::from)
                .unwrap_or_else(|| default_label(&meta.element_names, element, &meta.name)),
            min: cfg.min,
            max: cfg.max,
            unit: cfg
                .unit
                .clone()
                .map(SharedString::from)
                .unwrap_or(meta.unit),
            orientation: cfg.orientation,
            color: cfg.color.unwrap_or_else(|| theme(cx).control_active),
            show_value: !cfg.hide_value,
            show_limits: !cfg.hide_limits,
            component: SharedString::from(cfg.component.clone()),
            source,
            element,
            bound: Some(at),
            value: None,
            _binding_changes: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
            _task: task,
            _resolver_task: resolver_task,
        }
    }

    /// Where this meter currently reads from.
    fn at(&self) -> ElementRef {
        ElementRef::new(self.source.id(), self.element)
    }

    /// Restart the stream when the inspector has re-pointed the meter.
    ///
    /// Label and unit are re-derived from the new component: they described
    /// the old one, and carrying them across would leave a bar labelled
    /// `gyro_b.x` reading wheel momentum. A value from the previous binding
    /// is dropped rather than left on screen under the new scale.
    pub(crate) fn rebind(&mut self, cx: &mut Context<Self>) {
        self.source.resolve(&self.db, cx);
        let want = self.at();
        if !binding::rebound(want, &mut self.bound) {
            return;
        }

        let element = want.element;
        let meta = component_meta(&self.db, want.component);
        self.label = default_label(&meta.element_names, element, &meta.name);
        self.unit = meta.unit;
        self.component = meta.name;
        self.value = None;
        self._task = spawn_scalar_stream(
            self.db.clone(),
            want.component,
            element,
            cx,
            |meter, value, cx| {
                meter.value = value;
                cx.notify();
            },
        );
        self._resolver_task = spawn_meta_resolver(
            self.db.clone(),
            want.component,
            cx,
            move |meter, meta, cx| {
                meter.label = default_label(&meta.element_names, element, &meta.name);
                meter.unit = meta.unit;
                cx.notify();
            },
        );
    }

    pub fn to_config(&self) -> MeterConfig {
        MeterConfig {
            // Resolve the name for whatever is bound *now*, so a rebind is
            // what gets saved; the configured name is only the fallback for a
            // component nothing has registered.
            // An expression's component is named by a content hash and
            // labelled with the text that made it, so what round-trips is the
            // text — a name would rehydrate onto nothing.
            component: self.source.text(&self.db),
            element: self.element,
            label: Some(self.label.to_string()),
            min: self.min,
            max: self.max,
            unit: Some(self.unit.to_string()),
            orientation: self.orientation,
            color: Some(self.color),
            hide_value: !self.show_value,
            hide_limits: !self.show_limits,
        }
    }

    pub fn component(&self) -> &SharedString {
        &self.component
    }
}

/// The element's own name when the component has more than one, else the
/// component's — a single-element meter labelled `"[0]"` tells the operator
/// nothing.
pub(crate) fn default_label(
    element_names: &[SharedString],
    element: usize,
    component_name: &SharedString,
) -> SharedString {
    if element_names.len() > 1 {
        element_names
            .get(element)
            .cloned()
            .unwrap_or_else(|| component_name.clone())
    } else {
        component_name.clone()
    }
}

/// The `(start, end)` of the fill along the track, each in `0..=1` measured
/// from the scale's low end.
///
/// A scale spanning zero is bipolar: the fill runs between the zero position
/// and the value, in whichever direction the value lies. Any other scale
/// fills from the low end.
pub(crate) fn fill_span(value: f64, min: f64, max: f64) -> (f32, f32) {
    let span = max - min;
    if !span.is_finite() || span.abs() < f64::EPSILON {
        return (0.0, 0.0);
    }
    let norm = |v: f64| (((v - min) / span).clamp(0.0, 1.0)) as f32;
    let v = norm(value);
    if min < 0.0 && max > 0.0 {
        let zero = norm(0.0);
        (zero.min(v), zero.max(v))
    } else {
        (0.0, v)
    }
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
        let span = self.value.map(|v| fill_span(v, min, max));

        let bar = canvas(
            move |bounds, _window, _cx| bounds,
            move |_, bounds, window, _cx| {
                if let Some(tint) = tint {
                    window.paint_quad(fill(bounds, tint));
                }
                paint_rounded(bounds, track_color, window);
                if let Some((start, end)) = span
                    && end > start
                {
                    paint_rounded(slice(bounds, start, end, vertical), color, window);
                }
                for (t, mark_color) in &marks {
                    window.paint_quad(fill(tick(bounds, *t, vertical), *mark_color));
                }
            },
        );

        let bar = if vertical {
            div().flex_1().w_full().child(bar.size_full())
        } else {
            div().w_full().h(px(10.0)).child(bar.size_full())
        };

        let mut tile = div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .p(px(6.0))
            .text_size(px(10.0));

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
            let value = match self.value {
                Some(v) => format_number(v),
                None => "—".to_string(),
            };
            let reading =
                div()
                    .text_color(theme.text_primary)
                    .truncate()
                    .child(SharedString::from(if vertical || self.unit.is_empty() {
                        value
                    } else {
                        format!("{value} {}", self.unit)
                    }));
            tile = tile.child(reading);

            // A vertical meter is only as wide as its bar, so a unit sharing
            // the value's line truncates the number itself — the one thing
            // that has to stay readable. Give it its own line there; a
            // horizontal bar has room to keep them together.
            if vertical && !self.unit.is_empty() {
                tile = tile.child(
                    div()
                        .text_color(theme.text_tertiary)
                        .truncate()
                        .child(self.unit.clone()),
                );
            }
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

fn paint_rounded(bounds: Bounds<Pixels>, color: Hsla, window: &mut Window) {
    let mut quad = fill(bounds, color);
    quad.corner_radii = gpui::Corners::all(px(TRACK_RADIUS));
    window.paint_quad(quad);
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn limits_outside_the_scale_are_dropped() {
        assert_eq!(limit_position(0.5, 0.0, 1.0), Some(0.5));
        assert_eq!(limit_position(2.0, 0.0, 1.0), None);
        assert_eq!(limit_position(-2.0, 0.0, 1.0), None);
    }

    #[test]
    fn label_prefers_the_element_name_only_when_there_are_several() {
        let comp = SharedString::from("plant.wheels");
        let many: Vec<SharedString> = vec!["x".into(), "y".into(), "z".into()];
        assert_eq!(default_label(&many, 1, &comp), SharedString::from("y"));
        let one: Vec<SharedString> = vec!["[0]".into()];
        assert_eq!(default_label(&one, 0, &comp), comp);
        assert_eq!(default_label(&many, 9, &comp), comp);
    }
}
