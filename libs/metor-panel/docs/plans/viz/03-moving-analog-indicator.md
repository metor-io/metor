# Moving analog indicator (HPHMI)

Survey item #3. Companion plan: [10-alarm-limit-bands.md](10-alarm-limit-bands.md) —
both render alarm limits as shaded bands and share `binding::limit_bands`.

## Summary

The HPHMI moving analog indicator is a bar whose *track* carries the normal /
warn / critical bands and whose value reads as a pointer against them, rather
than as a colored fill. metor-panel already has that bar — `Meter`
(`src/views/meter.rs`) — with the same binding, the same alarm-sourced limits,
and the same two-surface registration; what it lacks is the band-and-pointer
rendering and the neutral color budget. This lands as a **style variant of
`Meter`**, not a new view.

## Reuse vs. new

**Repurpose `Meter`.** A new `MovingAnalogIndicator` view would duplicate
`Meter::from_config` / `rebind` / `to_config` (`meter.rs:120-243`), a second
`MeterConfig` clone, a second `PaneItem` (`tiles/panels.rs:380-417`), a second
`WidgetKind` (`dashboard/widgets.rs:373-389`), a second `From<ScaleSeed>`
(`views/instrument.rs:35`) and a second palette entry (`panels.rs:1275`) — some
250 lines of boilerplate whose only real difference is what the canvas paints.
Every field the indicator needs (`component`/`element`/`min`/`max`/`unit`/
`orientation`/`show_value`/`show_limits`) is already on `MeterConfig`.

The precedent is one file over: `Gauge` carries `GaugeStyle::{Arc, Needle}`
(`gauge.rs:33`) for exactly this — same instrument, same binding, two ways of
drawing the value. So:

```rust
/// How the value reads against the scale.
pub enum MeterStyle {
    /// Bar filled from the scale origin to the value.
    #[default]
    Fill,
    /// HPHMI moving analog indicator: alarm bands inside the track, value as
    /// a pointer against them.
    Analog,
}
```

**No rename.** "Meter" still describes both styles — a linear bar reading one
element against a scale. Renaming would also cost a `serialization_key` change
(`panels.rs:406`) plus a `WidgetKind::meter` change (`dashboard/mod.rs:97`), and
tile layouts have no migration path — `TileGroup::load` rejects any document
whose version differs (`tiles/mod.rs:62-79`), so a rename means bumping
`TILE_LAYOUT_VERSION` in `metor-proto-wkt` and breaking every saved layout for a
cosmetic gain. `MeterStyle` defaults to `Fill` and `#[serde(default)]` already
covers `MeterConfig`, so old layouts load unchanged and the version stays put.

## Design

### Config

`MeterConfig` (`meter.rs:52`) gains `pub style: MeterStyle`, defaulting to
`Fill`. `Meter` (`meter.rs:86`) gains the matching public field so it appears in
the inspector; enum fields render as a variant picker with no attribute needed
(`inspect::variants` only *restricts* the list — `inspector/registry/dispatch.rs:121`).
`to_config` round-trips it.

Nothing else is added. In particular **no band or setpoint limits in config** —
bands come from the alarm store, per the survey's "limits are data" principle
and the existing `binding` contract (`binding.rs:19-22`).

### Bands from the alarm store

`AlarmLimit` already carries the side of the value it bounds —
`LimitKind::{Upper, Lower}` (`metor-proto/wkt/src/msgs.rs:589-606`) — and
`binding::limit_marks` (`binding.rs:332`) throws it away. Add a sibling in
`binding.rs` that keeps it, shared with plan #10:

```rust
/// Value-space regions declared for `at`, ordered back-to-front: the normal
/// band first, then abnormal bands by ascending severity, so a painter can
/// draw them in order and let critical win the overlap.
pub(crate) fn limit_bands(at: ElementRef, cx: &App) -> SmallVec<[(f64, f64, Hsla); 4]>
```

Rules:

- `Upper` at `v`, severity `s` → `(v, f64::INFINITY)` in `theme.alarm_band(s)`.
- `Lower` at `v`, severity `s` → `(f64::NEG_INFINITY, v)`.
- A **normal** band `(highest lower, lowest upper)` in `theme.normal_band()` is
  emitted only when the element declares both a lower and an upper limit — a
  one-sided limit has no bounded desired range, and shading half the scale gray
  would say something the control system never declared.
- Empty when the store is uninitialized (tests) or nothing is declared, exactly
  like `limit_marks`.

`alarm_tint` (`binding.rs:350`) currently re-derives the severity it needs; split
out `pub(crate) fn active_severity(at, cx) -> Option<usize>` so the badge and the
tint read the same value, and make `alarm_tint` a two-line wrapper over it.

### Rendering

In `Meter::render` (`meter.rs:321`), branch on `self.style` inside the existing
canvas — one canvas, two paint bodies:

- `Fill`: unchanged (`paint_rounded` track, `slice` fill, `tick` marks).
- `Analog`:
  1. `paint_rounded(bounds, theme.border_primary)` — the gray ground.
  2. For each `(from, to, color)` from `limit_bands`, in order: project with a
     new `band_span(from, to, min, max) -> Option<(f32, f32)>` beside
     `fill_span`/`limit_position` in `meter.rs`, then `paint_rounded(slice(...))`.
     Unlike `limit_position` (`meter.rs:312`), which *drops* an out-of-scale
     limit, `band_span` **clamps** and returns `None` only when the band misses
     the scale entirely — a critical region running off the top of the scale
     still has to show its visible part.
  3. Pointer: a `POINTER_PX`-thick bar across the full track width at
     `fill_span(value, …).1`, plus a small triangle on the label side. Neutral
     `theme.text_primary`, never the accent.
  4. Badge: when `active_severity(at, cx)` is `Some(idx)`, an
     `Icon::Dot.svg_color(7.0, theme.alarm_color(idx))` pinned to the tile's
     label row — the same idiom as the status bar (`app.rs:576`). The
     whole-tile `alarm_tint` wash stays as-is under both styles.

`show_limits` gates ticks in `Fill` and bands in `Analog`; it is one control over
"draw what the control system declared", not two.

**Color budget.** In `Analog` the ground is gray and the pointer is
`text_primary`; saturated color appears only in the bands and the badge, and only
when a limit or an alarm exists. `MeterConfig::color` currently defaults to
`theme.control_active` (`meter.rs:162`) — a saturated green fill on every meter,
which is exactly the budget violation HPHMI warns about. Resolve the default per
style at construction: `Fill` keeps `control_active`, `Analog` takes
`text_primary`. An explicitly configured color still wins in both.

### Theme

Two derived methods on `Theme` (`theme.rs:164-188`), next to `alarm_color` /
`alarm_tint`. Derived rather than palette fields, so none of the thirteen theme
tables need editing:

```rust
/// Fill for an alarm band drawn inside an instrument track or behind a plot
/// region — heavier than `alarm_tint`, which washes a whole surface.
pub fn alarm_band(&self, severity_index: usize) -> Hsla  // a ≈ 0.22

/// Fill for the declared normal range: gray ground, never a hue.
pub fn normal_band(&self) -> Hsla                        // Self::dim(self.text_tertiary, 0.10)
```

Plan #10 uses both at plot alphas; see its "Theme" note on whether the two
surfaces need different alphas.

## Implementation steps

1. **`src/views/binding.rs`** — add `active_severity`, rewrite `alarm_tint` over
   it, add `limit_bands` with the ordering and normal-band rules above. Unit
   tests in the existing `mod tests`: ordering, the both-sides-required rule for
   the normal band, and the empty-store case.
2. **`src/theme.rs`** — add `alarm_band` and `normal_band`.
3. **`src/views/meter.rs`** — add `MeterStyle` (facet + serde, `#[repr(u8)]`,
   `Default = Fill`), the `style` field on `MeterConfig` and `Meter`, and
   `band_span` with tests (clamping, fully-outside → `None`, infinite ends,
   degenerate scale).
4. **`src/views/meter.rs`** — style branch in `render`: bands, pointer, badge,
   style-dependent default color. `to_config` carries `style`.
5. **`src/views/mod.rs`** — re-export `MeterStyle` beside `Meter, MeterConfig,
   Orientation` (`mod.rs:48`).
6. **Palette entries.** `panels.rs:1275` currently offers one "Meter" row through
   `instrument_wizard_rows`. Add a second row, "Analog Indicator", pointing at
   the same `"meter"` key with `MeterStyle::Analog` in the seeded config — the
   wizard's `make_config: fn(ScaleSeed) -> String` (`panels.rs:1538`) already
   allows two closures over one key. Mirror it in the dashboard add-widget rows.
   `suggested_scale` (`meter.rs:293`) already seeds the scale off the declared
   limits, so an analog meter opens with its bands inside the track.
7. **Tests.** Extend `panel_configs_round_trip_through_json`
   (`panels.rs:1704-1731`) to cover `style`, and confirm a blob without `style`
   still parses as `Fill`.

No change to `MeterPanel`, `WidgetKind::meter`, the registry entry, or
`TILE_LAYOUT_VERSION`.

## Open questions

- **Setpoint caret.** Survey #3 lists it as optional and #25
  (deviation-from-setpoint) owns the idea properly. A caret needs a *second*
  binding (`component` + `element`, another `spawn_scalar_stream`), which is the
  first thing to push `MeterConfig` past "one element, one scale". Proposal:
  leave it out of this plan and let #25 add `setpoint: Option<ElementRef>` to
  the shared binding once there is a second consumer (the gauge wants the same
  caret).
- **Should `Gauge` get the same treatment?** `GaugeStyle::Arc` with bands painted
  along the track is the round form of the same widget and reuses `band_span`
  directly (`gauge.rs:373-380` already maps limits through
  `meter::limit_position`). Cheap follow-on; not required for #3.
- **Band alpha on a thin horizontal meter.** A 10px-tall horizontal bar
  (`meter.rs:363`) at `alarm_band` alpha may read as mud. Might need the band to
  win the full track and the pointer to overhang it, rather than a wash.
- **Does `Fill` keep its ticks once bands exist?** Drawing bands under a colored
  fill in `Fill` style is possible, but the point of the split is that `Fill` is
  the "how full" instrument and `Analog` is the "where in the safe range" one.
  Keep them distinct unless operators ask otherwise.
