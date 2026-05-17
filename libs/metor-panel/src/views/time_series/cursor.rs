//! Right-click-drag measurement cursors on a [`TimeSeriesPlot`].
//!
//! A cursor selects a closed timestamp range on a focused trace and feeds
//! that range into the inspector for one-shot statistic readouts. Cursors
//! live as gpui entities so the inspector can subscribe to changes (toggling
//! a measurement, dragging a handle later) without going through the parent
//! plot — the parent only owns the list and the in-progress drag state.

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{Context, Entity, EntityId, WeakEntity};
use metor_proto::types::Timestamp;

use super::measurements::{MeasurementKind, MeasurementKindList};
use super::{LinePlot, TimeSeriesPlot};

/// Bytes within a cursor line treated as a click hit. Wider than the painted
/// stroke so a user landing within a few pixels still opens the inspector
/// instead of starting a new cursor.
pub const CURSOR_HIT_PIXELS: f32 = 6.0;

/// Monotonic id source; cursors compare by `id` rather than `Entity` ptr
/// because two cursor entities can render simultaneously and the parent
/// plot's list shuffles when one is removed.
static NEXT_CURSOR_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorId(pub u64);

/// One measurement cursor.
///
/// Endpoints are stored as raw timestamps (panel uses microseconds). The
/// focused trace is referenced by gpui `EntityId` so reordering the parent
/// plot's trace list doesn't invalidate the cursor; the cursor falls back
/// to "no focused trace" if the entity is gone.
pub struct MeasurementCursor {
    pub id: CursorId,
    pub t_start: Timestamp,
    pub t_end: Timestamp,
    pub focused_trace: Option<EntityId>,
    pub enabled: MeasurementKindList,
    /// Back-reference to the plot's `LinePlot`, used by the inspector row
    /// builder to read live trace values at the cursor endpoints.
    pub line_plot: WeakEntity<LinePlot>,
    /// Back-reference to the owning plot for the "Delete cursor" action.
    pub host: WeakEntity<TimeSeriesPlot>,
}

impl MeasurementCursor {
    pub fn new(
        t_start: Timestamp,
        t_end: Timestamp,
        focused_trace: Option<EntityId>,
        enabled: MeasurementKindList,
        line_plot: WeakEntity<LinePlot>,
        host: WeakEntity<TimeSeriesPlot>,
    ) -> Self {
        Self {
            id: CursorId(NEXT_CURSOR_ID.fetch_add(1, Ordering::Relaxed)),
            t_start,
            t_end,
            focused_trace,
            enabled,
            line_plot,
            host,
        }
    }

    /// Ensure `t_start <= t_end` regardless of which endpoint the user
    /// dragged out from. The cursor itself is direction-agnostic; the
    /// reductions all consume the ordered range.
    pub fn ordered(&self) -> (Timestamp, Timestamp) {
        if self.t_start.0 <= self.t_end.0 {
            (self.t_start, self.t_end)
        } else {
            (self.t_end, self.t_start)
        }
    }

    pub fn span_us(&self) -> i64 {
        let (a, b) = self.ordered();
        b.0 - a.0
    }

    pub fn toggle_kind(&mut self, kind: MeasurementKind, on: bool, cx: &mut Context<Self>) {
        let present = self.enabled.iter().position(|k| *k == kind);
        match (present, on) {
            (None, true) => {
                self.enabled.push(kind);
            }
            (Some(idx), false) => {
                self.enabled.remove(idx);
            }
            _ => {}
        }
        cx.notify();
    }

    pub fn has_kind(&self, kind: MeasurementKind) -> bool {
        self.enabled.contains(&kind)
    }
}

/// Snap a data-space X to the nearest sample timestamp on `focused_trace`.
///
/// Returns the requested X (as a `Timestamp`) when the focused trace has no
/// resolved component yet, so the cursor can still be placed and will snap
/// on the next call once data arrives.
pub fn snap_x_to_sample(
    line_plot: &LinePlot,
    focused_trace: Option<EntityId>,
    x_data: f64,
    cx: &gpui::App,
) -> Timestamp {
    let target = Timestamp(x_data as i64);
    let Some(entity_id) = focused_trace else {
        return target;
    };
    let Some(trace) = line_plot
        .traces()
        .iter()
        .find(|t| t.entity_id() == entity_id)
    else {
        return target;
    };
    let Some(component) = line_plot.component_for_trace(trace, cx) else {
        return target;
    };
    nearest_sample_timestamp(&component, target).unwrap_or(target)
}

/// Binary-search the live node slices of a component for the timestamp
/// closest to `target`. Used by both [`snap_x_to_sample`] and the
/// trace-value lookups for Δy readouts.
pub fn nearest_sample_timestamp(
    component: &metor_db::Component,
    target: Timestamp,
) -> Option<Timestamp> {
    let mut best: Option<Timestamp> = None;
    let mut best_dist = i64::MAX;
    for node in component.time_series.list.iter() {
        let timestamps = node.timestamps();
        if timestamps.is_empty() {
            continue;
        }
        let idx = match timestamps.binary_search_by_key(&target.0, |t| t.0) {
            Ok(i) => i,
            Err(i) => i,
        };
        for cand_idx in [idx.saturating_sub(1), idx, idx + 1] {
            let Some(cand) = timestamps.get(cand_idx) else {
                continue;
            };
            let d = (cand.0 - target.0).abs();
            if d < best_dist {
                best_dist = d;
                best = Some(*cand);
            }
        }
    }
    best
}

/// Pick the visible trace whose plotted Y at `x_ts` is nearest (in screen
/// pixels) to `cursor_screen_y`. Falls back to the first visible trace when
/// no trace has data at this X.
///
/// Used at right-mouse-down to decide which trace's grid the cursor should
/// snap to. The choice is sticky for the duration of the drag — it does not
/// re-evaluate on move events, so the cursor always traces the same axis.
pub fn focused_trace_at(
    line_plot: &LinePlot,
    x_ts: Timestamp,
    cursor_screen_y: gpui::Pixels,
    plot_area: gpui::Bounds<gpui::Pixels>,
    view: super::PlotBounds,
    cx: &gpui::App,
) -> Option<EntityId> {
    let mut first_visible: Option<EntityId> = None;
    let mut best: Option<(EntityId, f32)> = None;
    for trace in line_plot.traces() {
        let cfg = trace.read(cx);
        if !cfg.visible {
            continue;
        }
        if first_visible.is_none() {
            first_visible = Some(trace.entity_id());
        }
        let Some(y) = line_plot.trace_value_at(trace.entity_id(), x_ts, cx) else {
            continue;
        };
        let screen = view.to_screen(plot_area, x_ts.0 as f64, y);
        let dist = (f32::from(screen.y) - f32::from(cursor_screen_y)).abs();
        if best.map(|(_, d)| dist < d).unwrap_or(true) {
            best = Some((trace.entity_id(), dist));
        }
    }
    best.map(|(id, _)| id).or(first_visible)
}

/// Distance in pixels from `pixel_x` to the nearest of a cursor's two
/// endpoint vertical lines, or `f32::INFINITY` if the cursor's bounds
/// aren't resolvable in the current view.
pub fn hit_distance(
    cursor: &MeasurementCursor,
    pixel_x: gpui::Pixels,
    plot_area: gpui::Bounds<gpui::Pixels>,
    view: super::PlotBounds,
) -> f32 {
    let (a, b) = cursor.ordered();
    let xa = view.to_screen(plot_area, a.0 as f64, view.min_y).x;
    let xb = view.to_screen(plot_area, b.0 as f64, view.min_y).x;
    let px = f32::from(pixel_x);
    (f32::from(xa) - px).abs().min((f32::from(xb) - px).abs())
}

/// Find the topmost cursor whose endpoint line is within
/// [`CURSOR_HIT_PIXELS`] of `event_x`.
///
/// "Topmost" matches the rendering order: the most-recently-added cursor
/// paints last so it visually sits above earlier ones; we iterate the list
/// in reverse and return the first hit.
pub fn cursor_at(
    cursors: &[Entity<MeasurementCursor>],
    event_x: gpui::Pixels,
    plot_area: gpui::Bounds<gpui::Pixels>,
    view: super::PlotBounds,
    cx: &gpui::App,
) -> Option<Entity<MeasurementCursor>> {
    for cursor in cursors.iter().rev() {
        let c = cursor.read(cx);
        if hit_distance(c, event_x, plot_area, view) <= CURSOR_HIT_PIXELS {
            return Some(cursor.clone());
        }
    }
    None
}

/// Convert a pixel X within the plot area to a data X using the view at
/// drag-start time (the snapshot pinned in `cursor_drag.start_view`). Mirrors
/// the bounds-pinning pattern used by the left-click pan handler.
pub fn pixel_to_data_x(
    pixel_x: gpui::Pixels,
    plot_area: gpui::Bounds<gpui::Pixels>,
    view: super::PlotBounds,
) -> f64 {
    let pa_x = f32::from(plot_area.origin.x) as f64;
    let pa_w = f32::from(plot_area.size.width).max(1.0) as f64;
    let norm = (f32::from(pixel_x) as f64 - pa_x) / pa_w;
    view.min_x + norm.clamp(0.0, 1.0) * (view.max_x - view.min_x)
}
