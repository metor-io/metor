# 11 — Value-density heatmap / persistence display

## Summary

Show *how often* a channel visited each value, not just its mean line: a
time-bucketed 2D histogram over the visible range, rendered as intensity
(Grafana heatmap, oscilloscope digital-phosphor persistence). This lands as a
new **render mode for an existing time-series trace**, not a new pane — the
plot already owns the visible time range, the LoD ladder, the alarm limit
lines and the cursors that make a density readable. The one genuinely new
artifact is a reusable *intensity-field → colormap* pass, sized so the
deferred spectrogram (#5) can fill the same field from a CPU-built grid.

## Reuse vs. new

**Repurpose the time-series plot.** Every alternative loses something the plot
already has:

| Option | Verdict |
|---|---|
| New `HeatmapPanel` view | Rejected. Would re-implement the time axis, `TimeRangeBehavior`, LoD selection (`line_plot.rs:227` `update_lod_state`), gap bands, event flags, measurement cursors and alarm limit lines. A density plot *is* a time-series plot. |
| `PlotStyle::Density` on a trace | **Chosen.** One enum variant, one GPU pipeline, one trace-config block. Overlaying a density trace and a mean line in the same axes — the whole point of "the mean line lies" — becomes free. |
| Node op producing bucket vectors | Rejected as the primary path; see *Where bucketing happens*. |

**Renames this forces** (the naming no longer fits the broadened purpose):

- `LineDraw` → `TraceDraw` and `LineUniform` → `TraceUniform`
  (`src/views/time_series/gpu.rs:104,183`). Both already carry scatter and bar
  traces; "line" becomes plainly wrong once a trace can be a density field.
  `line.wgsl`, `line_pipeline` and `line_bg` keep their names — those really
  are the line shader and its bind group, except `line_buf`/`line_bg`, which
  are shared across all styles and become `trace_uniform_buf`/`trace_bg`.
- `LinePlot` (`src/views/time_series/line_plot.rs`) is the same kind of stale
  name, but renaming it churns the inspector downcasts in
  `src/inspector/registry/builders.rs:203,234,270` and `defaults.rs:375,478,587`.
  Leave it to `docs/plans/plot-shell-unification.md`, which already reshapes
  that layer.

## Design

### Where bucketing happens: in the view, on the GPU

Views in this crate are not uniformly latest-sample-only. Instruments and the
list plot are, but the time-series plot walks the DB's `TimeSeriesNodeSlice`
history inside `plan_trace` (`gpu.rs:922`) and picks a min/max LoD companion
when the raw range is over budget. Density belongs on that side of the line:

- **A node op would fix the buckets at graph-edit time.** Bin count, value
  range and time-bucket width would be baked into the `NodeId` hash, so
  zooming could not re-bucket. MoTeC's distinctive behaviour — the histogram
  recomputes as you zoom the plot — is exactly what a content-hashed node
  cannot do.
- **A node op cannot see the past.** It starts producing when it is built;
  the plot's whole visible range is already on disk.
- **A node op would need `persist` to be plottable at all**, since only DB
  components have a time series to scroll back through.

So: bucketing is view-side, driven by the visible `PlotBounds`, and — because
the CPU already has to touch each sample once to materialize it
(`materialize_axis`, `gpu.rs:1550`) — the accumulation itself goes on the GPU.

### Accumulation: additive scatter, no new geometry

`scatter.wgsl` already draws one soft dot per `(x, y)` pair out of the shared
storage buffers. A density pass is that same shader with additive blending:

```
BlendComponent { src_factor: One, dst_factor: One, operation: Add }
```

Add a `density_pipeline` to `PlotGpu` (`gpu.rs:321`) built from the *existing*
`scatter_shader` module with that blend state and a 1–2 px radius. No new WGSL
for accumulation. Every visible sample lands as one instance, so the buckets
are the framebuffer's own pixels — a true 2D histogram at display resolution,
and by construction it cannot hide a rare glitch.

Two changes to the plan step feeding it:

- **Stride selection.** `plan_trace` decimates to `pixel_budget`
  (`node_stride`, `gpu.rs:1612`). A density trace must not decimate to one
  sample per column; it gets its own `DENSITY_SAMPLE_BUDGET` (start at
  `1 << 18`) fed into the same `quantize_stride` (`gpu.rs:124`), so the
  power-of-4 stability property carries over.
- **Over-budget fallback.** Past that budget, fall back to the LoD min/max
  companion the trace already resolves (`AxisSource::MinMax`, `gpu.rs:74`) and
  paint each bucket as a dim vertical span — the envelope, at reduced
  intensity. Same correctness standard as today's zoomed-out line.

### Colormap: the reusable half

Accumulating into the current `Bgra8Unorm` MSAA target saturates almost
immediately, so density renders into its own single-sampled `R32Float`
*intensity field*, followed by a fullscreen pass that maps intensity through a
colormap into the normal target. That second pass is the piece #5 reuses: a
spectrogram fills the same field with `queue.write_texture` from a CPU-built
`[time × bin]` grid and runs the identical tonemap.

New file `src/views/time_series/heatmap.rs` (+ `heatmap.wgsl`) owning:

- `IntensityField` — an `R32Float` texture sized to the plot target, cleared
  per frame, with `fill_from_grid(&[f32], cols, rows)` for the #5 path.
- `Colormap` — `{ TraceColor, Heat, Mono }`; `TraceColor` ramps alpha over the
  trace's own colour, the others read stops from the theme.
- `tonemap(field, colormap, gain, scale)` — the fullscreen pass, with
  `IntensityScale::{Linear, Sqrt, Log}` so rare events stay visible.

Theme additions (`src/theme.rs`, alongside `plot_envelope_alpha:107`): a
`density_stops: [Hsla; 5]` table per theme plus
`Theme::density_color(t: f32) -> Hsla` interpolating them. No `Hsla` literal
leaves `theme.rs`; the LUT is built from the theme at pipeline setup and
rebuilt on theme change.

### Trace configuration

`PlotStyle` (`src/views/time_series/mod.rs:1035`) gains `Density`;
`PlotStyle::ALL` (`:1043`), `label()` and `parse()` grow with it. Three fields
join `Trace` (`mod.rs:1085`) and `TraceConfig` (`config.rs:85`), all
`#[facet(default)]` so old layouts load unchanged:

- `density_gain: f32` — hits-to-saturation, `#[facet(inspect::range(min = "0.1", max = "100.0"))]`
- `density_scale: IntensityScale`
- `density_colormap: Colormap`

`ListTrace.style` (`src/views/list_plot/mod.rs:47`) has no history to bucket,
so it gets `#[facet(inspect::variants = "Line, Scatter, Bar")]` — the
attribute at `src/inspect.rs:21` exists for exactly this. `XyTrace`
(`src/views/xy_plot/mod.rs:57`) *does* walk history through the same storage
buffers, so density works there unchanged and gives #13's g-g density for
free; enable it in the same step.

No `SUPPORTED_LAYOUT_VERSION` bump (`src/tiles/mod.rs:62`) — every new field is
additive with a facet default, and no persisted field changes shape or name.

### Registration

None. Density is a trace style on views that are already registered on both
surfaces (`PlotPanel` in `src/tiles/panels.rs`, `WidgetKind::plot()` in
`src/views/dashboard/widgets.rs:225`). This is the whole argument for the
repurpose: zero new `PaneItem`, zero new `WidgetKind`, zero new
`serialization_key`.

## Implementation steps

1. **Rename pass.** `LineDraw` → `TraceDraw`, `LineUniform` → `TraceUniform`,
   `line_buf`/`line_bg` → `trace_uniform_buf`/`trace_bg` in
   `src/views/time_series/gpu.rs`, and their uses in
   `time_series/line_plot.rs:758`, `xy_plot/line_plot.rs`,
   `list_plot/line_plot.rs:283`. Mechanical, no behaviour change; land it alone
   so the real diff stays readable.
2. **`PlotStyle::Density` + config.** Variant, `ALL`, `label`, `parse`; the
   three trace fields on `Trace`/`TraceConfig` and `XyTrace`/`XyTraceConfig`;
   the `inspect::variants` restriction on `ListTrace`. Extend
   `panel_configs_round_trip_through_json` (`src/tiles/panels.rs:1563`)
   with a density trace. At this point the style renders as `Line` — that is
   fine and keeps the step compiling.
3. **`heatmap.rs` + theme.** `IntensityField`, `Colormap`, `IntensityScale`,
   `tonemap`, `heatmap.wgsl`; `density_stops` and `Theme::density_color` across
   all theme tables in `src/theme.rs`. Unit-test the colormap interpolation at
   `t = 0, 0.5, 1` and that every theme's stops are monotonic in lightness.
4. **`density_pipeline`.** Add to `PlotGpu::try_new` (`gpu.rs:342`) from the
   existing `scatter_shader` with additive blending, targeting the intensity
   field. Route it in `PlotGpu::submit` (`gpu.rs:536`): density traces render
   into the field and tonemap into the main target *before* the line/scatter/bar
   pass, so ordinary traces overlay the density.
5. **Density planning.** In `plan_trace` (`gpu.rs:922`) branch on
   `PlotStyle::Density` before the standard cull: use `DENSITY_SAMPLE_BUDGET`
   in place of `pixel_budget`, and route to the LoD `MinMax` span path
   (`plan_min_max_trace`, `gpu.rs:1143`) once the visible sample estimate
   exceeds it.
6. **Inspector rows.** The three new trace fields render from facet reflection
   already; verify the `Colormap`/`IntensityScale` enums pick up `EnumRow` via
   `src/inspector/registry/dispatch.rs:121` and that the range attribute lands
   on `density_gain`.
7. **Manual verification.** `cargo run -p metor-panel` against a synthetic
   bimodal channel (a `Waveform` plus a `Random` node through `persist`):
   the mean line sits in a valley the density shows as empty. Zoom in and out
   and confirm the buckets re-derive and the LoD handoff does not blink.

## Open questions

- **Gain autoscaling.** A fixed `density_gain` makes a sparse zoom-in look
  black and a zoomed-out view look saturated. Options: normalize by the
  field's max (needs a readback or a reduction pass, one frame of latency), or
  normalize analytically by `samples_visible / columns`. The analytic estimate
  is free and probably good enough — confirm on real data before adding a
  reduction pass.
- **Should the intensity field be shared or per-plot?** `PlotRenderState`
  (`gpu.rs:718`) already owns a per-caller `RenderTarget`; the field is the
  same size and the same lifetime, so it likely belongs there rather than on
  the process-wide `PlotGpu`. Decide when wiring step 4.
- **Y bucketing under a log axis.** Log Y (#9) is not built yet; the density
  pass buckets in screen space, so it inherits whatever mapping the axis uses
  and should need no extra work. Worth re-checking when #9 lands.
- **P95 overlay.** The natural companion — a percentile line over the density
  — comes from `Window(N) → Percentile(p)` in
  `docs/plans/viz/19-node-op-gaps.md`, plotted as an ordinary second trace.
  No work here; noted so the two plans stay aware of each other.
- **Gated density.** MoTeC only accumulates while a condition holds. That is
  the `Condition` + `Gate` pair from #19 applied upstream of a `persist`, not
  a plot feature — unless we later want the gate evaluated per-sample at
  render time, which would need the condition as a second `AxisSource`.
- **Spectrogram hand-off.** `IntensityField::fill_from_grid` is designed for
  #5 but has no caller until #5 exists. Write it with the spectrogram in mind
  and cover it with a unit test rather than leaving it dead — or defer the
  method entirely and add it with #5 if the tonemap seam alone is enough.
