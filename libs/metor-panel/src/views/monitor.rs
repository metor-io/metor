use std::sync::Arc;

use gpui::{Context, Entity, IntoElement, SharedString, Window, div, prelude::*, px};
use metor_db::DB;
use metor_proto::types::ComponentId;
use smallvec::SmallVec;

use super::time_series::{LinePlot, Trace};
use super::value_strip::{ComponentValueStrip, StripBehavior, StripClick, StripStyle};
use crate::ComponentStreamBuilder;
use crate::inspector::edits::{EditRequest, pending_edits, pending_edits_mut};
use crate::theme::theme;

/// Dashboard widget that displays a component's current value alongside its
/// name and an optional sparkline. The value row itself is rendered by a
/// shared [`ComponentValueStrip`] that handles streaming, formatting, and
/// click-to-edit behaviour.
#[derive(facet::Facet)]
pub struct Monitor {
    #[facet(skip)]
    name: SharedString,
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

        // Seed traces when the component is already registered so the
        // sparkline is not momentarily empty.
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
            highlighted: SmallVec::new(),
            locked: pending_edits(cx).locked,
        };

        let strip = cx.new(|cx| {
            ComponentValueStrip::new(db.clone(), source, style, behavior, cx)
        });

        // Rebind traces once the component is registered — until then the
        // eager rebind above may have fallen through and the plot is empty.
        let resolver_task = cx.spawn({
            let db = db.clone();
            let sparkline = sparkline.clone();
            async move |_this, cx| {
                loop {
                    let comp =
                        db.with_state(|state| state.get_component(component_id).cloned());
                    if let Some(comp) = comp {
                        let element_count: usize =
                            comp.schema.dim.iter().product::<usize>().max(1);
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

/// Build the click adapter that forwards strip clicks to the pending-edit
/// palette. Element names are re-resolved at click time to reflect the
/// component's latest metadata. The click position is carried through as the
/// anchor so the resulting inspector opens next to the cell.
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

/// Snapshot the current pending-edits state relevant to `component_id` into
/// a [`StripBehavior`]. Shared by all three consumers of the strip.
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
        .unwrap_or_else(SmallVec::new);
    StripBehavior {
        on_element_click: Some(on_element_click),
        on_apply_element: Some(apply_click(db, component_id)),
        highlighted,
        locked: pending.locked,
    }
}

/// Build the chevron-click adapter: applies the pending edit for this
/// component (whole-value send) and drops `element_index` from the pending
/// UI list.
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
            .bg(theme.bg_primary)
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

        root = root.child(
            div()
                .px(px(6.0))
                .pt(px(3.0))
                .child(self.strip.clone()),
        );

        root = root.child(
            div()
                .px(px(6.0))
                .pb(px(4.0))
                .text_size(px(11.0))
                .text_color(theme.text_primary)
                .child(self.name.clone()),
        );

        root
    }
}
