//! Right-click-drag measurement cursors on a [`TimeSeriesPlot`].
//!
//! A cursor selects a closed timestamp range on a focused trace and feeds
//! that range into the inspector for one-shot statistic readouts. Cursors
//! live as gpui entities so the inspector can subscribe to changes (toggling
//! a measurement, dragging a handle later) without going through the parent
//! plot — the parent only owns the list and the in-progress drag state.

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{Bounds, Context, Entity, EntityId, Pixels, Point, Size, WeakEntity, point, px};
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
    let mut best: Option<(i64, Timestamp)> = None;
    for node in component.time_series.list.iter() {
        let timestamps = node.timestamps();
        let Some((idx, d)) = nearest_in_node(timestamps, target) else {
            continue;
        };
        if best.map(|(bd, _)| d < bd).unwrap_or(true) {
            best = Some((d, timestamps[idx]));
        }
    }
    best.map(|(_, ts)| ts)
}

/// Index of the timestamp in one node's slice nearest `target`, with its
/// absolute distance. The per-node half of the global nearest-sample scan:
/// [`nearest_sample_timestamp`] and [`LinePlot::trace_value_at`] both fold
/// it across a component's live nodes so the winner is the closest sample
/// anywhere, not just in the newest node.
pub(crate) fn nearest_in_node(timestamps: &[Timestamp], target: Timestamp) -> Option<(usize, i64)> {
    if timestamps.is_empty() {
        return None;
    }
    let idx = match timestamps.binary_search_by_key(&target.0, |t| t.0) {
        Ok(i) => i,
        Err(i) => i,
    };
    let mut best: Option<(usize, i64)> = None;
    for cand_idx in [idx.saturating_sub(1), idx, idx + 1] {
        let Some(cand) = timestamps.get(cand_idx) else {
            continue;
        };
        let d = (cand.0 - target.0).abs();
        if best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some((cand_idx, d));
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
    view: &super::PlotView,
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
        // Project against this trace's own axis so focus is correct even
        // when axes are zoomed independently.
        let screen = view
            .axis_bounds(cfg.axis_index)
            .to_screen(plot_area, x_ts.0 as f64, y);
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

/// Nominal per-character advance for the monospaced UI font at
/// [`LABEL_FONT_SIZE`](super::LABEL_FONT_SIZE). Used only to size the hover
/// readout box before layout runs, so an approximation is fine.
const READOUT_CHAR_W: f32 = 6.6;
/// Line height for one readout row (font size plus leading).
pub(crate) const READOUT_ROW_H: f32 = 15.0;
/// Inner padding on the readout box, per side.
pub(crate) const READOUT_PAD_X: f32 = 6.0;
pub(crate) const READOUT_PAD_Y: f32 = 4.0;
/// Width the color swatch plus its trailing gap reserves ahead of a trace
/// row's label.
const READOUT_SWATCH_W: f32 = 14.0;

/// Approximate pixel footprint of the hover readout box for a `header_len`
/// character header above `rows` of `(label_len, value_len)` character
/// counts.
///
/// The box is a native div tree whose real size only settles after layout,
/// but placement ([`readout_origin`]) has to run first — this estimate is
/// close enough for edge clamping given the monospaced UI font.
pub(crate) fn estimate_readout_size(header_len: usize, rows: &[(usize, usize)]) -> Size<Pixels> {
    let mut content_w = header_len as f32 * READOUT_CHAR_W;
    for (label_len, value_len) in rows {
        // label + a space + value, offset by the swatch column.
        let row_w = READOUT_SWATCH_W + (*label_len + 1 + *value_len) as f32 * READOUT_CHAR_W;
        content_w = content_w.max(row_w);
    }
    let width = content_w + READOUT_PAD_X * 2.0;
    let height = (1 + rows.len()) as f32 * READOUT_ROW_H + READOUT_PAD_Y * 2.0;
    Size {
        width: px(width),
        height: px(height),
    }
}

/// Top-left origin for the hover readout box.
///
/// Places the box down-and-right of the pointer by `offset` so it never
/// sits under the cursor, flips to the opposite side of the pointer when it
/// would overflow the plot's right or bottom edge, then clamps the result
/// inside `plot_area` so it can't spill past a pane border.
pub(crate) fn readout_origin(
    pointer: Point<Pixels>,
    box_size: Size<Pixels>,
    plot_area: Bounds<Pixels>,
    offset: Pixels,
) -> Point<Pixels> {
    let off = f32::from(offset);
    let (bw, bh) = (f32::from(box_size.width), f32::from(box_size.height));
    let left = f32::from(plot_area.origin.x);
    let top = f32::from(plot_area.origin.y);
    let right = left + f32::from(plot_area.size.width);
    let bottom = top + f32::from(plot_area.size.height);
    let (ptx, pty) = (f32::from(pointer.x), f32::from(pointer.y));

    let mut x = ptx + off;
    if x + bw > right {
        x = ptx - off - bw;
    }
    x = x.clamp(left, (right - bw).max(left));

    let mut y = pty + off;
    if y + bh > bottom {
        y = pty - off - bh;
    }
    y = y.clamp(top, (bottom - bh).max(top));

    point(px(x), px(y))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(x), px(y)),
            size: Size {
                width: px(w),
                height: px(h),
            },
        }
    }

    fn ts(values: &[i64]) -> Vec<Timestamp> {
        values.iter().map(|&t| Timestamp(t)).collect()
    }

    #[test]
    fn nearest_in_node_empty_slice() {
        assert_eq!(nearest_in_node(&[], Timestamp(5)), None);
    }

    #[test]
    fn nearest_in_node_exact_hit() {
        let t = ts(&[10, 20, 30]);
        assert_eq!(nearest_in_node(&t, Timestamp(20)), Some((1, 0)));
    }

    #[test]
    fn nearest_in_node_between_samples_picks_closer() {
        let t = ts(&[10, 20, 30]);
        assert_eq!(nearest_in_node(&t, Timestamp(24)), Some((1, 4)));
        assert_eq!(nearest_in_node(&t, Timestamp(26)), Some((2, 4)));
    }

    #[test]
    fn nearest_in_node_clamps_to_ends() {
        let t = ts(&[10, 20, 30]);
        assert_eq!(nearest_in_node(&t, Timestamp(-5)), Some((0, 15)));
        assert_eq!(nearest_in_node(&t, Timestamp(99)), Some((2, 69)));
    }

    #[test]
    fn readout_sits_down_and_right_when_it_fits() {
        let pa = bounds(0.0, 0.0, 400.0, 300.0);
        let size = Size {
            width: px(80.0),
            height: px(50.0),
        };
        let origin = readout_origin(point(px(100.0), px(100.0)), size, pa, px(12.0));
        assert_eq!(origin, point(px(112.0), px(112.0)));
    }

    #[test]
    fn readout_flips_left_and_up_near_the_far_edges() {
        let pa = bounds(0.0, 0.0, 400.0, 300.0);
        let size = Size {
            width: px(80.0),
            height: px(50.0),
        };
        // Pointer near the bottom-right corner: box flips to the pointer's
        // upper-left so it stays inside the plot.
        let origin = readout_origin(point(px(390.0), px(290.0)), size, pa, px(12.0));
        assert_eq!(origin, point(px(298.0), px(228.0)));
    }

    #[test]
    fn readout_clamps_into_plot_area() {
        let pa = bounds(50.0, 20.0, 100.0, 60.0);
        // Box wider/taller than the plot: origin pins to the top-left corner.
        let size = Size {
            width: px(200.0),
            height: px(120.0),
        };
        let origin = readout_origin(point(px(60.0), px(30.0)), size, pa, px(12.0));
        assert_eq!(origin, point(px(50.0), px(20.0)));
    }

    #[test]
    fn readout_size_grows_with_row_count_and_width() {
        let one = estimate_readout_size(4, &[(3, 4)]);
        let two = estimate_readout_size(4, &[(3, 4), (10, 6)]);
        assert!(two.height > one.height);
        assert!(two.width > one.width);
    }
}
