# 9 — Log-scale axes

## Summary

Give every value axis a `Linear`/`Log10` scale, selected per `YAxis` on the
time-series plot and per axis on the XY/list plots. The whole change reduces to
one idea: **the plot's stored ranges become _display space_** (already `log10`'d
for a log axis), so pan, zoom, `to_screen`, and the GPU view uniform need no
changes at all — only the handful of places that hand a *data* value to the
projection, the tick generator, and the single per-sample normalization step in
`gpu.rs` learn about the scale. Min/max envelope decimation is untouched because
`log10` is strictly monotone: the argmin/argmax of a bin is the same sample
either way, so a glitch that survives today survives on a log axis.

## Reuse vs. new

Extended:

- `views/time_series/bounds.rs` — gains `AxisScale`, `log_ticks`, and a
  scale-aware `pad_auto_range`. `PlotBounds`/`PlotView`/`ScreenTransform` are
  **not** touched: they already operate on whatever space the caller stores.
- `views/time_series/axis.rs` — `YAxis` gains `pub scale: AxisScale`. The
  inspector picks it up for free (any `facet` enum dispatches to `EnumRow`, the
  same path `PlotStyle` uses).
- `views/time_series/gpu.rs` — the five copies of `((v - y_epoch) * y_scale) as
  f32` collapse into one `YNorm::apply`. `LineDraw` gains `y_scale: AxisScale`;
  `y_min`/`y_max` change meaning from data space to display space (doc note).
- `views/time_series/mod.rs::value_ticks` stays the linear tick generator;
  `paint_underlay`/`paint_overlay` choose between it and `log_ticks`.
- `expand_value_bounds` / `expand_latest_sample_bounds` / `NodeBounds` gain a
  `min_positive` field — a log axis must auto-fit to the smallest *positive*
  sample, not to a zero that has no logarithm.

New: `AxisScale`, `log_ticks`, `YNorm`, and `ValueBounds`.

Coordinate with [11-value-density-heatmap.md](11-value-density-heatmap.md),
which renames `LineDraw` → `TraceDraw` and `LineUniform` → `TraceUniform`; if
that rename lands first, read those names throughout below.

Rename: `expand_value_bounds` keeps its name but returns
`Option<ValueBounds { min, max, min_positive }>` instead of
`Option<(f64, f64)>` — three call sites (`time_series/line_plot.rs` ×2 for raw
and LoD, `xy_plot/line_plot.rs` ×2).

## Design

### Display space

```rust
// views/time_series/bounds.rs
#[derive(Clone, Copy, Default, PartialEq, Eq, facet::Facet, Serialize, Deserialize)]
#[repr(u8)]
pub enum AxisScale {
    #[default]
    Linear,
    Log10,
}

impl AxisScale {
    /// Data value → display coordinate. Non-positive input on a log axis
    /// yields a non-finite value; every consumer treats that as "off plot".
    #[inline] pub fn forward(self, v: f64) -> f64;
    /// Display coordinate → data value. Used for tick labels and readouts.
    #[inline] pub fn inverse(self, u: f64) -> f64;
}
```

`PlotView::axes[i]` and `PlotBounds::min_y/max_y` hold display coordinates. A
log axis panned by a screen delta shifts its exponent (multiplicative pan) and
zoomed by a factor scales its decade span — both fall out of the existing
`offset_axis_y` / `zoom_axis_y` arithmetic with no new code, which is the
central reason for this representation.

The consumers that must now call `forward()` before projecting are a closed,
enumerable list:

| site | what is projected |
|------|-------------------|
| `mod.rs::paint_overlay` | alarm limit-line values, per-axis left-edge trace markers |
| `mod.rs::Render` (cursor canvas) | `(vs - b.min_y) / h` cursor endpoint markers |
| `mod.rs::Render` (hover canvas) | `(s.value - b.min_y) / h` hover sample markers |
| `cursor.rs::focused_trace_at` | trace value at the pointer's X |
| `xy_plot/mod.rs::paint_xy_underlay` | the two zero lines (suppressed on a log axis — zero is not on it) |
| `line_plot.rs::effective_view` | auto-fit min/max and the `Override` endpoints |

`format_value_label` is applied to `scale.inverse(tick)`, so labels read as
`1`, `10`, `1.0e2` rather than as exponents.

### Ticks

```rust
/// Tick positions in display space over `[min, max)`, with a major flag.
/// Log10: integer decades are major; 2..9 within a decade are minor and are
/// only emitted while the span is under ~2 decades. Above ~6 decades the
/// decade step widens (2, 3, …) so the count stays near `target_count`.
pub fn log_ticks(min: f64, max: f64, target_count: usize)
    -> impl Iterator<Item = (f64, bool)>;
```

Underlay gridlines paint minors at `theme.grid_color` with reduced alpha;
`paint_overlay` labels majors only. `value_ticks` is unchanged and still serves
every linear axis and both XY axes when linear.

### Auto-fit

`ValueBounds` carries `min_positive` alongside `min`/`max`. `scan_min_max`
tracks it with one extra compare in the inner loop; the scan already runs on
`cx.background_executor()`, so the cost is off the frame path. `effective_view`
then resolves a log axis's auto range as
`(forward(min_positive), forward(max))`, and `pad_auto_range` pads in display
space — a fixed fraction of the *decade* span, which is exactly what the
existing `AUTO_EDGE_PAD` arithmetic does once the inputs are logs. A user
`Override` below or at zero on a log axis is clamped to `min_positive` rather
than rejected.

### GPU: the transform is the last step, after decimation

```rust
// gpu.rs
#[derive(Clone, Copy)]
pub(crate) struct YNorm { epoch: f64, inv_span: f64, scale: AxisScale }

impl YNorm {
    #[inline(always)]
    fn apply(&self, v: f64) -> f32 {
        let u = (self.scale.forward(v) - self.epoch) * self.inv_span;
        if u.is_finite() { u as f32 } else { OFFSCREEN_NORM }
    }
}
/// One full view-height below the plot: a non-positive sample on a log axis
/// leaves the frame instead of being clamped onto the axis floor, so the line
/// visibly drops out rather than lying about a value it doesn't have.
const OFFSCREEN_NORM: f32 = -1.0;
```

`YNorm` replaces the `y_epoch: f64, y_scale: f64` parameter pair threaded
through `plan_trace`, `plan_min_max_trace`, `flush_fold`, `upload_pair`,
`materialize_axis`, `convert_element_strided`, `plan_list_trace`, and
`convert_latest_sample_strided` — a net parameter reduction, and it puts the
transform in exactly one function.

**Decimation correctness.** `log10` is strictly increasing on `(0, ∞)`, so for
any bin `argmin`/`argmax` are identical in data space and in log space.
Therefore:

- `select_minmax_indices` stays a pure data-space scan and selects the same
  sample indices on a log axis. It must **not** be given the scale.
- `plan_min_max_trace`'s stride fold (`min` of mins, `max` of maxs over
  `metor_db::lod` buckets) commutes with a monotone transform, so the LoD
  schema and its upstream bucket computation are unchanged.
- The only per-sample change is the write into `upload_y`, i.e. after every
  selection decision has been made. "Never hide a glitch" is preserved by
  construction, and is pinned by a test (below).

## Implementation steps

Each step ends with `cargo build -p metor-panel` green.

1. **`AxisScale` + tick generator.** Add `AxisScale`, `log_ticks`, and
   `ValueBounds` to `views/time_series/bounds.rs`; make `pad_auto_range` take
   an `AxisScale`. Unit-test `log_ticks` (decade anchoring, minor suppression
   past two decades, degenerate/zero-width ranges, no unbounded loop) beside
   the existing `value_ticks` tests. *Touches:* `bounds.rs`, `mod.rs`
   (re-export via `pub use bounds::*`).
2. **`min_positive` in the scanners.** Change `expand_value_bounds` and
   `expand_latest_sample_bounds` in `views/time_series/mod.rs` to return
   `ValueBounds`, extend `NodeBounds` and `scan_min_max`, and update the four
   call sites in `time_series/line_plot.rs` and `xy_plot/line_plot.rs`.
3. **`YNorm` in the GPU layer.** Add `YNorm` + `OFFSCREEN_NORM` to `gpu.rs`,
   add `y_scale: AxisScale` to `LineDraw`, replace the epoch/scale parameter
   pairs, and re-document `LineDraw::y_min`/`y_max` as display space. No
   behavior change yet — every caller passes `AxisScale::Linear`.
4. **Axis knob + auto-fit, time series.** `YAxis::scale`; `YAxisConfig::scale`
   in `views/time_series/config.rs`; `LinePlot::effective_view` fits in display
   space using `min_positive`; the `Render` canvas passes each trace's axis
   scale into its `LineDraw`.
5. **Time-series chrome.** `paint_underlay`/`paint_overlay` in
   `views/time_series/mod.rs` switch tick source per axis, label through
   `inverse`, forward limit-line values and axis markers, and suppress the zero
   line on a log axis. Update the cursor and hover marker normalization in
   `Render`, and `cursor::focused_trace_at`.
6. **XY and list.** `XyLinePlot`/`ListLinePlot` gain `x_scale`/`y_scale:
   AxisScale` and the matching `*PanelConfig` fields; `paint_xy_underlay` /
   `paint_xy_overlay` (both in `views/xy_plot/mod.rs`, shared by list) take the
   two scales. X-log rides the same mechanism: materialize
   `forward(raw) - forward(epoch_x)` and derive the view uniform from
   `forward(view.min_x)` — `ValueCache::epoch_x` becomes a display-space epoch.
   Timestamps are always linear.
7. **Decimation pin.** In `gpu.rs::tests`, alongside
   `minmax_selection_keeps_impulses`, assert that `select_minmax_indices`
   returns the identical index list for data whose values span several decades,
   and add a `YNorm` test that a `Log10` norm maps `min→0`, `max→1`, and a
   non-positive sample to `OFFSCREEN_NORM`.
8. **Layout version.** Bump `TILE_LAYOUT_VERSION` in
   `libs/metor-proto/wkt/src/tile.rs` and add a history line to the doc comment
   on `SUPPORTED_LAYOUT_VERSION` in `src/tiles/mod.rs`. Old layouts are
   rejected, not migrated (the crate's standing policy). Plans 02, 13, and 20
   also bump — take **one** shared bump per release, not one per plan.
9. **Docs.** Update the module docs on `bounds.rs` and `gpu.rs` to state the
   display-space invariant and the monotonicity argument.

## Open questions

- **Non-positive samples.** `OFFSCREEN_NORM` makes them visibly leave the
  frame. Should the plot also surface a count, next to the existing
  `"decimated"` badge in `LinePlot::render`? Cheap, and the alternative (silent
  disappearance) is exactly the failure mode this crate avoids elsewhere.
- **Auto-fit floor.** `min_positive` is honest but jumpy: one stray tiny sample
  stretches the axis by decades. A floor of `max / 10^k` (k ≈ 6, matplotlib's
  practical behavior) is steadier. Start with `min_positive`; revisit if it
  proves annoying in the field.
- **Per-trace vs. per-axis.** Scale is per `YAxis`, which is right — traces on
  one axis must share a mapping. Worth confirming nobody wants a "log this one
  trace" affordance instead of "put it on its own axis".
- **Minor-tick density.** Log minors are emitted only under ~2 decades. Whether
  2/5 (instead of 2..9) reads better at 2–4 decades is a taste call best made
  against real spectra.
- **Log X on time-series.** Deliberately excluded — a log time axis is
  meaningless against wall-clock. Only XY/list get X-log (step 6).
