# 12 — Radar / spider chart

*From the prior-art survey (`docs/plans/telemetry-viz-additions.md`), item 12: N channels
as spokes, each normalized so "normal" is the same radius; deformation = abnormality
(Ignition Radar Chart, HPHMI Level‑1 overviews). Normalize to the alarm-store normal band,
not engineering range.*

## Summary

A new `RadarChart` view binds N independent scalar elements ("spokes"), each through the
same `views/binding.rs` machinery `Meter`/`Gauge` already use, and normalizes each spoke so
any value inside its declared alarm-store normal band draws at the *same* radius — the
polygon is a regular N-gon exactly when everything is nominal, and only bulges outward on
spokes that are out of band. No spoke takes an engineering min/max as config; the normal
band comes entirely from `AlarmLimit`s, so there is nothing to mis-tune.

## Reuse vs. new

**New view, no new plumbing pattern.** Candidates rejected on shape, not laziness:

- **`TrafficLightGrid`** (`src/views/traffic_light_grid.rs`) is the closest architectural
  cousin — `Vec<GridCell>` reconciled against live components, each cell owning its own
  stream task via `spawn_on_stream` — but it's an on/off `flex_wrap` grid, not polar
  geometry. Welding a polygon-radius mode onto it would mix two rendering models in one
  type, against `STYLE.md`'s "logically distinct subsystems get their own file."
- **`ListPlot`** (`src/views/list_plot/`) plots one component's vector as `index → value`
  over the GPU line-plot pipeline (`PlotBounds`/`line_plot.rs`). Making it polar means
  rewriting that pipeline's coordinate mapping — a second renderer wearing the same name,
  not a config knob.
- **`Gauge`** (`src/views/gauge.rs`) is the real reuse candidate, but only for its *math*.
  `Dial::fit`/`Dial::at`/`paint_arc`/`paint_spoke` are single-value, single-sweep helpers; a
  radar chart needs N fixed-angle spokes with independent radii. `RadarChart` writes its own
  three-line polar-point helper rather than generalizing `Dial` or renaming `Gauge` —
  the duplication is one trivial function, not the repeated logic `STYLE.md` asks us to
  extract.

**No new binding.rs pattern for streaming.** Per-spoke binding is N copies of what `Gauge`
already does (`spawn_scalar_stream` + `spawn_meta_resolver` + `binding::rebound`), and
add/remove reconciliation is `TrafficLightGrid`'s `Vec<Cell>` shape, just user-curated
instead of glob-matched. The one real gap: `binding::limit_marks` discards
`AlarmLimit::kind` (`Upper`/`Lower`), which normalization needs — addressed below.

## Design

### Config

```rust
// src/views/radar_chart.rs
#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct RadarChartConfig {
    pub label: Option<String>,
    pub spokes: Vec<RadarSpokeConfig>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
pub struct RadarSpokeConfig {
    pub component: String,
    pub element: usize,
    pub label: Option<String>,
}
```

Deliberately **no `min`/`max`, no per-spoke color**. Meter/Gauge take an engineering scale
as config because "how full is the tank" needs a scale before any alarm exists. A radar
spoke's whole scale *is* the alarm-store normal band, so there's nothing to configure and
nothing to mis-tune. A spoke with no declared limits draws flat and gray forever (see
Normalization) rather than inventing a fake engineering-range fallback — strictly better
than `Meter`'s `0.0..1.0` default, not a corner cut. Spoke order is render order (clockwise
from 12 o'clock, `Dial::angle_at`'s convention).

### Per-spoke binding

`RadarChart` owns `Vec<Spoke>`, each `Spoke { component_id, element, label, value:
Option<f64>, bound: Option<binding::ElementRef>, _task, _resolver_task }` — `GridCell`
(`traffic_light_grid.rs:30`) with a scalar `value` instead of `latest_on`. Built exactly like
`Gauge::from_config`/`Gauge::rebind` (`gauge.rs:122-217`): one `spawn_scalar_stream` per
spoke for the value, one `spawn_meta_resolver` per spoke for late registration, and
`binding::rebound(want, &mut spoke.bound)` checked per spoke each frame. Editing the spoke
list mutates `self.spokes` and spawns/drops tasks the same way `reconcile_cells` does,
keyed off the user-edited list rather than a regex match.

### Normalization — the heart of the design

Add to `binding.rs` (it currently throws away `kind`, which the linear meter/gauge scale
never needed but a radar band does):

```rust
/// Declared `Lower`/`Upper` bounds of `at`'s normal band: the tightest
/// (nearest-to-nominal) limit on each side, paired with the farthest
/// declared limit on that side (the reference for how far a spoke bulges
/// once abnormal). `None` on a side with nothing declared.
pub(crate) struct NormalBand {
    pub lower: Option<(f64, f64)>, // (nearest, farthest)
    pub upper: Option<(f64, f64)>,
}
pub(crate) fn normal_band(at: ElementRef, cx: &App) -> NormalBand;
```

Reads the same `store.state().limits_for(...)` as `limit_marks`, splits by `LimitKind`
(`libs/metor-proto/wkt/src/msgs.rs:591`). `nearest` = the limit value closest to nominal
(max of `Lower` values / min of `Upper` values — the first one crossed); `farthest` = the
most extreme declared limit on that side. A side with only one limit reuses `Meter`'s
`SCALE_HEADROOM` idea (`meter.rs:308`, 1.25×) past `nearest` as a synthetic `farthest`.

Radius mapping, per spoke per frame:

```rust
const RADAR_NORMAL_R: f32 = 1.0;
const RADAR_MAX_R: f32 = 1.6; // hard clamp, mirrors Gauge/Meter headroom clamps

fn spoke_radius(value: f64, band: NormalBand) -> f32 {
    // inside the declared [nearest_lower, nearest_upper] => RADAR_NORMAL_R (flat)
    // beyond a declared side => lerp RADAR_NORMAL_R..RADAR_MAX_R toward `farthest`, clamped
    // no limits declared on either side => RADAR_NORMAL_R, always
}
```

The normal region is **flat, not a gradient** — any value inside the band draws at exactly
`RADAR_NORMAL_R`, so nominal is a *perfect* regular polygon and an operator pattern-matches
"is this a regular N-gon" in one glance instead of reading N gauges. Deformation appears
only once a spoke leaves its band, and always bulges *outward* regardless of which side was
crossed — a low-side and high-side overshoot of equal severity deform identically, which is
what makes the shape read as one fault signature (mirrors Ignition's radar HMI widget). A
spoke with no declared limits stays pinned at `RADAR_NORMAL_R` and renders desaturated/
dashed, so "unmonitored" is never mistaken for "monitored and nominal."

### Rendering

One `canvas` per `Gauge`'s pattern (`gauge.rs:382-431`, `PathBuilder::stroke` for straight
segments — no arc sampling needed, spokes are lines):

1. Fit center + max radius to tile bounds.
2. N radial axis lines, center to `RADAR_MAX_R`, in `theme.border_primary` — always drawn,
   so the chart reads as a coordinate system even with no data.
3. One reference ring at `RADAR_NORMAL_R`, in `theme.grid_color` (`theme.rs:98`), as a
   regular N-gon — "what nominal looks like," visible even for undeclared spokes.
4. The live data polygon: one closed `PathBuilder::stroke` through `spoke_radius(value_i)`,
   in a single neutral `theme.control_active` — not per-severity, since per-vertex color
   already carries that and a multi-color outline needs per-segment paths for no extra
   information.
5. One filled dot per vertex (`gpui::fill` + `Corners::all`, like `Gauge`'s hub dot at
   `gauge.rs:421-429`), colored by `binding::alarm_tint(at, cx)` when `Some`, else
   `theme.text_tertiary`. This is where "saturated color only for abnormal" actually lives.
6. Spoke labels as absolutely-positioned `div`s at each axis's outer end, computed from the
   same center/radius/angle as the canvas — the canvas-geometry-plus-overlay-`div`s split
   `Gauge` already uses for its readout (`gauge.rs:440-475`), just N labels around a ring.

### Color budget

Axes, reference ring, and polygon outline are entirely neutral theme colors
(`border_primary`, `grid_color`, `control_active`). The **only** saturated color is a
per-vertex dot, and only when that element has an active alarm — a direct application of
the survey's "gray-scale ground, saturated color only for abnormal" principle; this widget
is the one case in the batch where that rule *is* the design, not an incidental choice.

## Implementation steps

1. `src/views/binding.rs` — add `NormalBand`/`normal_band` above. Pure function, unit-test
   directly (same style as existing `any_on`/`rebound` tests).
2. `src/views/radar_chart.rs` (new) — `RadarChartConfig`, `RadarSpokeConfig`, `Spoke`,
   `RadarChart` (`from_config`/`to_config`/`add_spoke`/`remove_spoke`/`Render`), and
   `spoke_radius` (pure, unit-tested: flat inside band, clamps at `RADAR_MAX_R`, monotonic
   outward both directions — same test style as `meter.rs`'s `fill_span`). Export from
   `src/views/mod.rs` alongside the `gauge`/`meter` lines.
3. `src/tiles/panels.rs` — `RadarChartPanel: PaneItem` wrapping `Entity<RadarChart>`,
   modeled on `TrafficLightGridPanel` (`panels.rs:583-642`): `new`, `from_config`, `Render`,
   `serialization_key() -> "radar_chart"`. Register via `register_panel::<RadarChartPanel>`
   in `register_pane_item_deserializers` (`app.rs:1292-1307`).
4. `src/views/dashboard/widgets.rs` — `WidgetKind::radar_chart()` (beside
   `traffic_light_grid()`, `dashboard/mod.rs:94`), `build_radar_chart`,
   `snapshot_radar_chart` (or `snapshot_typed::<RadarChart>` per `widgets.rs:577-582`), and
   a `self.register(...)` block modeled on `traffic_light_grid`'s (`widgets.rs:355-372`).
5. Creation flow — add `radar_chart_wizard_rows` beside `instrument_widget_rows`
   (`dashboard/mod.rs:1084-1103`) and `instrument_wizard_rows` (`panels.rs:1526+`). Both
   reuse `trace_picker::select_traces_wizard_rows` (the existing multi-select scalar-element
   wizard behind Meter/Gauge/StateChip), but fold the selected `Vec<Trace>` into **one**
   `RadarChartConfig` (via `component_meta` for names) and call `add_widget`/
   `add_registered_panel` once — unlike `instrument_widget_rows`, which builds one widget
   per selected trace. This "many channels, one widget" fold is the one place radar's
   creation flow diverges from Meter/Gauge's reuse of the same wizard. Add "Radar Chart"
   `NavRow`s next to "Meter"/"Gauge"/"Traffic Light Grid" in both menus.
6. Tests — `spoke_radius` and `binding::normal_band` unit tests, module-local `#[cfg(test)]`
   per existing convention. No new integration-test infra.

## Open questions

- **Per-spoke color override.** Meter/Gauge allow an operator color override; a radar
  spoke's color is fully derived here (neutral polygon + alarm-tint dot). Worth a per-spoke
  label accent, or does that reopen the "decorative alarm palette" trap? Leaning no for v1.
- **Minimum spoke count.** A polygon needs ≥3 vertices to read as a shape. Refuse to render
  below 3 (placeholder, like `build_meter`'s empty-component case) or just draw what's
  there? Leaning placeholder.
- **Many spokes / label collision.** HPHMI overviews are typically 6–12 channels; beyond
  that, outer labels overlap. Out of scope for v1 — revisit with truncation or hover
  tooltips (`tooltip::TooltipText`, already used by `TrafficLightGrid`'s cells) if needed.
- **Reordering spokes.** Add/remove falls out of the `Vec<Spoke>` reconcile; drag-to-reorder
  is a nice-to-have, deferred.
