//! Mechanics shared by the GPU-backed plot entities (`time_series::LinePlot`,
//! `xy_plot::XyLinePlot`, `list_plot::ListLinePlot`).
//!
//! The three keep their own struct shapes — the inspector reflects a
//! different set of fields for each (multi-axis vs. flat scalar overrides) —
//! but the per-trace tracker bookkeeping is byte-identical, so it lives here.

use std::collections::{HashMap, HashSet};

use gpui::{
    Entity, EntityId, Hsla, IntoElement, MouseButton, Pixels, Point, SharedString, Styled, Task,
    TextRun, Window, div, prelude::*, px,
};

use crate::views::time_series::LABEL_FONT_SIZE;

/// Build the common interactive plot legend.
///
/// `notify` is the line-plot entity whose renderer and bounds cache need to
/// be refreshed after a trace visibility change.
pub fn plot_legend<T: 'static, N: 'static>(
    traces: &[Entity<T>],
    notify: Entity<N>,
    left: Pixels,
    describe: fn(&T) -> (SharedString, Hsla, bool),
    toggle_visible: fn(&mut T),
    cx: &gpui::App,
) -> impl IntoElement {
    let theme = crate::theme::theme(cx);
    let mut row = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap_1()
        .gap_y_0()
        .pl(left)
        .pb_1()
        .bg(theme.plot_chrome_bg());

    for trace_entity in traces {
        let trace = trace_entity.read(cx);
        let (label, trace_color, visible) = describe(trace);
        let opacity = if visible { 1.0 } else { 0.3 };
        let color = Hsla {
            a: opacity,
            ..trace_color
        };
        let text_color = Hsla {
            a: opacity,
            ..theme.text_secondary
        };
        let toggle_target = trace_entity.clone();
        let inspect_target = trace_entity.clone();
        let notify = notify.clone();

        row = row.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
                    toggle_target.update(cx, |trace, cx| {
                        toggle_visible(trace);
                        cx.notify();
                    });
                    notify.update(cx, |_, cx| cx.notify());
                })
                .on_mouse_down(MouseButton::Right, move |event, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::inspector::InspectEntity {
                            entity: inspect_target.clone().into_any(),
                            position: event.position,
                        }),
                        cx,
                    );
                })
                .child(div().w(px(10.0)).h(px(10.0)).rounded(px(2.0)).bg(color))
                .child(
                    div()
                        .text_size(px(LABEL_FONT_SIZE))
                        .text_color(text_color)
                        .child(label),
                ),
        );
    }

    row
}

/// Shape and paint a numeric axis label, allowing callers to position it
/// from the shaped width without duplicating GPUI text plumbing.
pub fn paint_value_label(
    value: f64,
    color: Hsla,
    origin: impl FnOnce(Pixels, Pixels) -> Point<Pixels>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    paint_text_label(
        super::time_series::format_value_label(value),
        color,
        origin,
        window,
        cx,
    );
}

/// Shape and paint a label, allowing its final position to depend on the
/// shaped width. Time-series uses this for its non-numeric X labels.
pub fn paint_text_label(
    text: impl Into<SharedString>,
    color: Hsla,
    origin: impl FnOnce(Pixels, Pixels) -> Point<Pixels>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let text = text.into();
    let font_size = px(LABEL_FONT_SIZE);
    let run = TextRun {
        len: text.len(),
        font: window.text_style().font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let shaped = window
        .text_system()
        .shape_line(text, font_size, &[run], None);
    let _ = shaped.paint(origin(shaped.width, font_size), font_size, window, cx);
}

/// Sync the tracker and task maps to the current `traces`: drop entries whose
/// trace disappeared, then `make` a tracker for each newly-added trace.
///
/// Keyed by [`EntityId`] so reordering `traces` leaves live tasks untouched.
pub fn reconcile_trackers<Tr: 'static, K>(
    traces: &[Entity<Tr>],
    tracking: &mut HashMap<EntityId, K>,
    tasks: &mut HashMap<EntityId, Task<()>>,
    mut make: impl FnMut(EntityId, &Entity<Tr>) -> (K, Task<()>),
) {
    let current: HashSet<EntityId> = traces.iter().map(|e| e.entity_id()).collect();
    tracking.retain(|id, _| current.contains(id));
    tasks.retain(|id, _| current.contains(id));
    for trace in traces {
        let id = trace.entity_id();
        if let std::collections::hash_map::Entry::Vacant(slot) = tracking.entry(id) {
            let (state, task) = make(id, trace);
            slot.insert(state);
            tasks.insert(id, task);
        }
    }
}
