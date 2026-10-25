//! Mechanics shared by the GPU-backed plots (`time_series`, `xy_plot`,
//! `list_plot`, `spectrogram`).
//!
//! The line-plot entities keep their own struct shapes — the inspector
//! reflects a different set of fields for each (multi-axis vs. flat scalar
//! overrides) — but their tracker bookkeeping, frame canvas, chrome painters,
//! gestures and LoD selection are the same, so they live here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpui::{
    Bounds, Context, Corners, Entity, EntityId, Hsla, IntoElement, MouseButton, Pixels, Point,
    Rems, SharedString, Styled, Task, TextAlign, TextRun, WeakEntity, Window, canvas, div,
    prelude::*, px,
};

use crate::theme::{Theme, ui_px, ui_scale};
use crate::views::time_series::{
    LABEL_FONT_SIZE, Override, PlotBounds, PlotRenderState, ReadbackHandle,
};

pub(crate) mod chrome;
pub(crate) mod lod;
mod numeric;
mod pan_zoom;

pub use numeric::{NumericLinePlot, NumericPlot};
pub(crate) use pan_zoom::PanZoom;

/// Build the common interactive plot legend.
///
/// `notify` is the line-plot entity whose renderer and bounds cache need to
/// be refreshed after a trace visibility change.
pub fn plot_legend<T: 'static, N: 'static>(
    traces: &[Entity<T>],
    notify: Entity<N>,
    left: Rems,
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
        let color = Theme::dim(trace_color, opacity);
        let text_color = Theme::dim(theme.text_secondary, opacity);
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
                .child(
                    div()
                        .w(ui_px(10.0))
                        .h(ui_px(10.0))
                        .rounded(px(2.0))
                        .bg(color),
                )
                .child(
                    div()
                        .text_size(ui_px(LABEL_FONT_SIZE))
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
    let font_size = px(LABEL_FONT_SIZE * ui_scale(window));
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
    let _ = shaped.paint(
        origin(shaped.width, font_size),
        font_size,
        TextAlign::Left,
        None,
        window,
        cx,
    );
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

/// Point each trace's back-reference at `cx`'s entity, so the trace's
/// inspector page can reach its plot (to rebind, or list sibling axes).
///
/// Mutating without notifying cannot re-enter reconcile: the plots do not
/// observe their traces.
pub(crate) fn link_traces<T: 'static, P: 'static>(
    traces: &[Entity<T>],
    link: fn(&mut T) -> &mut Option<WeakEntity<P>>,
    cx: &mut Context<P>,
) {
    let self_weak = cx.entity().downgrade();
    let self_id = cx.entity().entity_id();
    for trace in traces {
        trace.update(cx, |t, _| {
            let slot = link(t);
            let linked = slot
                .as_ref()
                .and_then(|w| w.upgrade())
                .map(|e| e.entity_id());
            if linked != Some(self_id) {
                *slot = Some(self_weak.clone());
            }
        });
    }
}

/// The canvas that shows a plot entity's GPU frame.
///
/// Prepaint runs `submit` to start a render for the current bounds (it
/// returns the readback when one was issued), releases the image the last
/// frame replaced back to the atlas, and paints the newest completed frame.
/// That frame was drawn at an earlier view — readback is asynchronous — so
/// it is placed under the current one ([`PlotRenderState::placement`]) and
/// clipped to the canvas: a pan moves the lines with the pointer, and the
/// frame drawn at the new view fills in the edge it uncovers.
/// `gpu_state` reaches the entity's [`PlotRenderState`].
pub(crate) fn gpu_frame<T: 'static>(
    entity: WeakEntity<T>,
    gpu_state: fn(&mut T) -> &mut PlotRenderState,
    submit: impl FnOnce(&mut T, Bounds<Pixels>, f32, &mut Context<T>) -> Option<ReadbackHandle>
    + 'static,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            let scale_factor = window.scale_factor();
            let (frame, released) = entity
                .update(cx, |plot, cx| {
                    if let Some(handle) = submit(plot, bounds, scale_factor, cx) {
                        handle.spawn_and_set(cx, gpu_state);
                    }
                    let state = gpu_state(plot);
                    let frame = state
                        .current_frame()
                        .map(|img| (img, state.placement(bounds, scale_factor)));
                    (frame, state.take_pending_release())
                })
                .unwrap_or((None, None));
            if let Some(img) = released {
                crate::atlas::release_image(window, cx, img);
            }
            (bounds, frame)
        },
        |_,
         (bounds, frame): (
            Bounds<Pixels>,
            Option<(Arc<gpui::RenderImage>, Bounds<Pixels>)>,
        ),
         window,
         _cx| {
            if let Some((img, rect)) = frame {
                let _ = window.paint_image(bounds, rect, Corners::default(), img, 0, false);
            }
        },
    )
    .size_full()
}

/// Install `view` as an interactive override, reporting whether it changed.
///
/// Compares bit patterns: [`PlotBounds`] holds `f64`s and has no `PartialEq`.
pub(crate) fn replace_view_override(
    slot: &mut Option<PlotBounds>,
    view: Option<PlotBounds>,
) -> bool {
    if slot.map(|b| b.bits()) == view.map(|b| b.bits()) {
        return false;
    }
    *slot = view;
    true
}

/// The custom values of a plot's reflected `[x_min, x_max, y_min, y_max]`
/// overrides. Compared on each reconcile so an inspector edit can clear the
/// interactive view override.
pub(crate) type BoundOverrides = [Option<f64>; 4];

pub(crate) fn bound_overrides(overrides: [&Override<f64>; 4]) -> BoundOverrides {
    overrides.map(|o| o.as_custom().copied())
}

/// Title for a plot with no custom one: the only trace's label when it has
/// one, `kind` when it doesn't, and `kind` with a count for several traces.
pub(crate) fn derive_title<T: 'static>(
    kind: &'static str,
    traces: &[Entity<T>],
    label: fn(&T) -> &SharedString,
    cx: &gpui::App,
) -> SharedString {
    match traces {
        [] => kind.into(),
        [only] => {
            let label = label(only.read(cx));
            if label.is_empty() {
                kind.into()
            } else {
                label.clone()
            }
        }
        _ => SharedString::from(format!("{kind} ({} traces)", traces.len())),
    }
}
