# State timeline lanes

## Summary

Add horizontal lanes of colored, labeled segments — one lane per enum/mode/bool
channel — to the existing time-series plot, drawn in a band at the bottom of the
plot area against the plot's own X axis. A lane's value→state mapping is the
state chip's `StateEntryConfig` table, or is derived with no configuration at
all from the alarm store's limits (the threshold bridge). A "State Timeline"
palette entry is a plot preset with lanes and no traces; a "Status Grid" preset
is the same lanes in fixed-bucket cell mode.

## Reuse vs. new

**Decision: a mode of the existing time-series plot, not a new view.**

A lane needs everything `TimeSeriesPlot` already owns and nothing it doesn't:
`TimeRangeBehavior` + `GlobalTimeRange` resolution
(`time_series/line_plot.rs:216`), pan/zoom through `PlotView::offset_x`/`zoom_x`,
the X tick/label chrome (`x_ticks`, `format_time_label`), the event-flag gutter
(`event_flags.rs`), measurement cursors (Δt between two cursors *is* "how long
was it in SAFE"), the hover readout, the hydrator gap bands
(`LinePlot::gap_bands`), and one `PlotPanelConfig` round trip. A sibling view in
`views/state_timeline/` would re-copy roughly 700 lines of that shell — exactly
the duplication `docs/plans/plot-shell-unification.md` exists to remove — and
would still not deliver the highest-value form of the widget: mode lanes
*underneath* the analog traces on one shared time axis (LabVIEW mixed-signal,
Saleae, MoTeC). A standalone timeline is then the degenerate case: a plot with
lanes and zero traces.

**Not a `PlotStyle` variant.** `PlotStyle::{Line,Scatter,Bar}` selects how a
trace maps to the Y axis; a lane has no Y extent and no GPU draw. Lanes are a
separate list on the plot model, exactly parallel to `event_overlays`, which is
already the precedent for "state lives on `LinePlot`, painting lives in
`TimeSeriesPlot`'s canvas stack".

**Reused, not reinvented:**

| Existing | Reused for |
|---|---|
| `views/state_chip.rs` `StateEntry` / `StateEntryConfig` / `match_state` | the coded value→label/color table |
| `views/binding.rs` `limit_marks`, `alarms::AlarmState::limits_for` | the threshold bridge |
| `event_flags.rs` `fit_label` | segment label truncation / nub collapse |
| `plot_common.rs` `reconcile_trackers`, `plot_legend` | per-lane scan tasks, lane legend chips |
| `TimeSeries::get_range` + `TimeSeriesSlice::as_iter` | history backfill (below) |

**Renames.** One move, no rename: `StateEntry`, `StateEntryConfig`,
`MATCH_TOLERANCE`, and `match_state` leave `views/state_chip.rs` for a new
`views/state_map.rs`; `views/mod.rs` keeps re-exporting the same names so no
call site changes. `LinePlot` is by now a misnomer (it owns bars, scatter,
axes, event overlays, and soon lanes), but renaming it here would fight both
`XyLinePlot`/`ListLinePlot` and the `LinePlotCore<B>` direction in
`plot-shell-unification.md` — left alone deliberately, noted below.

## Design

### Data flow and history backfill

A live `ComponentStream` only yields new samples, so lanes never use one. They
read history the way the plot's GPU path already does — straight off the
memory-mapped nodes: `component.time_series.get_range(view.x)` for a
`TimeSeriesSlice`, `.as_iter()` for its node slices (newest first), then per
node `timestamps()` + `data()` + `dynamic::tensor::read_f64_at`, classifying
each value and folding equal neighbours into runs. Raw bytes, as
`scan_min_max` (`time_series/mod.rs:518`) does, not `iter_values`, which
allocates a `ComponentView` per sample.

Three edges matter:

- **Left edge.** A state that began before the window must still paint from
  x=0. Seed the first run from `TimeSeries::binary_search_nearest` at
  `view.x.0`, not from the first in-window sample.
- **Remote history.** Call `series.coverage(visible, &mut gaps)` and
  `crate::hydration::hydrator(cx).request(id, gap.range)` per `RemoteOnly` gap,
  the same shape as `LinePlot::gap_bands` (`line_plot.rs:286`); paint those
  stretches with `theme.plot_gap_band` instead of a state color.
- **Right edge.** The last run ends at the newest sample, not at the window
  edge; the remainder paints as stale (dimmed, hatch-free) so a dead channel
  never reads as a healthy steady state.

Scanning happens on a background task, not in paint. `LinePlot` grows
`lane_tracking: HashMap<EntityId, LaneTracking>` and `lane_tasks`, reconciled by
the existing `reconcile_trackers` helper and woken by `time_series.wait()`,
mirroring `LinePlot::spawn_tracker`. Each task computes a `LaneRuns` snapshot
for the current view; the paint closure only reads it.

Over-budget windows: reuse `resolve_lod_levels` (make it `pub(super)`), whose
companion components carry per-bucket min/max. A bucket with `min == max` is
that one state; `min != max` is a **mixed** bucket painted in
`theme.lane_mixed`. That keeps the T&M "never hide a glitch" property —
stride-sampling a state channel would silently erase a one-sample SAFE blip.
Below the budget, sub-pixel runs are promoted to a 1px sliver rather than
merged away, for the same reason.

### Config and live types

`views/time_series/state_lane.rs`:

```rust
/// Where a lane's discrete states come from.
#[derive(facet::Facet, Serialize, Deserialize, Clone, Copy, Default, PartialEq)]
#[repr(u8)]
pub enum StateSource {
    #[default]
    Coded,      // the lane's own StateEntry table
    Threshold,  // derived from the alarm store's limits — no config
    Bool,       // binding::any_on semantics, two theme colors
}

#[derive(facet::Facet)]
pub struct StateLane {
    pub label: SharedString,
    pub source: StateSource,
    /// Same entity type the state chip edits, so the inspector rows for a
    /// lane's table are literally the chip's rows.
    pub states: Vec<Entity<StateEntry>>,
    pub visible: bool,
    /// `0.0` = continuous segments; > 0 = status-history cells that wide.
    #[facet(inspect::range(min = "0.0", max = "3600.0"))]
    pub cell_seconds: f32,
    #[facet(skip)] pub component_id: ComponentId,
    #[facet(skip)] pub element_index: usize,
}
```

`StateLaneConfig` mirrors it and follows `TraceConfig`'s conventions —
`component_id: ComponentId` + `element_index`, not a name string — and is added
to `PlotPanelConfig` as `pub lanes: Vec<StateLaneConfig>`. The field is
`#[serde(default)]`-additive, so **no `SUPPORTED_LAYOUT_VERSION` bump**.

`views/state_map.rs` gains the classifier both lane sources share: a `Class`
enum (`Coded(usize) | Severity(usize) | Normal | Unknown | Gap | Mixed`) and a
`Classifier` with `resolve(source, states, ElementRef, &App)`, `classify(f64)
-> Class`, and `describe(Class, &Theme) -> (SharedString, Hsla)`. `resolve`
runs once per scan on the main thread — it needs the theme and the alarm store
— and the result is sent into the background task.

### Rendering

Layout is one new function in `time_series/mod.rs` —
`split_lanes(pa: Bounds<Pixels>, lane_count: usize) -> (Bounds, Bounds)`,
returning the trace region and the lane band beneath it.
`plot_area` and `left_margin` are untouched; only the callers that must exclude
the band change — the GPU child's inset (`mod.rs:2388`), the gridline loop in
`paint_underlay`, and `hover_samples`. With zero traces the band takes the whole
area, and the Y tick labels are suppressed (an auto 0..1 axis means nothing);
each lane's label is painted in the Y-axis column beside it instead, so lanes
cost no extra chrome.

Painting lives in `views/time_series/lane_paint.rs`, a plain canvas in
`TimeSeriesPlot::render` slotted next to the flag canvas, fed by a `LanePaint`
snapshot struct (no `App` access in paint — the `ClusterPaint` idiom). Per run:
a filled quad in `Theme::dim(color, 0.55)` with a 1px separator, plus the state
label shaped through `event_flags::fit_label` (lifted to `pub(super)`), which
already handles truncation-to-gap and the bare-nub collapse.

**Status-history grid.** `cell_seconds > 0` quantizes the window into fixed
buckets and paints one inset cell per bucket. Where the classes are ordered
(threshold/severity), the bucket takes the **worst** class in it, not the last —
a health grid that hides a one-sample critical is worse than useless. Coded
lanes take the last value with a mixed marker when the bucket wasn't constant.

**Hover.** `TimeSeriesPlot::hover_readout` gains lane rows: lane label, state
name, and time-in-state at the crosshair. No new popover.

### Limits are data; color budget

The threshold bridge takes **zero** limit configuration. It reads
`alarms::AlarmState::limits_for(component, element)`, sorts the `AlarmLimit`s
into `LimitKind::Upper` / `Lower` bands, and classifies a sample as the highest
`Severity` whose limit it violates. Colors come from `theme.alarm_color(idx)`,
tint from `theme.alarm_tint` — the same table the plot's limit lines and every
`binding.rs` instrument use. A `Threshold` lane therefore has no color, no
threshold, and no severity in its config at all; pointing it at a component is
the whole setup.

Color budget (HPHMI/NASA, and the survey's cross-cutting note): the *normal*
class is `theme.lane_normal`, a near-background gray, not a green. Saturated
color is reserved for abnormal classes and for coded states the operator
explicitly colored. Unknown codes get `theme.lane_unknown` (neutral, distinct
from any severity) rather than being folded into normal, matching the chip's
"an unmatched code must not masquerade as a known one". Three new theme fields
— `lane_normal`, `lane_unknown`, `lane_mixed` — go in `theme.rs` for all
palettes; nothing outside `theme.rs` names a color.

## Implementation steps

1. **Extract the state map.** Move `StateEntry`, `StateEntryConfig`,
   `MATCH_TOLERANCE`, `match_state` and their tests from
   `src/views/state_chip.rs` into new `src/views/state_map.rs`; re-export
   unchanged from `src/views/mod.rs`; `state_chip.rs` imports them. Pure move,
   no behavior change. *Touches:* `views/state_map.rs`, `views/state_chip.rs`,
   `views/mod.rs`.

2. **Classifier + theme.** Add `StateSource`, `Class`, `Classifier` to
   `views/state_map.rs`; add `lane_normal`/`lane_unknown`/`lane_mixed` to every
   palette in `src/theme.rs`. Unit-test `classify` for an Upper+Lower band, a
   two-severity ladder, and an empty limit set. *Touches:* `views/state_map.rs`,
   `theme.rs`.

3. **Lane model and run building.** New `src/views/time_series/state_lane.rs`:
   `StateLane`, `StateLaneConfig`, `Run`, `LaneRuns`, and
   `build_runs(component, element, range, &Classifier, min_run_us) -> LaneRuns`.
   Tests: neighbour folding, sub-pixel sliver promotion, pre-window seeding via
   `binary_search_nearest`, empty component. *Touches:* new file,
   `views/time_series/mod.rs` (module decl + re-export).

4. **Hang lanes off the model.** `LinePlot.lanes: Vec<Entity<StateLane>>`, plus
   `lane_tracking`/`lane_tasks` reconciled through `reconcile_trackers` and a
   `spawn_lane_tracker` modeled on `spawn_tracker`; make `resolve_lod_levels`
   `pub(super)`. *Touches:* `views/time_series/line_plot.rs`.

5. **Paint.** New `src/views/time_series/lane_paint.rs` (`LanePaint`,
   `paint_lanes`); `split_lanes` in `views/time_series/mod.rs`; lane canvas
   child in `TimeSeriesPlot::render`; inset the GPU child and the gridline/label
   loops; lift `fit_label` to `pub(super)` in `event_flags.rs`. *Touches:*
   `views/time_series/{mod,lane_paint,event_flags}.rs`.

6. **Persist.** `PlotPanelConfig.lanes`; build/read in
   `TimeSeriesPlot::from_config` / `to_config`. Extend
   `panel_configs_round_trip_through_json` in `src/tiles/panels.rs` with a lane
   carrying a coded table and a threshold lane. *Touches:*
   `views/time_series/config.rs`, `tiles/panels.rs`.

7. **Inspect.** `register_entity_list::<LinePlot, StateLane>` and
   `::<StateLane, StateEntry>` in `src/inspector/registry/defaults.rs`;
   `build_lane_add_wizard` in `builders.rs` reusing
   `inspector/trace_picker.rs`, defaulting `source` to `Threshold` when the
   alarm store has limits for the picked element, `Bool` for a `PrimType::Bool`
   component, else `Coded`.

8. **Palette entries.** Register `WidgetKind::state_timeline()` in
   `src/views/dashboard/{mod,widgets}.rs`, reusing `build_plot`/`snapshot_plot`
   with `.with_tile("state_timeline", …)` and a lane-first add flow; add
   "State Timeline" and "Status Grid" rows to `new_panel_rows` in
   `src/tiles/panels.rs`, both calling `add_registered_panel` with a
   lanes-only `PlotPanelConfig`.

9. **Hover, legend, stale tail.** Lane rows in `hover_readout`, lane chips via
   `plot_common::plot_legend`, dimmed post-last-sample tail — all in
   `views/time_series/mod.rs`.

10. **LoD mixed buckets** (optional, last). Over-budget lanes classify from the
    LoD companion's min/max, painting `min != max` as mixed. *Touches:*
    `views/time_series/{state_lane,line_plot}.rs`.

Steps 1–7 are the widget; 8 makes it findable; 9–10 are polish. Each step ends
`cargo build -p metor-panel` green.

## Open questions

1. **Lane height.** Fixed (~22px, Grafana-like) or proportional so a lanes-only
   plot fills its pane? Proportional reads better standalone; fixed keeps mixed
   plots stable while lanes are added. Proposed: fixed with a per-plot
   `lane_height` override, defaulting to proportional when there are no traces.

2. **Is the extra `WidgetKind` worth it?** A "State Timeline" kind is ~20 lines
   reusing the plot's builders and gives both hosts a named entry with its own
   tab title, at the cost of a second on-disk key for the same view. The
   alternative is a second palette row that writes `"time_series_plot"`, with
   the dashboard reaching lanes only through the plot's inspector.

3. **`LinePlot` rename.** Deferred here on purpose. Worth doing as part of
   `plot-shell-unification.md` (where all three plot models get reshaped), or
   never?

4. **Enum names from the control system.** Coded lanes need a table the
   operator types today. Component metadata is a `HashMap<String, String>` and
   carries no variant names. Should the FSW announce them (a `states` metadata
   key, `"0=IDLE,1=SETTLING,…"`), so a coded lane binds with zero config the way
   a threshold lane does? That is an FSW-side change, but it is the difference
   between the two lane sources being equally cheap.

5. **Sequence and alarm lanes.** A lane's source could be an `EventSource`
   (`plot_events::EventKindKey`) rather than a component element — a sequence
   channel's run state is already a discrete state with a color
   (`Theme::run_state_color`). Natural fit, but it makes `StateLane` bind to two
   different kinds of thing. Follow-up, or fourth `StateSource` variant?
