# Map / ground-track view

Implements item 15 of `docs/plans/telemetry-viz-additions.md`.

## Summary

A ground track is a plot of longitude against latitude, so it is an XY plot with
a projection, a locked aspect, angular axis chrome, and a basemap behind it.
Rather than add a third plot panel, we grow `views/xy_plot` into a
projection-aware 2-D plot and add a "Ground Track" add-flow that seeds one; the
frame conversion (ECI/ECEF → lat/lon/alt) lands where every other data
transformation in the panel lands, as a dynamic-node op.

## Reuse vs. new

**Decision: extend `src/views/xy_plot`. No new `PaneItem`, no new `WidgetKind`,
no rename.**

*Why not a new map view.* The XY plot's data path is already exactly what a
ground track needs. `XyLinePlot::spawn_tracker`
(`src/views/xy_plot/line_plot.rs:233`) resolves two `(component, element)` pairs
and tracks their bounds; the `(Element, Element)` arm of `plan_trace`
(`src/views/time_series/gpu.rs:1017`) pairs the two components' node slices by
stack index and decimates them. When X and Y are two elements of *one* component
— the ground-track case, elements of an `[lat, lon, alt]` triple — both sides
walk the same node list and pair trivially. That path calls `iter_node_slices()`
with no time-range cull, so **the whole track history is already drawn**;
history backfill needs no new machinery. Pan/zoom, legend, decimation, GPU
readback, config round-trip, and inspector reflection all come free. A new view
would reimplement all of it to add a projection and different tick labels.

*Why not the 3D viewer.* `src/views/viewer_3d/bridge.rs` binds only the *latest*
sample to a Bevy transform; it has no time-series ingestion, no decimation, and
no 2-D chrome. A globe mode would mean building history rendering from scratch
plus shipping an Earth mesh and texture. It is the right home for a future 3-D
orbit view, and the geodetic op below serves that too — but it is the wrong home
for a 2-D ground track.

*Why no rename.* "XY plot" states the type's intent precisely: plot two channels
against each other in a 2-D data space. A map is a *configuration* of that (a
projection plus a basemap layer), not a different kind. Renaming would also
break `XyPlotPanel::serialization_key() == "xy_plot"`
(`src/tiles/panels.rs:946`) and `WidgetKind::new("xy_plot")`
(`src/views/dashboard/widgets.rs:258`) in every persisted layout and every
target-shipped preset, for no functional gain.

*Bonus.* The value-gradient coloring (phase 3) is the same feature as survey item
13's "color-by-third-channel scatter" (MoTeC g-g diagram). Building it on the XY
plot closes both items with one change.

## Coordinate handling

The only FSW target in the tree publishes ECI position, not GPS lat/lon:
`gps.pos_eci` and `body.pos_eci`, both `Vector<f64,3>` in metres, declared at
`examples/adcs-fsw2/contracts/src/lib.rs:210` and `:247`, published from
`examples/adcs-fsw2/systems/adcs-systems/src/plant.rs:444`. Wire names are
`block.plant.gps.pos_eci` etc. Nothing publishes lat/lon today. So **ECI → LLA
projection is the primary path**, and a bare lat/lon pair is the degenerate one.

The math already exists, split across two places:

- `nox_frames::earth::eci_to_ecef(Epoch) -> DCM<GCRF, ITRF>`
  (`libs/nox-frames/src/earth.rs:83`) — rigorous IERS/SOFA chain, not a naive
  sidereal rotation.
- `ecef_to_geodetic(&V3) -> (lat_rad, lon_rad, alt_m)`
  (`examples/adcs-fsw2/contracts/src/lib.rs:87`) — Bowring-seeded iteration over
  WGS84, currently a private helper of the example, feeding the WMM lookup. The
  example's own doc comment at `:85` says nox-frames is where it belongs.

`impl From<Timestamp> for hifitime::Epoch` exists at
`libs/metor-proto/src/types.rs:939`, so each sample carries the epoch the ECI
rotation needs.

The conversion belongs in a dynamic-node op, not in the view: it is a
transformation of a component into another component, it composes with the node
editor, and `persist` (`src/dynamic/ops/persist.rs`) gives the result on-disk
history and every existing view binding for free. The *map* projection
(equirectangular / Mercator) stays in the view, because it must match the
basemap and is a display choice.

## Design

### Phase 0 — geodetic source

- Move `ecef_to_geodetic` into `libs/nox-frames/src/earth.rs` (with `WGS84_A`,
  `WGS84_E2` and its `geodetic_recovers_equatorial_altitude` test); return a
  named `Geodetic { lat_rad, lon_rad, alt_m }` rather than a tuple. Rewrite the
  example's `mag_field_eci` to call it.
- New panel op `src/dynamic/ops/geodetic.rs`:
  `geodetic(input, frame) -> Result<Arc<dyn DynamicNode>, BuildError>` with
  `PositionFrame::{Eci, Ecef}`. Built on the `NodeImpl::spawn` pattern of
  `src/dynamic/ops/derive.rs:49`: requires a 3-element input schema, emits
  `ComponentSchema::new(PrimType::F64, [3])` holding `[lat_deg, lon_deg, alt_m]`.
  For `Eci`, rotate by `eci_to_ecef(Epoch::from(ts))` per sample first.
- Register in the node editor: `NodeSpec::Geodetic { frame }` and
  `NodeSpecKind::Geodetic` (`src/node_editor/spec.rs:26`), an `OpDescriptor`
  (`category: "Transform"`, `inputs: ONE_VALUE`, `output: VAL`, `arg_count: 1`)
  in `src/node_editor/registry.rs:94`, its arg row in
  `src/node_editor/inspector_rows.rs`, and the build arm in
  `src/node_editor/worker.rs`.
- Panel gains `nox-frames` (default `earth` feature). See open questions.

A `FromDb(gps.pos_eci) → Geodetic(Eci) → Persist("gps.lla")` graph then yields a
plottable component. Optionally publish `lat_lon_alt` directly from the example's
`gps` frame so the feature demos without a graph.

### Phase 1 — projection, aspect, graticule (the MVP)

New `src/views/xy_plot/projection.rs`:

```rust
pub enum Projection { Linear, Equirectangular, Mercator }
```

- `Linear` — today's behaviour exactly; the default, so every serialized config
  round-trips unchanged.
- `Equirectangular` — `x = lon_deg`, `y = lat_deg`. **Identity in data space**,
  so it needs no change to the GPU materializer at all. Everything map-like about
  it is chrome plus aspect.
- `Mercator` — `y' = ln(tan(45° + φ/2))` in degree-equivalent units. Nonlinear,
  so it needs a per-sample transform where `upload_pair`
  (`src/views/time_series/gpu.rs:1353`) already does `((v - epoch) * scale)`:
  a `y_transform: AxisTransform` on `LineDraw`, defaulting to `Identity`.

`Projection` lives on `XyLinePlot`, not on the trace — one plot, one coordinate
system.

**Aspect lock.** Any non-`Linear` projection locks the data-space aspect to the
plot area's pixel aspect, so a track is not stretched. `XyLinePlot` caches
`last_plot_size: Option<Size<Pixels>>` from its own canvas prepaint (its canvas
rect is exactly the plot area — `XyPlot::render` insets it by
`Y_LABEL_WIDTH + PADDING`), and `effective_view`
(`src/views/xy_plot/line_plot.rs:179`) applies a new
`PlotBounds::fit_aspect(size)` in `src/views/time_series/bounds.rs`. Single
source, so the wrapper's `view()` and the renderer agree.

**Antimeridian.** A LEO track crossing ±180° must not draw a segment across the
whole map. Add `x_period: Option<f64>` to `LineDraw`; in `upload_pair`'s X loop,
unwrap each sample against its predecessor by ±period. ~8 lines, and `None` at
the two existing call sites keeps their behaviour.

**Chrome.** New `paint_graticule_underlay` / `paint_graticule_overlay` in
`src/views/xy_plot/map.rs`, called from `XyPlot::render` in place of
`paint_xy_underlay` / `paint_xy_overlay` when the projection is not `Linear`.
Ticks come from a `graticule_step(span_deg)` snapping to 1/2/5/10/15/30/45°
(the angular analogue of `value_ticks`), labels formatted `45°N` / `120°W` via
`plot_common::paint_text_label`. Equator and prime meridian use
`theme.zero_line_color`; the rest use `theme.grid_color`.

**Wizard.** `src/views/xy_plot/trace_picker.rs` currently walks X component → X
element → Y component → Y element. Add `select_ground_track_rows`: pick a
component, and if it has ≥ 2 elements offer "Longitude/Latitude pair" (defaulting
`x_element_index = 1`, `y_element_index = 0` for an `[lat, lon, alt]` triple)
against the manual four-step path. `Projection::Equirectangular` and
`PlotStyle::Line` are the seeded defaults.

**Registration.** Both surfaces already exist for `xy_plot`; the ground track is
a second *add-flow* over the same config, per the "extend the standard path"
convention:

- a `NavRow::new("Ground Track", …)` beside the "XY Plot" row in
  `src/tiles/panels.rs:1160`, building an `XyPlotPanelConfig` with
  `projection: Equirectangular` and passing `"xy_plot"` to
  `add_registered_panel`;
- the matching dashboard entry via `WidgetSpec::with_add_flow` on the existing
  `WidgetKind::new("xy_plot")` spec (`src/views/dashboard/widgets.rs:258`).

### Phase 2 — vector basemap (offline by construction)

Ship a Natural Earth 110m coastline, pre-baked to packed `f32` lon/lat pairs with
polyline break markers, as `assets/coastline_110m.bin` (~150 KB), loaded with
`include_bytes!` the way `src/icons.rs` loads SVGs — no new dependency, no
network, no cache directory. Painted in the underlay canvas as gpui
`PathBuilder` strokes, view-culled per polyline and rebuilt only when
`view.bits()` changes (the view is static between interactions, so this is not a
per-frame tessellation cost). Colours are new `Theme` fields: `map_coastline`,
`map_land`, `map_ocean`.

If tessellation proves too slow at full zoom-out, the escape hatch is a
`AxisSource::Static { values: &[f32] }` arm in `gpu.rs` that pushes the basemap
through the same GPU pass as the traces — which also unlocks survey item 13's
static reference/envelope curves.

### Phase 3 — value-gradient coloring

The automotive idiom: colour the track by a third channel. The pipeline is
uniform-colour today (`LineUniform.color`, `src/views/time_series/gpu.rs:180`),
so this needs a real but contained GPU change:

- a third storage buffer `c_buf` at `@group(2) @binding(2)`, added to
  `storage_layout` / `storage_bg`;
- `LineDraw` gains `c: Option<AxisSource<'a>>` plus `c_min` / `c_max`;
  `upload_pair` fills a `scratch_c` alongside `scratch_x` / `scratch_y` using the
  same selected indices, normalized to `[0,1]`;
- `LineUniform` gains `ramp: [vec4<f32>; 4]` and `color_mode: u32`;
  `line.wgsl` and `scatter.wgsl` mix the ramp by `c_values[idx]`. `out.color` is
  already a varying, so mixing `c_a`/`c_b` by `corner.y` gives a gradient along
  each segment for free.

`XyTrace` gains `color_component_id: Option<ComponentId>`, `color_element_index`,
and `color_mode: ColorMode { Solid, Channel }`; `XyTraceTracking`
(`src/views/xy_plot/line_plot.rs:29`) gains `c_component` / `c_bounds` /
`c_node_bounds`, mirroring its existing x/y fields exactly. The ramp is a
`Theme::value_ramp() -> [Hsla; 4]` in `src/theme.rs` — never hardcoded. A
colorbar strip replaces the legend when a trace is channel-coloured.

### Phase 4 — raster tiles

Deferred. See open questions.

## Implementation steps

1. `libs/nox-frames/src/earth.rs`: add `Geodetic` + `ecef_to_geodetic` (+ its
   test); rewrite `examples/adcs-fsw2/contracts/src/lib.rs:87-122` to use it.
2. `src/dynamic/ops/geodetic.rs` + `op_tag::GEODETIC` in `src/dynamic/node.rs` +
   re-export from `src/dynamic/ops/mod.rs`; unit test against a known
   ECEF/geodetic pair in `src/dynamic/tests.rs`.
3. Node-editor wiring: `spec.rs`, `registry.rs`, `inspector_rows.rs`,
   `worker.rs`; extend the round-trip test in `src/node_editor/tests.rs`.
4. `src/views/xy_plot/projection.rs`: `Projection`, `graticule_step`,
   `Projection::forward`.
5. `PlotBounds::fit_aspect` in `src/views/time_series/bounds.rs`; cache
   `last_plot_size` on `XyLinePlot` and apply it in `effective_view`.
6. `LineDraw::x_period` + `y_transform` in `src/views/time_series/gpu.rs`;
   unwrap and transform in `upload_pair`; `..Default` the two other call sites.
7. `src/views/xy_plot/map.rs`: graticule underlay/overlay; branch on projection
   in `XyPlot::render` (`src/views/xy_plot/mod.rs:244`, `:268`).
8. `projection` field on `XyLinePlot` (`#[facet(inspect::variants = …)]`) and
   `XyPlotPanelConfig` (`serde(default)` keeps old layouts loading); no
   `SUPPORTED_LAYOUT_VERSION` bump needed.
9. `select_ground_track_rows` in `src/views/xy_plot/trace_picker.rs`; add-flow
   rows in `src/tiles/panels.rs` and `src/views/dashboard/widgets.rs`.
10. Serde round-trip test beside the existing one at `src/tiles/panels.rs:1669`.
11. Phase 2: bake `assets/coastline_110m.bin`, add `src/views/xy_plot/basemap.rs`
    and the three `Theme` fields.
12. Phase 3: the GPU colour-channel change, `XyTrace` fields, tracker third axis,
    `Theme::value_ramp`, colorbar.

## Open questions

- **Raster tiles.** No HTTP client exists anywhere in the panel's dependency tree
  (no `reqwest`, no `ureq` in `Cargo.lock`), and no gpui-native slippy-map crate
  exists — `walkers` is egui-only. Tiles therefore mean a new networking
  dependency, a disk cache, an attribution obligation, and a story for the
  air-gapped mission-ops case this tool is aimed at. Options: (a) never, vector
  basemap only; (b) a local MBTiles/directory path in config, no network; (c) a
  full tile client behind a feature flag. Recommend (b) if anything — it needs
  only `rusqlite` or a directory walk and stays offline-first. **Unresolved.**
- **`rsofa` build cost.** Pulling `nox-frames` into the panel adds `rsofa`, a
  `bindgen`/`cc` binding to the IAU SOFA C library, to a crate that is currently
  pure Rust plus wgpu. Measure the clean-build delta first. If it is
  unacceptable, the fallback is an ERA-only rotation in `nox-frames` behind a
  lighter feature — accurate to well under a pixel at ground-track scale — with
  the SOFA chain kept for the FSW side.
- **Should the ground-track wizard build the derivation itself?** It could append
  `FromDb → Geodetic → Persist` to the node graph and bind the trace to the
  result, holding its alive-set through `GraphCoordinator::submit` with the plot
  as `OwnerId` (`src/node_editor/coordinator.rs:36`). That seam exists and is the
  right one, but it makes a plot an owner of graph nodes. Phase 1 ships without
  it; revisit once the op is in use.
- **Track fade / current-position marker.** MoTeC and Foxglove both fade older
  track and mark "now". The fade wants per-sample alpha, i.e. the phase-3 colour
  buffer with age as the channel — so it is nearly free after phase 3, and
  awkward before it. Defer.
- **Multi-revolution tracks.** A long LEO history overlays dozens of revolutions
  into an unreadable band. The general answer is survey item 21 (episode
  overlay); the cheap one is honouring the plot's `TimeRangeBehavior` — and
  `LineDraw::x_clip` already exists for it. Neither is in scope here.
