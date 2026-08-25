# Telemetry visualization plans

One plan per surviving Tier 1/2 item from `../telemetry-viz-additions.md` (survey
numbering kept in the filenames). Skipped for now: #4 histogram, #5 spectrogram,
#16 timeline/Gantt.

Each plan follows the same shape: Summary, Reuse vs. new, Design, Implementation
steps, Open questions.

## Plans

| Plan | Verdict |
|---|---|
| [01-state-timeline](01-state-timeline.md) | `LinePlot.lanes`, shared `views/state_map.rs` |
| [02-stacked-strip-chart](02-stacked-strip-chart.md) | `AxisLayout::Stacked` on `LinePlot`; layout version bump |
| [03-moving-analog-indicator](03-moving-analog-indicator.md) | `MeterStyle::Analog` variant |
| [06-annunciator-panel](06-annunciator-panel.md) | `TrafficLightGrid` → `Annunciator` rename + latching |
| [07-table-alarm-coloring](07-table-alarm-coloring.md) | `ComponentValueStrip` render path |
| [08-staleness-indication](08-staleness-indication.md) | `sample_time()` on streams + timer race in bindings |
| [09-log-scale-axes](09-log-scale-axes.md) | axis ranges stored in display (log) space |
| [10-alarm-limit-bands](10-alarm-limit-bands.md) | bands painted by `LinePlot`; `binding::limit_bands` |
| [11-value-density-heatmap](11-value-density-heatmap.md) | `PlotStyle::Density`, GPU additive accumulation |
| [12-radar-chart](12-radar-chart.md) | new view (only one); normal band = fixed radius |
| [13-xy-plot-upgrades](13-xy-plot-upgrades.md) | reference curves as chrome; per-point ramp buffer |
| [14-alarm-shelving-latching](14-alarm-shelving-latching.md) | alarm panel upgrade; `AlarmShelved` wkt msgs |
| [15-map-ground-track](15-map-ground-track.md) | XY plot + `Projection`; `Geodetic` node op |
| [17-telemetry-imagery](17-telemetry-imagery.md) | `ImageWidget` → `ImageView`, U8 tensor frames |
| [18-export](18-export.md) | CSV from raw DB range; PNG scoped out (no gpui readback) |
| [19-node-op-gaps](19-node-op-gaps.md) | `Reduce` + `Threshold`→`Condition` + op catalog |
| [20-manual-annotations](20-manual-annotations.md) | annotations as a fourth `EventSource` |

## Shared work discovered across plans

- **`binding::limit_bands` / keeping `LimitKind`**: needed by 03, 10, 12 —
  `limit_marks` currently discards upper/lower, which is exactly what bands and
  normal-band normalization need. Build once, first plan to land wins.
- **Per-point ramp storage buffer in the line/scatter shaders**: 13's
  color-by-third-channel + comet trail and 15's value-gradient track are the
  same GPU change.
- **Staleness**: 08 is the general mechanism; 17's fresh-image badge is a narrow
  precursor and should collapse into it when 08 lands.
- **Latch state machine** (`alarms/latch.rs`): shared by 06 and 14.
- **Plot internals** (01, 02, 09, 11, 20 all touch `LinePlot`/`gpu.rs`): read
  alongside `../plot-shell-unification.md`; the `LinePlot` rename is deferred to
  that refactor.
