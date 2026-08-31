use std::sync::Arc;

use gpui::{Context, Entity, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

use super::time_series::{LinePlot, Trace};
use super::value_strip::{ComponentValueStrip, StripBehavior, StripClick, StripStyle};
use crate::ComponentStreamBuilder;
use crate::inspector::edits::{EditRequest, pending_edits, pending_edits_mut};
use crate::inspector::plot_preview::shift_hover_listener;
use crate::theme::theme;

/// Dashboard tile showing a component's current value, name, and an optional
/// sparkline.
///
/// The widget delegates streaming, formatting, and click-to-edit to a shared
/// [`ComponentValueStrip`]. Sparkline traces are bound lazily: producers may
/// register the component after the monitor is placed, so an async resolver
/// rebinds when the schema finally shows up in the DB.
#[derive(facet::Facet)]
pub struct Monitor {
    #[facet(skip)]
    name: SharedString,
    /// Held for its drop when this monitor is bound to a `=` expression: the
    /// expression runs while a view wants it, and this is that view.
    #[facet(opaque)]
    _expression: Option<crate::dynamic::expressions::Expression>,
    pub unit: SharedString,
    pub show_sparkline: bool,
    #[facet(opaque)]
    db: Arc<DB>,
    #[facet(opaque)]
    component_id: ComponentId,
    #[facet(opaque)]
    sparkline: Entity<LinePlot>,
    #[facet(opaque)]
    strip: Entity<ComponentValueStrip>,
    #[facet(opaque)]
    click: StripClick,
    #[facet(opaque)]
    _resolver_task: gpui::Task<()>,
}

impl Monitor {
    /// Bind this monitor to a running `=` expression, taking a share of its
    /// lifetime and showing the text the operator typed as its name.
    pub fn bind_expression(
        &mut self,
        expression: crate::dynamic::expressions::Expression,
        label: impl Into<SharedString>,
    ) {
        self.name = label.into();
        self._expression = Some(expression);
    }

    pub fn new(
        db: Arc<DB>,
        source: impl ComponentStreamBuilder + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Self {
        let component_id = source.component_id();
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

        let strip = cx.new(|cx| ComponentValueStrip::new(db.clone(), source, style, behavior, cx));

        // Await component registration, then rebind traces to match its
        // actual element count.
        let resolver_task = cx.spawn({
            let db = db.clone();
            let sparkline = sparkline.clone();
            async move |_this, cx| {
                loop {
                    let comp = db.with_state(|state| state.get_component(component_id).cloned());
                    if let Some(comp) = comp {
                        let element_count: usize = comp.schema.dim.iter().product::<usize>().max(1);
                        let _ = sparkline.update(cx, |sp, cx| {
                            if element_count != sp.trace_count() {
                                let traces =
                                    build_traces(component_id, element_count, &line_colors);
                                sp.bind_traces(traces, cx);
                            }
                        });
                        break;
                    }
                    db.vtable_gen.wait().await;
                }
            }
        });

        Self {
            name: name_shared,
            unit: unit_shared,
            show_sparkline: true,
            db,
            component_id,
            _expression: None,
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
/// Centralises the snapshot so Monitor, ComponentText, and ComponentOutline
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
        let theme = theme(cx);

        let style = StripStyle::dashboard().with_unit(self.unit.clone());
        let behavior =
            behavior_snapshot(cx, self.db.clone(), self.component_id, self.click.clone());
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
                    .pt(px(4.0))
                    .flex_1()
                    .min_h(px(16.0))
                    .child(self.sparkline.clone()),
            );
        }

        root = root.child(div().px(px(6.0)).pt(px(3.0)).child(self.strip.clone()));

        root = root.child(
            div()
                .id("monitor-name")
                .px(px(6.0))
                .pb(px(4.0))
                .text_size(px(11.0))
                .text_color(theme.text_primary)
                .on_mouse_move(shift_hover_listener(self.component_id, SmallVec::new()))
                .child(self.name.clone()),
        );

        root
    }
}
