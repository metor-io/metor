# 13 — XY plot upgrades

## Summary

Three additions to the XY plot: static **reference curves and envelopes** (the
XY analog of the time-series alarm limit line, and painted the same way — on
the CPU, in the chrome canvas), **color-by-third-channel** scatter, and a
**comet trail** that draws only the newest N samples with a recency fade. The
second and third are the same mechanism: one per-point `t ∈ [0,1]` storage
buffer plus a two-stop ramp in `LineUniform`, so one shader change buys both.

## Reuse vs. new

Extended:

- `views/xy_plot/line_plot.rs` — `XyLinePlot` gains `references:
  Vec<Entity<XyReference>>`; `XyTraceTracking`'s x/y field pairs collapse into
  a reusable `AxisTracking` so a third (color) channel doesn't triplicate them.
- `views/xy_plot/mod.rs` — `XyTrace` gains the trail and color-by fields;
  `paint_xy_underlay` grows a reference-curve pass. (Note: `paint_xy_underlay`
  and `paint_xy_overlay` are also called by `list_plot`; keep the new argument
  defaulted/empty there. Moving them to `plot_common` under axis-neutral names
  is step 2 of `plot-shell-unification.md` and is a prerequisite worth taking
  first if that plan is live.)
- `views/time_series/gpu.rs` — a third storage buffer and two new
  `LineUniform` fields; `LineDraw` gains `color_ramp: Option<ColorRamp>`.
  `line.wgsl` and `scatter.wgsl` each gain three lines.
- `views/xy_plot/trace_picker.rs` — the existing two-step component wizard is
  reused for both the third channel and component-sourced references.
- `views/plot_common.rs` — `plot_legend` is called a second time for the
  reference row; a new `color_scale_legend` renders the colorbar.
- `inspector/registry/defaults.rs` / `builders.rs` — one more
  `register_entity_list::<XyLinePlot, XyReference>` and its wizard.

New: `views/xy_plot/reference.rs` (`XyReference`, `ReferenceSource`,
`paint_references`), `ColorRamp`/`TSource` in `gpu.rs`, `AxisTracking`.

Depends on [09-log-scale-axes.md](09-log-scale-axes.md): reference curves and
the auto-fit fold must project through `AxisScale::forward` if log axes have
landed. Coordinate with
[11-value-density-heatmap.md](11-value-density-heatmap.md), which renames
`LineDraw` → `TraceDraw` and `LineUniform` → `TraceUniform`.

## Design

### Reference curves and envelopes are data, painted like limit lines

Alarm limit lines are values the plot is *told about* and paints as CPU chrome
in `paint_overlay`. A reference curve is the same thing with two coordinates,
so it gets the same treatment — a `PathBuilder` polyline, not a GPU trace.
Reference tables are tens to thousands of points, never millions, so the GPU
path buys nothing and costs a whole `AxisSource` variant.

```rust
// views/xy_plot/reference.rs
#[derive(Clone, facet::Facet)]
#[facet(pod)]
pub struct XyReference {
    pub label: SharedString,
    pub color: Hsla,
    pub visible: bool,
    /// Join the last point back to the first — a V-n diagram or a operating
    /// envelope is a closed region, a pump curve is not.
    pub closed: bool,
    /// Shade the interior. Only meaningful with `closed`.
    pub fill: bool,
    pub dashed: bool,
    #[facet(inspect::range(min = "0.5", max = "6.0"))]
    pub stroke_width: f32,
    #[facet(skip)]
    pub source: ReferenceSource,
}

pub enum ReferenceSource {
    /// Literal points, authored in the layout or shipped by a target preset.
    Table(Arc<[[f64; 2]]>),
    /// A component pair, resolved exactly like an `XyTrace`'s axes — a curve
    /// the control system publishes (a predicted profile, a calibration).
    Component {
        x_component_id: ComponentId,
        x_element_index: usize,
        y_component_id: ComponentId,
        y_element_index: usize,
    },
}
```

One shape covers all three prior-art cases: pump curve = open polyline,
V-n envelope = closed + filled, predicted-vs-actual overlay = open polyline in
a muted color. A band between two curves is authored as one closed polygon.

Resolution and paint:

- `XyLinePlot::references` gets its own entry in the tracking map. `Table`
  needs no tracker; `Component` reuses `wait_for_component` plus the existing
  `reconcile_trackers` bookkeeping, waking on `time_series.wait()` like a
  trace, and caching a decimated `SmallVec<[[f64; 2]; 64]>` (stride to
  `REFERENCE_MAX_POINTS = 2048`) so paint never re-reads the component.
- The underlay canvas's prepare closure snapshots the resolved polylines
  alongside `effective_view`; `paint_references` strokes them (dashes emitted
  as alternating screen-space segments — `PathBuilder` has no dash support) and
  fills closed ones at low alpha using the reference's own color, mirroring how
  `alarm_tint` derives a wash from `alarm_color`. No new theme fields.
- `XyLinePlot::effective_view` folds visible reference point bounds into the
  auto-fit min/max. An envelope the data sits inside must be on screen, or it
  isn't doing its job.
- References render in a second legend row (`plot_legend` called again over
  `references`), with a hollow swatch so they read as chrome rather than data.

### One per-point channel serves color-by-third and comet trail

Today every trace is one flat `LineUniform.color`. Add a `t` storage buffer and
a second color stop:

```wgsl
struct LineUniform {
    color: vec4<f32>,     // t = 0
    color_hi: vec4<f32>,  // t = 1
    line_width: f32,
    ramp: u32,            // 0 = flat `color`, 1 = mix(color, color_hi, t)
}
@group(2) @binding(2) var<storage, read> t_values: array<f32>;
```

A line segment takes its start point's `t`; a scatter point takes its own.
Comet trail is then just a ramp from `Hsla { a: 0.0, ..color }` to `color` —
the same code path as a data-driven colormap, no separate fade mode.

```rust
// gpu.rs
pub(crate) struct ColorRamp<'a> {
    pub hi: Hsla,
    pub t: TSource<'a>,
}
pub(crate) enum TSource<'a> {
    /// t = i / (n - 1) across the uploaded span. Costs no data reads.
    Recency,
    /// A third component element, normalized against `min..max`.
    Channel { component: &'a Component, element_index: usize, min: f64, max: f64 },
}
```

`upload_pair` materializes `t` into a `scratch_t` at the **same index set** it
uses for x and y, so `(x, y, t)` triples stay honest under min/max selection.
When `color_ramp` is `None` nothing is uploaded and the shader never reads the
buffer.

### Trace knobs

`XyTrace` gains:

```rust
/// `Auto` plots the whole history; `Custom(n)` plots only the newest `n`
/// paired samples and fades them by recency — the comet trail.
pub trail: Override<usize>,
/// Third channel driving color. Picked with the trace picker's component
/// wizard; `None` keeps the flat trace color.
#[facet(skip)] pub color_by: Option<(ComponentId, usize)>,
pub color_min: Override<f64>,
pub color_max: Override<f64>,
/// The `t = 1` end of the ramp. `Auto` picks a contrasting theme line color.
pub color_hi: Override<Hsla>,
```

Fade is implied by `trail` rather than being its own toggle — a truncated
window without a fade is just a shorter trace, and the fade is what makes the
recency legible. When both `trail` and `color_by` are set, `color_by` owns the
hue and `trail` only limits the window.

`trail` is also a real performance fix: `plan_trace`'s XY branch currently
collects **every** node slice of both components with no culling. With
`Custom(n)` it walks `iter_node_slices()` newest-first and stops once `n`
samples are covered, setting each `NodeView`'s visible window to that suffix.

A colorbar for `color_by` goes in the legend row — the right chrome is only
`PADDING` wide. `plot_common::color_scale_legend(label, lo, hi, min, max)`
renders ~24 interpolated swatch divs plus the two endpoint labels (a div strip
rather than a gradient API keeps it independent of gpui's gradient support).

## Implementation steps

Each step ends with `cargo build -p metor-panel` green.

1. **`AxisTracking`.** Collapse `XyTraceTracking`'s `x_*`/`y_*` field pairs in
   `views/xy_plot/line_plot.rs` into `struct AxisTracking { component,
   bounds, node_bounds, cached_element_index }` held as `x`, `y`. Pure
   refactor; the third channel in step 6 becomes `c: Option<AxisTracking>`
   rather than a third copy.
2. **`XyReference` type and store.** New `views/xy_plot/reference.rs` with
   `XyReference`, `ReferenceSource`, and the resolved-polyline cache; wire
   `references: Vec<Entity<XyReference>>` into `XyLinePlot` and its
   `reconcile`. Serialize as `XyReferenceConfig` in
   `views/xy_plot/config.rs`.
3. **Reference paint + fit.** `paint_references` in `reference.rs`, called from
   the underlay canvas in `views/xy_plot/mod.rs`; fold reference bounds into
   `effective_view`; second legend row.
4. **Reference inspector.** `register_entity_list::<XyLinePlot, XyReference>`
   in `inspector/registry/defaults.rs` with a wizard in `builders.rs`
   (`build_xy_reference_add_wizard`) reusing `xy_plot::trace_picker`'s two-step
   component pick. `Table` references are authored in the layout/preset in v1.
5. **GPU ramp plumbing.** Add the `t_buf` storage binding, `color_hi` + `ramp`
   to `LineUniform`, and the three-line change to `line.wgsl` and
   `scatter.wgsl`. Add `ColorRamp`/`TSource` to `LineDraw` and materialize `t`
   in `upload_pair`. `bars.wgsl` keeps the flat color. No caller sets a ramp
   yet.
6. **Comet trail.** `XyTrace::trail`; newest-suffix node walk in `plan_trace`'s
   XY branch; `TSource::Recency` ramp from transparent to the trace color.
7. **Color by third channel.** `XyTrace::color_by` + `color_min`/`color_max`/
   `color_hi`; resolve the third component in `XyLinePlot::spawn_tracker` via
   `AxisTracking`; scan its bounds with `expand_value_bounds`; pair a third
   `NodeView` in `plan_trace` and feed `TSource::Channel`. Add
   `plot_common::color_scale_legend` and render it when `color_by` is set.
8. **Log-axis interop.** If plan 9 has landed, project reference points and the
   reference bound fold through `AxisScale::forward`, and normalize the color
   channel in data space (a color ramp is not an axis and stays linear unless
   asked otherwise — see open questions).
9. **Tests and version.** Unit-test the reference decimation stride and the
   closed/open path point ordering; assert `upload_pair` emits `t` with exactly
   the same count as x/y for a strided min/max selection. Bump
   `TILE_LAYOUT_VERSION` (`libs/metor-proto/wkt/src/tile.rs`) and the history
   note in `src/tiles/mod.rs` — plans 02, 09, and 20 bump too, so take one
   shared bump per release rather than one per plan.

## Open questions

- **Third GPU buffer cost.** `t_buf` at `VALUE_BUF_BYTES` adds 16 MB of VRAM
  next to the existing x/y buffers, always allocated even for plots that never
  ramp. Alternatives: a smaller dedicated capacity for ramp traces (they are
  scatter plots, so the per-pixel budget is the real cap anyway), or `f16`.
  Sizing it at `VALUE_CAPACITY / 4` and refusing ramps past that is probably
  right.
- **Colormap stops.** Two stops cover the g-g diagram and the trail. A
  perceptual map (turbo/viridis) needs 4+ stops in the uniform — trivially
  affordable inside the 256-byte `UNIFORM_ALIGN` slot, but it is a real design
  choice about the crate's color budget (HPHMI: saturated color means
  abnormal). Deferred until someone asks.
- **Log color scale.** PSD-style third channels want a log ramp. The
  `AxisScale` from plan 9 would drop straight into the `Channel` normalization
  — worth adding only when a case appears.
- **Table reference authoring.** A paste-a-CSV path in the wizard would make
  spec-sheet envelopes self-service, but "typed data in the UI" is a new kind
  of state for this crate. Layout/preset authoring first.
- **References on the list plot.** A target profile overlay on the list plot
  (survey item 26) is the same `XyReference` with an index X. Free once the
  painters move to `plot_common`; not scoped here.
- **Trail on the time-series plot.** Not applicable — time already orders the
  samples. The trail is an XY-only idiom.
