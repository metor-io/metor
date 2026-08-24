# Telemetry Visualization Additions — Prior-Art Survey

A gap analysis of metor-panel's visualization set against telemetry tooling from four
fields, surveyed 2026-08-22:

- **Aerospace ground systems**: NASA Open MCT, OpenC3 COSMOS, Yamcs/Yamcs Studio,
  SCOS-2000, IADS (flight test), NASA display standards, AltosUI
- **Industrial control**: Ignition, WinCC, InTouch, iFIX, PI Vision, High-Performance
  HMI (ISA-101/Hollifield), ISA-18.x alarm standards, SPC/OEE/andon
- **Robotics / observability / T&M**: Foxglove, PlotJuggler, Rerun, Grafana, Honeycomb,
  DEWESoft, LabVIEW, oscilloscopes, Saleae
- **Automotive/race telemetry**: MoTeC i2, Vector CANape, McLaren ATLAS

## What metor-panel already covers

Time-series plot (4 Y-axes, LOD decimation, measurement cursors, alarm limit
lines/tint, event flags, global time-range linking), XY plot, list plot (vector
profile), meter/gauge/state-chip/traffic-light(+grid)/monitor/text, attitude ball with
vector markers, component/data tables + browsers, alarm/log/sequence panels, 3D viewer,
system graph, free-form dashboard with telemetry-bound connectors, and a node-editor
derivation layer (FFT, window, arithmetic, resample, persist).

Verified absent: log axes, histogram, heatmap, spectrogram, state timeline, stacked
plots, geospatial, annotations, export, staleness indication, table alarm coloring.

---

## Tier 1 — strong prior art in ≥2 fields, natural fit with existing infrastructure

1. **State timeline / state transitions pane.** Horizontal lanes of colored segments
   showing enum/mode channels over time (Grafana State Timeline, Foxglove State
   Transitions, Rerun StateTimelineView, LabVIEW mixed-signal digital lanes). The
   single most conspicuous gap — mode/state channels are unreadable as line plots.
   Reuses `StateEntryConfig` (value→label/color) from the state chip, the plot's time
   axis/`TimeRangeBehavior`, and the event-flag gutter. Include Grafana's threshold
   bridge (continuous → discrete states) and "status history" cell-grid variant for
   health checks.

2. **Stacked strip-chart plot.** N sub-plots sharing one X axis, each with its own Y
   (Open MCT stacked plots, IADS strip charts, PlotJuggler split tabs, pen recorders).
   The 4-axis overlay doesn't scale to heterogeneous quantities (thrust vs valve state
   vs temp). Could be a tile arrangement of linked `LinePlot`s sharing pan/zoom + one
   shared cursor readout column.

3. **Moving analog indicator (HPHMI flagship widget).** Vertical bar = full instrument
   range with normal/warn/critical bands drawn *inside*, pointer for current value,
   optional setpoint caret, alarm badge on violation (Ignition, PAS/Hollifield,
   Rockwell). The most codified widget in industrial HMI and a perfect
   `views/binding.rs` fit — bands come from the alarm store, never config. Arguably a
   `Meter` style variant rather than a new view.

4. **Histogram.** (Skip for Now) Distribution of a channel over a window or the visible plot range
   (Grafana, MoTeC damper/throttle histograms, scope measurement statistics). Needs a
   `Histogram` node op (window + bins) plus a view; MoTeC's linked-recompute (histogram
   follows plot zoom) and gating (only when condition holds) are the distinctive
   interactions. 

5. **Spectrogram / waterfall.** (Skip for now) Time × frequency × magnitude-as-color, scrolling
   (DEWESoft, IADS flutter analysis, Yamcs Intensity Graph). The `Window→FFT→Magnitude`
   chain already produces spectra; today they can only be viewed as an instantaneous
   list plot. A history-accumulating heatmap view over a vector-valued stream closes
   this — and generalizes to any vector-over-time (thermal zone arrays, battery cells).
   Flight-test tools own this space; the open-source MCS frameworks (Open MCT, COSMOS)
   lack it — differentiator.

6. **Annunciator panel.** Fixed grid of *named, latching* tiles colored by condition,
   with first-out marking (ISA-18.1, IADS annunciators, COSMOS Limits Monitor, SpaceX
   engine-status grids). Traffic-light grid is close but has no labels-with-value, no
   latching (transient violations self-erase), no first-out. Latching + "keep visible
   until dismissed" is the point: management by exception.

7. **Alarm coloring in tables/value strips.** Verified absent: no table cell consults
   the alarm store. Every surveyed field colors alphanumeric values by limit state
   (COSMOS/SCOS ANDs, NASA Appendix F, PI Vision tables). Wire
   `ComponentValueStrip`/`CellKind` to the same alarm lookup `binding.rs` uses. High
   value-to-effort ratio.

8. **Staleness indication (cross-cutting).** Every aerospace tool flags data that
   stopped arriving (NASA standard, COSMOS `STALENESS_SECONDS`, Yamcs expiry) — a dead
   value must never look healthy. metor-panel views repaint on `next()` and otherwise
   hold the last frame silently. A shared "age > threshold ⇒ gray/strike/badge"
   behavior in `binding.rs` + value strips would cover all instruments and tables at
   once.

9. **Log-scale Y axes.** Verified absent from all plots. Table stakes in T&M (spectra,
   PSDs) and observability.

10. **Alarm limit bands + sparkline normal-band.** Limits render as lines only; shaded
    warn/critical regions (Grafana thresholds, HPHMI bands) read pre-attentively. Same
    data, better rendering. Ignition's sparkline "desired range band" applies to the
    Monitor widget.

## Tier 2 — strong prior art, medium effort

11. **Value-density heatmap / persistence display.** Time-bucketed 2D histogram of a
    high-rate channel (Grafana heatmap, scope digital-phosphor persistence). The honest
    view when a mean line lies (multimodality, rare glitches). Shares the heatmap
    renderer with #5.

12. **Radar/spider chart.** N channels as spokes, each normalized so "normal" is the
    same radius; deformation = abnormality (Ignition Radar Chart, HPHMI Level-1
    overviews). Normalize to the alarm-store normal band, not engineering range.

13. **XY plot upgrades**: static reference/envelope curves (pump curves, V-n envelopes,
    IADS predicted overlays — the reference *curve* is the XY analog of the limit
    line), color-by-third-channel scatter (MoTeC g-g diagram), comet-trail fade for
    recent points.

14. **Alarm shelving + latching out-of-limits view.** ISA-18.2 shelving (time-limited
    suppression with visible expiry) and COSMOS Limits Monitor semantics (ignore item,
    temporarily hide, reappear-on-change) in the alarm panel.

15. **Map / ground-track view.** GPS/position channels on a map or ground-track
    (Foxglove Map, Grafana Geomap, MoTeC track maps, mission-control wall maps). The
    automotive idiom — track colored by a channel's value gradient — projects any
    time-series onto position. Absent from all three open-source MCS frameworks, so
    also a differentiator; needs tile sourcing/offline strategy decisions.

16. **Timeline / Gantt pane.** (Skip for now) Planned activities vs actual execution with a now-line
    (Open MCT Timeline/Plan, Ignition Equipment Schedule, ISA-88 batch Gantt). Natural
    fit with the sequence system: channels as rows, runs as bars.

17. **Telemetry imagery view.** `ImageWidget` is static-only. Imagery-as-telemetry
    (COSMOS `IMAGEVIEWER` — image bytes out of a packet; Open MCT thumbnail strip with
    scrub + fresh-image indication) rides the existing stream/time machinery.

18. **Export.** CSV of a plot's visible traces / a table; PNG of a pane. Verified
    absent; universal elsewhere (Open MCT tables, Foxglove, Grafana).

19. **Node-op gaps exposed by the survey**: integral, IIR/FIR/median filters, windowed
    statistics (min/max/RMS/percentile — feeds #4), quaternion→Euler (PlotJuggler's
    canonical example), and a `Condition` op producing a bool from comparisons —
    feeding #6, #20, and gating.

20. **Manual plot annotations.** User-placed vertical markers, text notes, shaded
    regions (Ignition Power Chart annotations, IADS test points, Saleae timing
    markers). Only store-sourced event flags exist today; measurement-cursor
    persistence infrastructure is reusable.

## Tier 3 — ambitious / cross-cutting

21. **Run comparison / relative-time overlay.** Re-base the time axis to an event start
    and overlay N episodes of the same channels: PI event frames + golden-batch
    envelopes, MoTeC lap overlay (with auto-computed difference channels), Foxglove
    comparison slots. The general lesson: *overlay on a domain-meaningful abscissa, not
    wall clock*. Most valuable for test campaigns (compare this burn/deployment/startup
    against the last ten). Big: needs an episode concept over the DB.

22. **Condition sets → conditional styling.** User-defined boolean logic over telemetry
    driving color/visibility/text of any widget (Open MCT Condition Widgets, BOY rules,
    mimic dynamic objects). The connector `threshold`+`on_color` binding is a one-off
    special case of this; generalizing turns the dashboard into a true mimic/synoptic
    layer.

23. **Global hover cursor + click-to-drill.** One shared time cursor across all panes
    (PlotJuggler tracker, MoTeC cursor, Foxglove sync) — global time-range linking
    exists, hover-cursor sync doesn't. Plus the exemplar idiom: click a spike → open
    the log/message/event nearest that instant (the event-popover machinery is halfway
    there).

24. **Phase-aware layouts.** Auto-switch preset/layout on a mode channel's value
    (AltosUI's flight-phase tabs: pad → ascent → descent → landed, each showing only
    phase-relevant instruments). Presets + auto-apply already exist; this adds a
    trigger binding.

25. **Deviation-from-setpoint displays.** PV vs SP paired rendering: deviation bar
    centered on zero, SP caret on meters, PV/SP/OP trend convention (every DCS). Cheap
    once setpoint channels are identifiable; pairs with #3.

26. **Lower priority, noted for completeness**: SPC control charts (X̄/R, EWMA, CUSUM
    with ±3σ *statistical* limits — manufacturing-specific; keep distinct from alarm
    limits if ever built), digital/logic timing lanes with protocol decode (Saleae
    pattern — raw lanes + annotations + searchable frame table), profile-view target
    overlays on the list plot (target profile + band), oscilloscope-style triggered
    capture (recorder vs scope distinction), voice/audio callouts (AltosUI).

## Cross-cutting design principles the survey converged on

- **Limits are data, not chart config** — every mature tool projects limits from the
  alarm/channel definition. metor-panel's `binding.rs` already follows this; extend it
  to tables (#7), bands (#10), and the analog indicator (#3) rather than adding
  per-widget limit config.
- **Color budget** (HPHMI/NASA): gray-scale ground, saturated color only for abnormal,
  alarm palette never reused decoratively — worth an explicit note in STYLE.md when
  building #1/#3/#6.
- **Never hide a glitch**: min/max envelope decimation is treated as a correctness
  property in T&M — metor-panel's LOD zigzag already complies; keep new views
  (spectrogram, heatmap) on the same standard.
- **Every number wants context**: the recurring trio is value + sparkline + limit band.
- **The x-axis should be a channel choice, not hardwired to time** (distance,
  RPM/orders, array index, another channel).
