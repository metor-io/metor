//! The native-gpui measurement readout panels.
//!
//! Replaces the manually-painted text badges that used to sit above each
//! cursor's band. The plot-level `panel_position` decides between two
//! shapes:
//!
//! - [`PanelPosition::Track`] — one mini-panel per cursor, anchored above
//!   its band. Each shows that cursor's measurements only. Dragging a mini
//!   panel collapses everything into a single consolidated panel at the
//!   drag location.
//! - [`PanelPosition::Pinned`] — one consolidated panel at the user-chosen
//!   plot-local coordinates, sectioned by cursor. The track-icon button
//!   restores the per-cursor mini-panels.
//!
//! Drag state lives on [`super::TimeSeriesPlot`]; this module is the pure
//! renderer.

use gpui::{
    AnyElement, Context, Entity, Hsla, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, SharedString, div, prelude::*, px,
};

use super::cursor::MeasurementCursor;
use super::measurements::{self, MeasurementKind};
use super::{LinePlot, TimeSeriesPlot, Trace};
use crate::icons::Icon;
use crate::theme::theme;

/// Where the measurement readout sits inside its host plot.
///
/// Coordinates are local to the plot's interior `relative()` container so
/// pinned positions are stable across window moves and round-trip cleanly
/// through layout serialization.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PanelPosition {
    /// Per-cursor mini-panels above each cursor's band.
    Track,
    /// Single consolidated panel at a user-pinned plot-local offset.
    Pinned { x: f32, y: f32 },
}

impl Default for PanelPosition {
    fn default() -> Self {
        Self::Track
    }
}

/// In-flight drag of a panel's title bar.
///
/// `offset` is the pointer's position relative to the panel's top-left at
/// drag-start; subsequent moves keep that offset so the panel doesn't
/// teleport on the first frame.
#[derive(Clone, Copy, Debug)]
pub struct PanelDragState {
    pub offset: Point<Pixels>,
}

const PANEL_WIDTH: f32 = 240.0;
const PANEL_PAD: f32 = 8.0;
const TITLE_HEIGHT: f32 = 22.0;
const ROW_HEIGHT: f32 = 22.0;
const SECTION_BAR_WIDTH: f32 = 3.0;
const SWATCH_SIZE: f32 = 8.0;
const LABEL_FONT_SIZE: f32 = 11.0;

/// One renderable panel: its plot-area-local origin (top-left, in the plot's
/// interior coordinate space) plus the card element.
pub struct RenderedPanel {
    pub origin: Point<Pixels>,
    pub element: AnyElement,
}

/// Build every measurement panel that should be shown for `plot`.
///
/// In `Track` mode this is one mini-panel per cursor. In `Pinned` mode it's
/// a single consolidated panel listing all cursors. Returns an empty vec
/// when no cursors exist.
pub fn render_panels(
    plot: &TimeSeriesPlot,
    cx: &mut Context<TimeSeriesPlot>,
) -> Vec<RenderedPanel> {
    if plot.cursors().is_empty() {
        return Vec::new();
    }
    match plot.panel_position() {
        PanelPosition::Track => render_track_mode(plot, cx),
        PanelPosition::Pinned { x, y } => {
            let origin = clamp_origin(Point { x: px(x), y: px(y) }, plot);
            let element = render_consolidated(plot, origin, cx);
            vec![RenderedPanel { origin, element }]
        }
    }
}

fn render_track_mode(
    plot: &TimeSeriesPlot,
    cx: &mut Context<TimeSeriesPlot>,
) -> Vec<RenderedPanel> {
    let theme = theme(cx);
    let line_plot_entity = plot.line_plot().clone();
    let plot_area = plot.last_plot_area();
    let view = line_plot_entity.read(cx).effective_view(cx);
    let cursors: Vec<Entity<MeasurementCursor>> = plot.cursors().to_vec();

    let mut out: Vec<RenderedPanel> = Vec::with_capacity(cursors.len());
    for cursor in cursors.iter() {
        // Read once to snapshot the cursor's data without holding the
        // borrow across the (mutable) listener-installing render call.
        let (cursor_id, mid_t) = {
            let c = cursor.read(cx);
            let (a, b) = c.ordered();
            (c.id.0, (a.0 + b.0) as f64 / 2.0)
        };

        // Compute the cursor band midpoint in plot-area-local pixels. Fall
        // back to the top-left if the view isn't resolved yet (e.g. on the
        // very first frame before the plot has data).
        let mid_x_pa_local = match (plot_area, view) {
            (Some(pa), Some(v)) => v.to_screen(pa, mid_t, v.min_y).x - pa.origin.x,
            _ => px(PANEL_WIDTH / 2.0 + PANEL_PAD),
        };
        let preferred = Point {
            x: mid_x_pa_local - px(PANEL_WIDTH / 2.0),
            y: px(PANEL_PAD),
        };
        let origin = clamp_origin(preferred, plot);

        let element = render_mini_panel(cursor, cursor_id, &line_plot_entity, origin, &theme, cx);
        out.push(RenderedPanel { origin, element });
    }
    out
}

fn render_mini_panel(
    cursor: &Entity<MeasurementCursor>,
    cursor_id_seed: u64,
    line_plot_entity: &Entity<LinePlot>,
    origin: Point<Pixels>,
    theme: &crate::theme::Theme,
    cx: &mut Context<TimeSeriesPlot>,
) -> AnyElement {
    let (delta_t, row_elements) = {
        let c = cursor.read(cx);
        let (start, end) = c.ordered();
        let delta_t =
            measurements::format_value(MeasurementKind::DeltaX, (end.0 - start.0) as f64);
        let line_plot = line_plot_entity.read(cx);
        let rows = measurement_rows_for_cursor(c, line_plot, theme, cx);
        (delta_t, rows)
    };

    let title = mini_title_bar(
        cursor_id_seed,
        SharedString::from(format!("Δt {}", delta_t)),
        theme,
        origin,
        cx,
    );

    div()
        .id(("measurement-mini-panel", cursor_id_seed as usize))
        .flex()
        .flex_col()
        .w(px(PANEL_WIDTH))
        .bg(theme.bg_secondary)
        .border_1()
        .border_color(theme.border_primary)
        .rounded(px(4.0))
        .shadow_sm()
        // The panel sits over the plot canvas; without this, gpui lets
        // mouse events fall through to the cursor canvas handlers below.
        .occlude()
        // Drag mouse-move/up handlers on the panel root: needed because
        // `occlude()` blocks the wrapper's `on_mouse_move` from firing
        // while the cursor is over the panel — and a dragged panel
        // follows the cursor, so the cursor sits on the panel almost the
        // whole time. Without these the wrapper handler only fires when
        // the cursor leaves the panel.
        .on_mouse_move(cx.listener(
            |plot, event: &MouseMoveEvent, _window, cx| {
                if event.dragging() {
                    plot.advance_panel_drag(event.position, cx);
                }
            },
        ))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|plot, _event: &MouseUpEvent, _window, _cx| {
                plot.end_panel_drag();
            }),
        )
        .child(title)
        .children(row_elements)
        .into_any_element()
}

fn render_consolidated(
    plot: &TimeSeriesPlot,
    _origin: Point<Pixels>,
    cx: &mut Context<TimeSeriesPlot>,
) -> AnyElement {
    let theme = theme(cx);
    let cursors: Vec<Entity<MeasurementCursor>> = plot.cursors().to_vec();
    let line_plot_entity = plot.line_plot().clone();

    let title = consolidated_title_bar(theme.text_primary, cx);

    let mut card = div()
        .id("measurement-consolidated-panel")
        .flex()
        .flex_col()
        .w(px(PANEL_WIDTH))
        .bg(theme.bg_secondary)
        .border_1()
        .border_color(theme.border_primary)
        .rounded(px(4.0))
        .shadow_sm()
        .occlude()
        // Drag handlers on the panel root — see `render_mini_panel` for
        // the rationale (`.occlude()` blocks the wrapper's listener when
        // the cursor is over the panel).
        .on_mouse_move(cx.listener(
            |plot, event: &MouseMoveEvent, _window, cx| {
                if event.dragging() {
                    plot.advance_panel_drag(event.position, cx);
                }
            },
        ))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|plot, _event: &MouseUpEvent, _window, _cx| {
                plot.end_panel_drag();
            }),
        )
        .child(title);

    for (idx, cursor) in cursors.iter().enumerate() {
        if idx > 0 {
            card = card.child(divider(theme.border_primary));
        }
        let line_plot = line_plot_entity.read(cx);
        let cursor_ref = cursor.read(cx);
        let (start, end) = cursor_ref.ordered();
        let delta_t =
            measurements::format_value(MeasurementKind::DeltaX, (end.0 - start.0) as f64);
        card = card
            .child(
                section_header(theme.selection_bg, theme.text_primary, delta_t)
                    .into_any_element(),
            )
            .children(measurement_rows_for_cursor(cursor_ref, line_plot, &theme, cx));
    }

    card.into_any_element()
}

fn mini_title_bar(
    cursor_id_seed: u64,
    label: SharedString,
    theme: &crate::theme::Theme,
    panel_origin: Point<Pixels>,
    cx: &mut Context<TimeSeriesPlot>,
) -> AnyElement {
    div()
        .id(("measurement-mini-title", cursor_id_seed as usize))
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(TITLE_HEIGHT))
        .px(px(PANEL_PAD))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |plot, event: &MouseDownEvent, _window, cx| {
                // Drag from a mini-panel collapses everything to a single
                // pinned panel at this drag location.
                plot.start_panel_drag(event.position, panel_origin, cx);
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .text_size(px(LABEL_FONT_SIZE))
                .text_color(theme.text_primary)
                .child(label),
        )
        .child(
            div()
                .id(("measurement-mini-pin", cursor_id_seed as usize))
                .flex()
                .items_center()
                .justify_center()
                .w(px(16.0))
                .h(px(16.0))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |plot, _event: &MouseDownEvent, _window, cx| {
                        // Click-without-drag on the lock icon: pin in
                        // place. All other mini panels disappear and this
                        // becomes the single consolidated panel.
                        plot.pin_panel_at(panel_origin, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(Icon::LockOpen.svg(12.0)),
        )
        .into_any_element()
}

fn consolidated_title_bar(
    text_primary: Hsla,
    cx: &mut Context<TimeSeriesPlot>,
) -> AnyElement {
    div()
        .id("measurement-consolidated-title")
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .h(px(TITLE_HEIGHT))
        .px(px(PANEL_PAD))
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|plot, event: &MouseDownEvent, _window, cx| {
                let origin = plot.consolidated_panel_origin();
                plot.start_panel_drag(event.position, origin, cx);
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .text_size(px(LABEL_FONT_SIZE))
                .text_color(text_primary)
                .child(SharedString::new_static("Measurements")),
        )
        .child(
            div()
                .id("measurement-consolidated-track")
                .flex()
                .items_center()
                .justify_center()
                .w(px(16.0))
                .h(px(16.0))
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|plot, _event: &MouseDownEvent, _window, cx| {
                        plot.unpin_panel(cx);
                        cx.stop_propagation();
                    }),
                )
                .child(Icon::Lock.svg(12.0)),
        )
        .into_any_element()
}

fn measurement_rows_for_cursor(
    cursor: &MeasurementCursor,
    line_plot: &LinePlot,
    theme: &crate::theme::Theme,
    cx: &gpui::App,
) -> Vec<AnyElement> {
    let (start, end) = cursor.ordered();
    let visible_traces: Vec<&Entity<Trace>> = line_plot
        .traces()
        .iter()
        .filter(|t| t.read(cx).visible)
        .collect();
    let mut samples_by_trace: std::collections::HashMap<
        gpui::EntityId,
        Option<std::sync::Arc<Vec<(metor_proto::types::Timestamp, f64)>>>,
    > = std::collections::HashMap::new();

    let mut rows: Vec<AnyElement> = Vec::new();
    for kind in cursor.enabled.iter().copied() {
        if matches!(kind, MeasurementKind::DeltaX) {
            // ΔX already appears in the section / title header.
            continue;
        }
        let mut value_cells: Vec<AnyElement> = Vec::new();
        for trace in &visible_traces {
            let cfg = trace.read(cx);
            let element_index = cfg.element_index;
            let samples = samples_by_trace
                .entry(trace.entity_id())
                .or_insert_with(|| {
                    line_plot
                        .component_for_trace(trace, cx)
                        .and_then(|c| measurements::collect_samples(&c, element_index, start..end))
                });
            let Some(s) = samples.as_ref() else {
                continue;
            };
            let Some(v) = measurements::reduce(kind, s) else {
                continue;
            };
            value_cells.push(
                value_cell(cfg.color, measurements::format_value(kind, v)).into_any_element(),
            );
        }
        if value_cells.is_empty() {
            continue;
        }
        rows.push(
            measurement_row(
                kind.label(),
                value_cells,
                theme.text_primary,
                theme.text_primary,
            )
            .into_any_element(),
        );
    }
    rows
}

fn section_header(
    bar_color: Hsla,
    text_color: Hsla,
    delta_t: SharedString,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(ROW_HEIGHT))
        .pr(px(PANEL_PAD))
        .child(
            div()
                .w(px(SECTION_BAR_WIDTH))
                .h(px(ROW_HEIGHT - 6.0))
                .ml(px(PANEL_PAD - SECTION_BAR_WIDTH))
                .mr(px(6.0))
                .rounded(px(1.0))
                .bg(bar_color),
        )
        .child(
            div()
                .text_size(px(LABEL_FONT_SIZE))
                .text_color(text_color)
                .child(SharedString::from(format!("Δt {}", delta_t))),
        )
}

fn measurement_row(
    label: &'static str,
    value_cells: Vec<AnyElement>,
    label_color: Hsla,
    value_text_color: Hsla,
) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(ROW_HEIGHT))
        .px(px(PANEL_PAD))
        .gap(px(8.0))
        .text_color(value_text_color)
        .child(
            div()
                .w(px(56.0))
                .text_size(px(LABEL_FONT_SIZE))
                .text_color(label_color)
                .child(SharedString::new_static(label)),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap_x(px(8.0))
                .gap_y(px(2.0))
                .children(value_cells),
        )
}

fn value_cell(swatch_color: Hsla, value: SharedString) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
        .child(
            div()
                .w(px(SWATCH_SIZE))
                .h(px(SWATCH_SIZE))
                .rounded(px(2.0))
                .bg(swatch_color),
        )
        .child(div().text_size(px(LABEL_FONT_SIZE)).child(value))
}

fn divider(color: Hsla) -> impl IntoElement {
    div().h(px(1.0)).w_full().bg(color)
}

/// Clamp `preferred` so the panel stays inside the plot area minus a small
/// margin. Returns the same point unchanged when `last_plot_area` isn't
/// resolved yet.
fn clamp_origin(preferred: Point<Pixels>, plot: &TimeSeriesPlot) -> Point<Pixels> {
    let Some(plot_area) = plot.last_plot_area() else {
        return preferred;
    };
    let panel_height_estimate = px(80.0);
    let max_x = (plot_area.size.width - px(PANEL_WIDTH) - px(PANEL_PAD)).max(px(0.0));
    let max_y = (plot_area.size.height - panel_height_estimate - px(PANEL_PAD)).max(px(0.0));
    Point {
        x: preferred.x.clamp(px(0.0), max_x),
        y: preferred.y.clamp(px(0.0), max_y),
    }
}
