use std::sync::Arc;

use gpui::{Context, Entity, IntoElement, SharedString, Window, div, prelude::*};
use metor_db::DB;
use metor_proto::types::ComponentId;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::time_series::{LinePlot, Trace};
use super::value_strip::{ComponentValueStrip, StripAlign, StripBehavior, StripClick, StripStyle};
use crate::data_binding::Binding;
use crate::inspect;
use crate::inspector::edits::{EditRequest, pending_edits, pending_edits_mut};
use crate::inspector::plot_preview::shift_hover_listener;
use crate::theme::{theme, ui_px};

/// A component's current value, name, and an optional sparkline — the one
/// value readout, on panes and dashboards alike.
///
/// The widget delegates streaming, formatting, and click-to-edit to a shared
/// [`ComponentValueStrip`]. Sparkline traces are bound lazily: producers may
/// register the component after the monitor is placed, so an async resolver
/// rebinds when the schema finally shows up in the DB.
#[derive(facet::Facet)]
pub struct Monitor {
    #[facet(skip)]
    name: SharedString,
    pub unit: SharedString,
    pub show_sparkline: bool,
    /// Where the value and name sit across the tile.
    pub align: StripAlign,
    /// Value text size; zero keeps the strip's own sizing, which shrinks
    /// multi-element values to fit side by side.
    #[facet(inspect::label = "Value size (0 = auto)")]
    #[facet(inspect::range(min = "0", max = "96"))]
    pub value_size: f32,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    _binding_changes: gpui::Task<()>,
    pub source: Binding,
    #[facet(skip)]
    bound: ComponentId,
    #[facet(opaque)]
    sparkline: Entity<LinePlot>,
    #[facet(opaque)]
    strip: Entity<ComponentValueStrip>,
    #[facet(opaque)]
    click: StripClick,
    #[facet(opaque)]
    _resolver_task: gpui::Task<()>,
}

/// Persisted shape of a monitor — the source component to monitor plus the
/// inspector-editable display fields.
///
/// `unit` and `show_sparkline` are `Option`: `None` means "never edited", so
/// restore keeps the metadata-derived defaults instead of clobbering them.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MonitorConfig {
    pub component: String,
    pub unit: Option<String>,
    pub show_sparkline: Option<bool>,
    pub align: StripAlign,
    pub value_size: f32,
}

impl Monitor {
    pub fn from_config(cfg: MonitorConfig, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let binding = Binding::from_text(&cfg.component, &db, cx);
        let mut monitor = Self::bind(db, binding, cx);
        if let Some(unit) = cfg.unit {
            monitor.unit = SharedString::from(unit);
        }
        if let Some(show) = cfg.show_sparkline {
            monitor.show_sparkline = show;
        }
        monitor.align = cfg.align;
        monitor.value_size = cfg.value_size.max(0.0);
        monitor
    }

    pub fn to_config(&self) -> MonitorConfig {
        MonitorConfig {
            component: self.source.text(&self.db),
            unit: Some(self.unit.to_string()),
            show_sparkline: Some(self.show_sparkline),
            align: self.align,
            value_size: self.value_size,
        }
    }

    /// Stream whatever `binding` currently resolves to. A rebind replaces the
    /// whole view, which is what drops the previous strip and resolver.
    fn bind(db: Arc<DB>, binding: Binding, cx: &mut Context<Self>) -> Self {
        let component_id = binding.id();
        let line_colors = theme(cx).line_colors;

        let (name, unit) = db.with_state(|state| {
            let meta = state.get_component_metadata(component_id);
            let name = meta
                .map(|m| m.name.clone())
                .unwrap_or_else(|| format!("{:?}", component_id));
            let unit = meta
                .and_then(|m| m.metadata.get("unit").cloned())
                .unwrap_or_default();
            (name, unit)
        });

        let sparkline = cx.new(|cx| LinePlot::new(db.clone(), cx));

        // Eager bind avoids a one-frame empty sparkline when the component
        // is already registered. The async resolver below handles the case
        // where it isn't.
        if let Some(comp) = db.with_state(|state| state.get_component(component_id).cloned()) {
            let element_count: usize = comp.schema.dim.iter().product::<usize>().max(1);
            let traces = build_traces(component_id, element_count, &line_colors);
            sparkline.update(cx, |sp, cx| sp.bind_traces(traces, cx));
        }

        let name_shared = SharedString::from(name);
        let unit_shared = SharedString::from(unit);

        let click = edit_click(db.clone(), component_id, name_shared.clone());

        let style = StripStyle::dashboard().with_unit(unit_shared.clone());
        let behavior = StripBehavior {
            on_element_click: Some(click.clone()),
            on_apply_element: Some(apply_click(db.clone(), component_id)),
            on_element_right_click: None,
            highlighted: SmallVec::new(),
            locked: pending_edits(cx).locked,
        };

        let strip =
            cx.new(|cx| ComponentValueStrip::new(db.clone(), component_id, style, behavior, cx));

        // Await component registration, then rebind traces to match its
        // actual element count.
        let resolver_task = cx.spawn({
            let db = db.clone();
            let sparkline = sparkline.clone();
            async move |_this, cx| {
                let comp = crate::wait_for_component(&db, component_id).await;
                let element_count: usize = comp.schema.dim.iter().product::<usize>().max(1);
                sparkline.update(cx, |sp, cx| {
                    if element_count != sp.trace_count() {
                        let traces = build_traces(component_id, element_count, &line_colors);
                        sp.bind_traces(traces, cx);
                    }
                });
            }
        });

        Self {
            name: name_shared,
            unit: unit_shared,
            show_sparkline: true,
            align: StripAlign::Left,
            value_size: 0.0,
            _binding_changes: crate::data_binding::watch_registrations(db.clone(), cx),
            db,
            source: binding,
            bound: component_id,
            sparkline,
            strip,
            click,
            _resolver_task: resolver_task,
        }
    }
}

fn build_traces(
    component_id: ComponentId,
    element_count: usize,
    line_colors: &[gpui::Hsla],
) -> Vec<Trace> {
    (0..element_count)
        .map(|i| {
            let mut t = Trace::new(component_id, i, line_colors[i % line_colors.len()]);
            t.stroke_width = 1.5;
            t
        })
        .collect()
}

/// Click adapter that routes a strip click into the pending-edit palette.
///
/// Element names are looked up at click time so a metadata refresh doesn't
/// leave the palette showing stale labels. The cursor position travels with
/// the request so the inspector anchors next to the clicked cell.
pub(crate) fn edit_click(
    db: Arc<DB>,
    component_id: ComponentId,
    component_name: SharedString,
) -> StripClick {
    Arc::new(move |element_index, position, _window, cx| {
        let element_names: Vec<SharedString> =
            crate::inspector::trace_picker::element_names_for_component(&db, component_id)
                .into_iter()
                .map(SharedString::from)
                .collect();
        pending_edits_mut(cx).pending_request = Some(EditRequest {
            component_id,
            component_name: component_name.clone(),
            element_names,
            element_index,
            anchor: Some(position),
        });
    })
}

/// Build a fresh [`StripBehavior`] each frame from the global pending-edit state.
///
/// Centralises the snapshot so Monitor, StateChip, and ComponentOutline
/// share the same edit semantics (highlighted indices and lock state).
pub(crate) fn behavior_snapshot(
    cx: &gpui::App,
    db: Arc<DB>,
    component_id: ComponentId,
    on_element_click: StripClick,
) -> StripBehavior {
    let pending = pending_edits(cx);
    let highlighted = pending
        .get(component_id)
        .map(|edit| edit.modified_elements.iter().copied().collect())
        .unwrap_or_default();
    StripBehavior {
        on_element_click: Some(on_element_click),
        on_apply_element: Some(apply_click(db, component_id)),
        on_element_right_click: None,
        highlighted,
        locked: pending.locked,
    }
}

/// Chevron-click adapter: sends the component's full pending value and
/// clears `element_index` from the UI's modified list.
pub(crate) fn apply_click(db: Arc<DB>, component_id: ComponentId) -> StripClick {
    Arc::new(move |element_index, _position, _window, cx| {
        crate::inspector::edits::apply_element(&db, component_id, element_index, cx);
    })
}

impl Render for Monitor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.source.resolve(&self.db, cx);
        if self.bound != self.source.id() {
            let unit = self.unit.clone();
            let (show, align, value_size) = (self.show_sparkline, self.align, self.value_size);
            *self = Self::bind(self.db.clone(), self.source.clone(), cx);
            self.unit = unit;
            self.show_sparkline = show;
            self.align = align;
            self.value_size = value_size;
        }
        let theme = theme(cx);

        let style = StripStyle::dashboard()
            .with_unit(self.unit.clone())
            .with_align(self.align)
            .with_value_size((self.value_size > 0.0).then_some(self.value_size));
        let behavior = behavior_snapshot(cx, self.db.clone(), self.source.id(), self.click.clone());
        self.strip.update(cx, |strip, cx| {
            strip.set_style(style, cx);
            strip.set_behavior(behavior, cx);
        });

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .justify_end()
            .overflow_hidden();

        if self.show_sparkline && !self.sparkline.read(cx).is_empty() {
            root = root.child(
                div()
                    .w_full()
                    .pt(ui_px(4.0))
                    .flex_1()
                    .min_h(ui_px(16.0))
                    .child(self.sparkline.clone()),
            );
        }

        root = root.child(
            div()
                .px(ui_px(6.0))
                .pt(ui_px(3.0))
                .child(self.strip.clone()),
        );

        root = root.child(
            div()
                .id("monitor-name")
                .px(ui_px(6.0))
                .pb(ui_px(4.0))
                .map(|name| match self.align {
                    StripAlign::Left => name,
                    StripAlign::Center => name.text_center(),
                    StripAlign::Right => name.text_right(),
                })
                .text_size(ui_px(11.0))
                .text_color(theme.text_primary)
                .on_mouse_move(shift_hover_listener(self.source.id(), SmallVec::new()))
                .child(self.name.clone()),
        );

        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Also the shape of a saved Component Text pane or widget, which now
    /// reopens as a monitor.
    #[test]
    fn monitor_config_defaults_optional_display_fields() {
        let cfg: MonitorConfig = serde_json::from_str(r#"{"component":"a.b"}"#).unwrap();
        assert_eq!(cfg.component, "a.b");
        assert_eq!(cfg.unit, None);
        assert_eq!(cfg.show_sparkline, None);
        assert_eq!(cfg.align, StripAlign::Left);
        assert_eq!(cfg.value_size, 0.0);
    }

    #[test]
    fn monitor_config_round_trips_display_fields() {
        let cfg = MonitorConfig {
            component: "a.b".to_string(),
            unit: Some("V".to_string()),
            show_sparkline: Some(false),
            align: StripAlign::Right,
            value_size: 32.0,
        };
        let blob = serde_json::to_string(&cfg).unwrap();
        let back: MonitorConfig = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.component, "a.b");
        assert_eq!(back.unit.as_deref(), Some("V"));
        assert_eq!(back.show_sparkline, Some(false));
        assert_eq!(back.align, StripAlign::Right);
        assert_eq!(back.value_size, 32.0);
    }
}
