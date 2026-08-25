# Alarm limit bands + sparkline normal band

Survey item #10. Companion plan:
[03-moving-analog-indicator.md](03-moving-analog-indicator.md) — both render the
same alarm limits as shaded regions and share `binding::limit_bands`.

## Summary

The plot already draws every declared limit as a hairline with a label
(`alarm_limit_lines`, `time_series/mod.rs:574`) and washes the whole plot when an
alarm is active (`alarm_plot_tint`, `:551`). A line says "the limit is here"; a
shaded region says "you are *in* the bad part", which is what reads
pre-attentively. This is a rendering upgrade to existing views — no new widget,
no new config, the same data out of the same store. The one structural move is
that bands belong to `LinePlot` rather than to `TimeSeriesPlot`'s chrome, which
is what gives the Monitor's sparkline its desired-range band for free.

## Reuse vs. new

**Pure upgrade — nothing new is registered.** No `PaneItem`, no `WidgetKind`, no
config struct, no serialization key, no `TILE_LAYOUT_VERSION` bump. The existing
`LinePlot::show_alarm_limits` (`line_plot.rs:138`, persisted as
`PlotPanelConfig::hide_alarm_limits`, `config.rs:29/190/267`) keeps gating the
whole limit display: bands and lines are the same declaration rendered twice, and
a second toggle would just be a way to get them out of sync.

The only *placement* decision: bands are painted by **`LinePlot`**, not by
`TimeSeriesPlot::paint_underlay`. Rationale below.

## Design

### Where bands live

`TimeSeriesPlot::render` (`mod.rs:2206`) stacks three siblings: an underlay canvas
(`paint_underlay`, `:599` — tint, gridlines, zero line), the `LinePlot` child, and
an overlay canvas (`paint_overlay`, `:662` — axis chrome, limit lines, labels).
The `LinePlot` child is inset by exactly the chrome margins (`mod.rs:2388-2396`),
so its own bounds equal `plot_area(outer, axis_count)` (`mod.rs:341`) — the
underlay's coordinate frame, to the pixel.

`Monitor` (`monitor.rs:62`) embeds a bare `LinePlot` with no chrome at all, so a
band painted in the parent's underlay would never reach the sparkline. Painting
inside `LinePlot::render` (`line_plot.rs:744`) gets both surfaces from one
implementation, and `LinePlot` already owns everything the projection needs:
`effective_view(cx)` (`line_plot.rs:414`), `axis_bounds(i)` and
`to_screen` (`bounds.rs:27/238`), and its visible traces.

Ordering inside `LinePlot::render`: a `bands_canvas` inserted **before**
`plot_canvas` in the child list (`line_plot.rs:842-845`), so bands sit under the
GPU trace image and under the existing gap bands. That puts them *over* the
parent's gridlines rather than under — acceptable at band alpha (the grid still
reads through), and the alternative would mean teaching `TimeSeriesPlot` a
band pass that the sparkline can't reach.

### Band data

`binding::limit_bands(at, cx) -> SmallVec<[(f64, f64, Hsla); 4]>`, specified in
plan #3 (§ "Bands from the alarm store"): value-space regions derived from
`LimitKind::{Upper, Lower}` (`metor-proto/wkt/src/msgs.rs:589`), back-to-front
ordered — normal band first, then abnormal by ascending severity — with the
normal band emitted only when the element declares both sides. Whichever plan
lands first adds it; the second just calls it.

In `line_plot.rs`, a private collector mirroring `alarm_limit_lines`' shape:

```rust
/// `(axis_index, from, to, color)` for the plot's visible traces, ordered so
/// later bands paint over earlier ones.
fn alarm_bands(&self, cx: &App) -> SmallVec<[(usize, f64, f64, Hsla); 8]>
```

Two rules keep it honest:

- **Per axis, one band set.** Collect each visible trace's bands keyed by
  `axis_index`, dedupe by `(from, to, color)`. If an axis ends up with more than
  one distinct set, paint none for that axis — two elements with different
  redlines overlaid on one scale would shade a region that is normal for one of
  them. The common cases both survive: a single-trace plot, and the Monitor
  sparkline binding every element of a component (`monitor.rs:127-139`) where a
  component-wide `AlarmDef` (`element_index: None`, `alarms/mod.rs:193`) yields
  identical sets that collapse to one.
- **Bands only on a single-axis plot.** With `axis_count() > 1` a band from axis
  2 painted across the full plot area lands at the wrong height for axis 0's
  grid. Multi-axis plots keep lines plus their axis-marker triangles
  (`mod.rs:784-804`). See Open questions.

### Painting

In the new canvas, for each `(axis, from, to, color)`: project with
`view.axis_bounds(axis).to_screen(bounds, x, y)`, clamp the two y values to
`bounds`, skip when the clamped span is empty or the band misses the view, then
`window.paint_quad(gpui::fill(band_bounds, color))`. Infinite ends clamp to the
view edge, which is the whole point — a critical region above the top of the
scale must still shade the top of the plot.

The prepare closure reads `self.alarm_bands(cx)` alongside the existing
`effective_view`, so the paint closure stays `'static` with no `App` access, the
same pattern as `CursorPaint` (`mod.rs:850`).

### Lines and tint stay

`alarm_limit_lines` and its right-edge labels (`mod.rs:805-833`) remain in
`paint_overlay`, on top of the bands — the band is the pre-attentive layer, the
hairline is the precise one, and the label is the only thing that names the
limit. `alarm_plot_tint` (`:551`) also stays: it reports *active* alarms, which is
a different fact from *declared* limits, and it is the one thing that fires when a
control system raises an alarm with no declared limit at all.

Worth folding while here: `alarm_limit_lines` re-implements the store lookup that
`binding::limit_marks` (`binding.rs:332`) already does, minus the label. Either
give `binding` a labelled variant and have the plot call it, or leave the note.
Not a blocker.

### Monitor sparkline

Nothing to add on the Monitor side — it already hosts a `LinePlot`
(`monitor.rs:219-228`) whose `show_alarm_limits` defaults to `true`
(`line_plot.rs:173`). Once bands paint inside `LinePlot`, the sparkline shows the
normal-band shading Ignition calls the "desired range" with no Monitor change.
Two things to check when it does:

- At ~40px of sparkline height the abnormal bands are slivers; the *normal*
  band doing the work is exactly the intent.
- The Monitor sparkline binds every element of the component. The dedupe rule
  above decides whether it bands or not; verify against a multi-element
  component with per-element limits.

### Theme

Reuses `Theme::alarm_band(severity_index)` and `Theme::normal_band()` from plan
#3 (`theme.rs`, beside `alarm_color`/`alarm_tint` at `:164-188`). Both derived,
so no theme table changes. A plot region is much larger than a meter track, so
the meter alpha may be too heavy here — if so, the two surfaces split into
`alarm_band` / `alarm_band_plot` rather than call sites inventing `Hsla`
literals (`STYLE.md`: no `Hsla` outside `theme.rs`). Decide by eye during step 3.

## Implementation steps

1. **`src/views/binding.rs`** — `limit_bands` per plan #3 step 1, if that plan
   hasn't landed it.
2. **`src/theme.rs`** — `alarm_band` / `normal_band`, if not already added.
3. **`src/views/time_series/line_plot.rs`** — `alarm_bands(&self, cx)` with the
   dedupe and single-axis rules; the band canvas as the first child of
   `LinePlot::render`. Unit-test the projection/clamping helper (infinite ends,
   band fully outside the view, degenerate axis range) — the collector itself
   needs an alarm store and stays untested, like `alarm_limit_lines`.
4. **Visual check** on a multi-trace, multi-axis `TimeSeriesPlot`: bands under the
   traces, gridlines still legible, lines and labels on top, no bleed into the
   axis chrome (the chrome fills in `paint_overlay:677-694` paint after, so a
   band clipped to `plot_area` cannot escape).
5. **Visual check** on a Monitor widget with a limited component, and on one
   whose elements carry different limits (must fall back to no bands).
6. **`docs/plans/telemetry-viz-additions.md`** — mark #10 done once both
   surfaces are verified.

## Open questions

- **Multi-axis plots.** Suppressing bands above one axis is the conservative
  call. The alternative — band only axis 0, since that is what the gridlines and
  zero line already track (`mod.rs:619`) — is defensible and shows something
  rather than nothing. Pick one after looking at a real four-axis plot.
- **Does the hairline survive?** With a band edge at the same value, the line may
  be redundant everywhere except where it carries a label. Cheapest experiment:
  keep it, and drop unlabelled lines only if the result looks busy.
- **XY and list plots.** `xy_plot` and `list_plot` have the same limit
  vocabulary available and no limit rendering at all today. Survey #13 covers XY
  reference curves separately; a Y-band there is a small follow-on once
  `limit_bands` exists.
- **Bands vs. LOD envelopes.** A dimmed min/max envelope (`plot_envelope_alpha`,
  `line_plot.rs:776`) over a band stacks two translucent layers. Check that a
  critical band under an envelope still reads as critical.
