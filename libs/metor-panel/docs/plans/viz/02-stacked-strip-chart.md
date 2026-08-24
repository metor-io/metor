# Stacked strip-chart plot

Item 2 of `docs/plans/telemetry-viz-additions.md`.

## Summary

The time-series plot already models "N groups of traces, each with its own Y
range" — that is exactly `LinePlot::axes: Vec<Entity<YAxis>>` plus
`Trace::axis_index`. Today those axes *overlay* in one rect with their tick
columns stacked leftward, capped at four. Strip-chart mode is the same data
model rendered with the axes stacked *vertically* into lanes sharing one X
axis. This is a layout mode on the existing plot, not a new view.

## Reuse vs. new

**Decision: option (c) — generalize the existing `LinePlot` axis model.** Add
`AxisLayout { Overlay, Stacked }` to `LinePlot`; in `Stacked` each `YAxis`
gets a horizontal band of the plot rect instead of a Y-tick column to the
left. No new `PaneItem`, no new `WidgetKind`, no new config type.

Why the other two lose:

- **(a) Linked `LinePlot`s in a tile split.** Nothing in `src/tiles/` has a
  link-group concept, and pan/zoom is a per-plot `LinePlot::view_override`
  (`line_plot.rs:201`) that `GlobalTimeRange` (`time_range.rs:161`) does not
  reach — `GlobalTimeRange` carries a `TimeRangeBehavior`, not an interactive
  view. Linking would mean a second global for imperative view state plus
  cross-pane hover sync, and every sub-plot would still repeat its own X tick
  row, its own left chrome, and its own legend. The shared readout column has
  no home in a split at all. Worst cost/benefit of the three.
- **(b) A new stacked view owning sub-plots.** It would re-implement
  `TimeSeriesPlot`'s measurement cursors, event flags, hover readout, alarm
  tint/limit lines, LoD selection and GPU readback. `plot-shell-unification.md`
  explicitly keeps `time_series` out of the generic shell, and
  `metor-panel-simplification.md` Phase 5.5 says "do not introduce ...
  extension slots for Time Series". A second time-series view is the thing both
  plans forbid.

Option (c) also lands on **both surfaces for free**: `PlotPanelConfig`
(`views/time_series/config.rs:20`) is the config for the `PlotPanel` pane item
(`tiles/panels.rs:878`) *and* for the dashboard `WidgetKind::plot()`
(`views/dashboard/widgets.rs:225`), so a new field is registered on both by
construction — the `TrafficLight` rule satisfied with zero registration work.

**Renames.** `YAxis` stays: a lane *is* a Y axis with its own range; stacking
is layout, not a new concept, and renaming would churn `TraceConfig::axis_index`,
`YAxisConfig`, `register_entity_list::<LinePlot, YAxis>`
(`inspector/registry/defaults.rs:68`) and the axis picker
(`defaults.rs:369-433`) for nothing. Three chrome helpers do need reshaping,
because `axis_count` alone no longer determines geometry: `left_margin`,
`axis_zone` and `plot_area` (`views/time_series/mod.rs:317-350`) collapse into
one `PlotChrome { layout, axis_count }` value type. That is a bundle of the
context already threaded pairwise through ~12 call sites, per STYLE.md
("bundle shared immutable context into a struct"), and it moves ~60 lines out
of the 2698-line `mod.rs`.

## Design

### Config

```rust
// views/time_series/bounds.rs
pub enum AxisLayout { #[default] Overlay, Stacked }
```

- `LinePlot::axis_layout: AxisLayout` — a fieldless facet enum, so the
  inspector renders it as an `EnumRow` through the existing dispatch path
  (`inspector/registry/dispatch.rs:116-140`), exactly like `x_time_format`.
- `PlotPanelConfig::axis_layout: AxisLayout` with `#[serde(default)]`, wired in
  `TimeSeriesPlot::from_config`/`to_config` (`config.rs:181-282`).
- `LinePlot::MAX_AXES` (`line_plot.rs:161`) becomes
  `fn max_axes(layout) -> usize` — 4 for `Overlay` (the left-chrome budget that
  justified the constant), 8 for `Stacked` (no left chrome is consumed).
  `reconcile` (`line_plot.rs:359`) and `from_config` (`config.rs:196`) call it.

Creating a strip chart is then: add axes in the inspector's existing "Axes"
list, assign traces via the existing axis picker, flip `Axis Layout` to
`Stacked`. A "Stacked Plot" palette row in `new_panel_rows`
(`tiles/panels.rs:1122`) seeds `axis_layout: Stacked` with one axis per picked
component so the common case is one click.

### Lane geometry — no GPU change

`PlotView` (`bounds.rs:12`) gains `layout: AxisLayout` and two methods:

```rust
/// Normalized `(bottom, top)` band of axis `i` inside the plot rect.
/// Axis 0 is the top lane; `Overlay` gives every axis the whole rect.
pub fn lane_band(&self, i: usize) -> (f64, f64)

/// Y range to normalize axis `i`'s data against so it lands in its lane.
/// Equals `axis_bounds(i)` under `Overlay`.
pub fn draw_bounds(&self, i: usize) -> PlotBounds
```

`draw_bounds` is the whole trick. `gpu.rs:550-552` normalizes a trace as
`(v - y_min) / (y_max - y_min)` across the *full* canvas, so widening the range
by the reciprocal of the lane's height and shifting by its offset places the
trace inside the band. `span = (max_y - min_y) / (top - bottom)`,
`min_y' = min_y - bottom * span`. Nothing in `gpu.rs`, `line.wgsl`, or
`PlotRenderState` changes; `LinePlot::render` just reads `draw_bounds` instead
of `axis_bounds` at `line_plot.rs:770`.

Every other place that pre-normalizes a value into the full-rect frame swaps
the same way: cursor trace markers (`mod.rs:2511`), hover markers
(`mod.rs:2559`), cursor focus hit-testing (`cursor.rs:209`), the axis-membership
triangles (`mod.rs:788`), and alarm limit lines (`mod.rs:808`).
`axis_bounds(i)` keeps its current meaning — the axis's true data range — and
stays the source for tick *values*.

### Chrome

New `views/time_series/chrome.rs` holds `Y_LABEL_WIDTH`, `X_LABEL_HEIGHT`,
`PADDING`, `LABEL_FONT_SIZE`, `AxisZone`, and:

```rust
pub(crate) struct PlotChrome { layout: AxisLayout, axis_count: usize }
impl PlotChrome {
    pub const SINGLE: Self;                                     // xy/list plots
    fn left_margin(self) -> f32;                                // stacked: one column
    fn plot_area(self, outer: Bounds<Pixels>) -> Bounds<Pixels>;
    fn lane_rect(self, pa: Bounds<Pixels>, i: usize) -> Bounds<Pixels>;
    fn zone(self, pos: Point<Pixels>, pa: Bounds<Pixels>) -> AxisZone;
}
```

`PlotView::chrome()` builds one. Under `Stacked`, `left_margin` is
`Y_LABEL_WIDTH + PADDING` regardless of axis count (every lane labels its Y in
the same column) and `lane_rect` splits the plot rect into equal horizontal
bands. Under `Overlay` both behave exactly as today, so the existing rendering
is bit-identical.

`paint_underlay` (`mod.rs:599`) loops lanes: gridlines and the zero line come
from each lane's own `axis_bounds(i)` mapped through `lane_rect(pb, i)`, with
the tick target scaled by lane height (`(lane_px / 40.0).clamp(2, 5)`) so a
quarter-height lane isn't five gridlines deep. `paint_overlay` (`mod.rs:662`)
does the same for Y tick labels, then paints one hairline separator between
lanes and the `YAxis::label` at each lane's top-left. X tick labels and the X
rule are painted once at the bottom — they already track `axis_bounds(0)`'s X,
which is shared.

### Shared X, pan, zoom

The X range is a single `PlotView::x` shared by construction — there is no
sync problem to solve, which is the core structural argument for (c). Only the
Y gesture needs to become lane-local. `AxisZone::Plot` grows a lane:

```rust
enum AxisZone { Plot { lane: Option<usize> }, XAxis, YAxis(usize) }
```

`None` is overlay (the body belongs to every axis at once). Handlers in
`TimeSeriesPlot::render` (`mod.rs:2327` pan, `mod.rs:2352` zoom):

| zone | pan | zoom |
|---|---|---|
| `Plot { lane: None }` | `offset_x(-nx).offset_y_all(ny)` | `zoom_x(f, ax).zoom_y_all(f, 1-ay)` |
| `Plot { lane: Some(i) }` | `offset_x(-nx).offset_axis_y(i, ny / band)` | `zoom_x(f, ax).zoom_axis_y(i, f, anchor_in_lane)` |
| `XAxis` | `offset_x(-nx)` | `zoom_x(f, ax)` |
| `YAxis(i)` | `offset_axis_y(i, ny / band)` | `zoom_axis_y(i, f, anchor_in_lane)` |

`band` is `lane_band(i)` height (1.0 under overlay), so the pixel-to-fraction
conversion in `screen_delta_to_norm` stays against the full rect and the
existing overlay arms are unchanged expressions. No new `PlotView` methods are
needed — `offset_axis_y`/`zoom_axis_y` (`bounds.rs:76-93`) already exist for
per-axis gestures. Double-click reset (`reset_view`, `mod.rs:1589`) is
unchanged.

`xy_plot/mod.rs` and `list_plot/mod.rs` import `AxisZone`, `axis_zone` and
`plot_area` (both at their `mod.rs:23-27`); they pass `PlotChrome::SINGLE` and
match `AxisZone::Plot { .. }`. Mechanical, ~8 lines each.

### Shared cursor readout

Already built. `TimeSeriesPlot::hover_samples` (`mod.rs:1660`) collects
`(color, label, ts, value, axis_index)` for every visible trace at the
crosshair, and measurement cursors (`paint_cursors`, `mod.rs:874`) draw a
vertical line spanning the whole plot rect — i.e. across all lanes — for free,
which is the strip-chart behavior we want.

What changes is placement: `hover_readout` (`mod.rs:1691`) builds a floating
box that follows the pointer. Under `Stacked`, dock it instead — a fixed
column in the right gutter, rows grouped by `axis_index` under each lane's
label, aligned to the lane's vertical band. Same rows, same data path, one
`match self.layout` in the positioning tail (`mod.rs:1722-1728`) plus a
grouping pass over `HoverSample::axis_index`. The pinned measurement panel
(`measurement_panel.rs`) gets the same lane grouping in its per-trace row list.

### Serialization

`PlotPanelConfig` gains one defaulted field; `YAxisConfig` gains nothing in v1.
Per `metor-panel-simplification.md` rule 5 ("bump the current layout version
and reject older versions without conversion"), bump
`TILE_LAYOUT_VERSION` 5 → 6 in `libs/metor-proto/wkt/src/tile.rs:17` and add
the history line to the comment in `src/tiles/mod.rs:47-62`. Target-shipped
presets stamp the same constant (`libs/metor-fsw-2/src/preset/mod.rs:81`), so
this is a coordinated workspace bump, not a panel-local one.

## Implementation steps

Each step ends with `cargo build -p metor-panel` green.

1. **`AxisLayout` + lane math.** Add the enum, `PlotView::layout`,
   `lane_band`, `draw_bounds` to `views/time_series/bounds.rs`. Unit-test that
   `Overlay` makes `draw_bounds == axis_bounds`, and that under `Stacked` a
   value at `axis_bounds(i).min_y`/`max_y` normalizes to the band's bottom/top.
   *Touches:* `bounds.rs`.

2. **`PlotChrome`.** New `views/time_series/chrome.rs`; move the four layout
   constants, `AxisZone` (with the `Plot { lane }` variant), `left_margin`,
   `axis_zone`, `plot_area` out of `mod.rs`; add `lane_rect` and
   `PlotChrome::SINGLE`. Re-export from `time_series/mod.rs` so existing paths
   resolve. Update `views/xy_plot/mod.rs` and `views/list_plot/mod.rs` to
   `PlotChrome::SINGLE` and the new match arm.
   *Touches:* `chrome.rs` (new), `time_series/mod.rs`, `xy_plot/mod.rs`,
   `list_plot/mod.rs`, `event_flags.rs:176`.

3. **Plot-side layout state.** Add `LinePlot::axis_layout`, replace
   `MAX_AXES` with `max_axes(layout)`, populate `PlotView::layout` in
   `effective_view` (`line_plot.rs:414`), and switch the `LineDraw` Y range at
   `line_plot.rs:770` (both the LoD and raw draws) to `draw_bounds`.
   *Touches:* `line_plot.rs`.

4. **Chrome painting.** Per-lane gridlines/zero line in `paint_underlay`,
   per-lane Y tick labels + separators + lane labels in `paint_overlay`, X
   chrome once. Route the axis triangles and alarm limit lines through
   `draw_bounds`. Replace the `left_margin(view.axis_count())` call sites
   (`mod.rs:1726, 1965, 2090, 2216`) with `view.chrome().left_margin()`.
   *Touches:* `time_series/mod.rs`.

5. **Gestures.** Rewrite the pan/zoom match arms per the table above; update
   the three `axis_zone(...) != AxisZone::Plot` guards (`mod.rs:1472, 1501,
   1647`) to `matches!(.., AxisZone::Plot { .. })`.
   *Touches:* `time_series/mod.rs`.

6. **Cursors and readout.** `draw_bounds` in `cursor::focused_trace_at`
   (`cursor.rs:209`) and the two marker-normalization sites (`mod.rs:2511,
   2559`); dock + lane-group `hover_readout`; lane-group the measurement panel
   rows.
   *Touches:* `cursor.rs`, `time_series/mod.rs`, `measurement_panel.rs`.

7. **Persistence and creation.** `axis_layout` in `PlotPanelConfig` +
   `from_config`/`to_config`; bump `TILE_LAYOUT_VERSION` to 6 and extend the
   version history comment; add the "Stacked Plot" palette row. Round-trip
   test in the `tiles/panels.rs` config tests (`panels.rs:1571`) covering a
   three-axis stacked plot.
   *Touches:* `views/time_series/config.rs`, `libs/metor-proto/wkt/src/tile.rs`,
   `src/tiles/mod.rs`, `src/tiles/panels.rs`.

8. **Docs.** Update the `views/time_series/mod.rs` and `axis.rs` module docs to
   describe the two layouts; note the strip-chart mode in `CLAUDE.md`'s views
   paragraph.

Manual test matrix: overlay plots must be pixel-unchanged (1–4 axes, pan/zoom
in body / X / each Y column, alarm limits, cursors, event flags, LoD
decimation); stacked with 2/4/8 lanes must share X on every gesture, pan/zoom Y
only in the lane under the pointer, draw one cursor line across all lanes, and
round-trip through save/load.

## Open questions

1. **Per-lane heights.** v1 gives every lane an equal band. Adding
   `YAxisConfig::weight: f32` is small (`lane_band` divides by the weight sum)
   but adds a drag-to-resize gesture between lanes to be worth much. Ship equal
   heights first, or take weights now?
2. **Docked readout scope.** Should the docked lane-grouped readout be
   stacked-only, or a general `PanelPosition::Docked` that overlay plots can
   also choose (the survey's "value + context" principle argues for the
   latter)?
3. **`AxisZone::Plot { lane }` in the shared enum.** `xy_plot`/`list_plot` can
   never produce a lane. Alternative: leave `AxisZone` alone and have the
   time-series handler call `chrome.lane_at(pos, pa)` separately. The enum is
   more honest; the alternative touches two fewer files.
4. **Alarm tint granularity.** `alarm_plot_tint` (`mod.rs:551`) washes the
   whole plot rect from the worst active severity across all traces. Under
   stacked layout, should the tint be per-lane (only the offending lane
   washes)? Per-lane is more informative and matches the color-budget principle
   in the survey, but it means threading `axis_index` through the tint.
5. **Legend under stacked layout.** Keep the single wrap-flow legend
   (`plot_common::plot_legend`, invoked at `mod.rs:2613`), or drop it entirely
   when lanes are labelled and each lane holds one or two traces? A per-lane
   legend row would fight the lane labels for space.
6. **Stacked lane cap.** Is 8 the right ceiling for `max_axes(Stacked)`? Below
   ~30 px a lane cannot show two gridlines, so the real limit is pane height —
   should the cap instead be advisory (a "lane too short" hint) rather than a
   hard truncate in `reconcile`?
