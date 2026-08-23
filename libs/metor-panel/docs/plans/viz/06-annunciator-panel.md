# 06 — Annunciator panel

Survey item 6 (`docs/plans/telemetry-viz-additions.md:66`). Shares the latch
state machine and the alarm-store queries with
[14 — Alarm shelving + latching](./14-alarm-shelving-latching.md); read that
plan's "Latching" section first if implementing both.

## Summary

Turn the traffic-light grid into a proper ISA-18.1 annunciator: named tiles in a
fixed grid, coloured by condition, with an optional latch (a transient violation
stays lit until acknowledged) and a first-out ring on the tile that tripped
first. Tiles can be sourced either from a component glob (today's behaviour,
and the primary path for the `*.healthy` idiom — systems exposing a boolean
healthy flag — via a polarity field) or from the control system's declared
alarm set, in which case colour, latch, and acknowledge all come from the
shared alarm store instead of being invented locally.

## Reuse vs. new

**Repurpose `TrafficLightGrid`, renamed to `Annunciator`.** No new widget.

The existing grid (`src/views/traffic_light_grid.rs`) already owns everything
structurally hard about this view: the glob compile-and-reconcile loop against
`db.vtable_gen` (`spawn_watcher`, `:242`), the per-cell stream tasks
(`build_cell`, `:279`), the "inspector wrote the field directly, recompile at
render time" pattern (`:182`), and the dual registration on both surfaces
(`TrafficLightGridPanel` in `src/tiles/panels.rs:583`, `WidgetKind::traffic_light_grid()`
in `src/views/dashboard/widgets.rs:356`). What is missing is presentation
(labels, values, fixed columns) and semantics (latch, first-out, ack) — additive
in every case.

The rename is warranted: "traffic light" names a two-state swatch, and the
broadened widget is a four-state named annunciator. `TrafficLight`
(`src/views/traffic_light.rs`) keeps its name and stays as-is — a single bound
swatch that toggles a bool is a genuinely different, smaller thing.

**Persisted keys do not change identity lightly.** The tiles
`serialization_key()` and the dashboard `WidgetKind` string are on-disk ids
(`docs/plans/widget-kind-registry.md`, "Risks"). New instances write
`"annunciator"`; `"traffic_light_grid"` is registered as an alias so saved
layouts keep loading — `ItemRegistry::register_erased`
(`src/tiles/serial.rs:73`) takes an arbitrary key, and `WidgetRegistry::register`
can take the same `Arc<WidgetSpec>` twice.

## Design

### Where latch state lives — split by authority

Two sources, two homes, one state machine:

- **Component-sourced tiles** watch a boolean the panel itself derives
  (`binding::any_on`). Nothing declared it an alarm, no ack is published, and no
  other client has an opinion. Latch state therefore lives **per view**, in the
  tile struct, and is **not persisted** — restoring a latch from a layout file
  would assert a trip that may never have happened in this session.
- **Alarm-sourced tiles** reflect declared alarms. Ack is already a wire event
  (`AlarmAck`, published by `AlarmStore::acknowledge`, `src/alarms/mod.rs:317`,
  and consumed by the FSW's `AlarmSystem`), so the latch must be shared: it lives
  in `AlarmState` (plan 14) and every annunciator, the alarm panel, and the
  titlebar summary read the same answer.

The transition rules are identical, so they are written once as a pure type in
`src/alarms/latch.rs`:

```rust
/// ISA-18.1 sequence-A annunciator states.
pub enum TileState { Normal, AlarmUnacked, AlarmAcked, ClearedUnacked }

pub struct Latch { active: bool, acked: bool, since: Option<Timestamp> }
```

`Latch::condition(active, now)` on a rising edge sets `since` and drops `acked`;
on a falling edge it keeps `since` while unacked (that is `ClearedUnacked`, the
ringback state). `Latch::ack()` sets `acked` and returns to `Normal` if the
condition already went away. `Latch::state(latching: bool)` collapses to
`Normal`/`AlarmUnacked` when latching is off, which is exactly today's grid.

**First-out** is a display rule over the tiles one annunciator shows — the grid
*is* the trip group — so it needs no stored flag: the non-`Normal` tile with the
smallest `since` gets the ring, recomputed per render. Same rule for both
sources (`ActiveAlarm::raised_at` supplies `since` for alarm tiles).

### Config

`AnnunciatorConfig` (renamed `TrafficLightGridConfig`, still shared verbatim by
both surfaces via the `pub use` aliases at `panels.rs:579` and `widgets.rs:489`):

```rust
pub struct AnnunciatorConfig {
    pub pattern: String,              // unchanged
    pub color: Option<Hsla>,          // unchanged: fill for component tiles with no alarm def
    pub source: AnnunciatorSource,    // Components (default) | Alarms
    pub alarm_when: AlarmWhen,        // On (default) | Off — Off for *.healthy-style flags
    pub show_labels: bool,
    pub show_values: bool,
    pub latch: bool,
    pub columns: usize,               // 0 = wrap (today); N = fixed N-wide grid
}
```

Every new field is `#[serde(default)]` and defaults to the current behaviour, so
an existing `{pattern, color}` blob renders pixel-identically. The palette
wizard writes `show_labels: true, columns: 4` for newly created annunciators —
a deliberate difference between the serde default (back-compat) and the
construction default (the useful widget).

`AnnunciatorSource::Alarms` matches the glob against `AlarmDef::name` rather
than component names; the tile then needs no stream task at all, only a
`cx.observe` on the store (as `AlarmView::new` does, `src/views/alarm_panel.rs:31`).

### Health-flag polarity

Component-sourced annunciators are not a legacy mode — the `*.healthy` idiom
(each system exposing a boolean healthy flag) is a primary use case and must
read correctly under the new state table. The tile *condition* is the alarm-ness
of the value, not its truthiness:

```rust
pub enum AlarmWhen { On, Off }

let in_alarm = any_on(view) ^ (config.alarm_when == AlarmWhen::Off);
```

`AlarmWhen::On` (the serde default) preserves today's fault-flag semantics: a
truthy value lights the tile. `AlarmWhen::Off` inverts it for healthy flags:
`healthy == true` renders `Normal` grey, `false`/zero goes to `AlarmUnacked` —
so a wall of healthy systems is calm and the one sick system is the only
saturated thing on screen, which is the HPHMI point. The "no sample yet" row of
the state table matters doubly here: a healthy flag that has never reported is
*not* healthy, and stays dimmed `text_tertiary`, never `Normal` grey. The field
only applies to `AnnunciatorSource::Components` (alarm-sourced tiles get their
condition from the occurrence) and is hidden from the inspector when the source
is `Alarms`.

**Labels strip the glob's literal parts.** With `pattern = "*.healthy"` every
matched name ends in `.healthy`; rendering "eps.healthy / adcs.healthy / …"
wastes the label on the one substring that carries no information. The tile
label is the text matched by the pattern's wildcards — the literal prefix and
suffix of the glob are removed (`*.healthy` → `eps`, `adcs`; `gnc.*` →
`wheels.temp`). A pattern with no wildcard, or a stripped label that would be
empty, falls back to the full name. The full component name stays in the
tooltip (`TooltipText`) either way.

### Tile rendering

The tile is a fill-coloured box, not a swatch inside a box, so it does not reuse
`traffic_light_swatch` (which stays with `TrafficLight`). Fill by state, all from
`theme.rs` — no new theme entries, and the HPHMI colour budget the survey calls
out (`telemetry-viz-additions.md:183`) is respected because `Normal` is grey:

| state | fill | label |
|---|---|---|
| `Normal` | `bg_secondary` | `text_secondary` |
| `AlarmUnacked` | `alarm_color(sev)` | `text_primary` |
| `AlarmAcked` | `alarm_tint(sev)` + `alarm_color` border | `text_primary` |
| `ClearedUnacked` | `bg_secondary` + `alarm_color` border | `alarm_color(sev)` |
| no sample yet | `bg_secondary`, dimmed label | `text_tertiary` |

Severity index comes from `AlarmState::active_severity_for` for component tiles
that happen to be an alarm target — via `binding::active_severity`, the split-out
helper plan 03 adds beside `alarm_tint` — and from the occurrence otherwise; a
component with no alarm def falls back to the configured `color` at index 1.
That is the survey's "limits are data, not chart config" principle applied to
tile colour.

First-out adds a `border_2` ring in `theme.control_active` and a `"1st"` badge.
Value text uses `views::format::format_number` with the component's unit from
`ComponentMeta`.

### Interaction

- `latch == false`: click toggles a bool component (today's `toggle_cell`,
  `:153`) — unchanged, but only for `AlarmWhen::On`. A healthy-flag tile
  (`AlarmWhen::Off`) is a report from the producing system, not a control;
  clicking it does nothing while unlatched.
- `latch == true`: click **acknowledges** the tile. A latching annunciator is a
  monitoring surface, not a control; toggling is not offered. This is
  back-compatible because `latch` defaults off.
- Alarm-sourced tiles always ack on click, via `AlarmStore::acknowledge(occurrence)`.
- A header strip gains an "Ack" chip acking every non-`Normal` tile *in this
  annunciator* (not the global `acknowledge_all`).

Note the FSW only acts on an ack for alarms configured `latching = true`, and
`AlarmDef` does not carry that flag — see Open questions.

## Implementation steps

1. **`src/alarms/latch.rs`** — new file: `TileState`, `Latch`, `pub mod latch;`
   in `src/alarms/mod.rs`. Unit-test the four transitions inline
   (`#[cfg(test)] mod tests`, as `binding.rs` does): rise, fall-while-unacked,
   ack-while-active, ack-while-cleared, and `latching = false` collapsing to two
   states. No gpui, no DB — same discipline as `AlarmState`.
2. **`binding.rs`: carry the value with the on/off bit.** `spawn_on_stream`
   (`src/views/binding.rs:286`) discards everything but `any_on`. Widen its
   decode to yield `(bool, Option<f64>)` and update both call sites
   (`TrafficLight`, which ignores the value, and the annunciator). One helper,
   not two near-copies.
3. **Rename, mechanically.** `git mv src/views/traffic_light_grid.rs
   src/views/annunciator.rs`; `TrafficLightGrid` → `Annunciator`,
   `TrafficLightGridConfig` → `AnnunciatorConfig`, `GridCell` → `Tile`,
   `TrafficLightGridPanel` → `AnnunciatorPanel`,
   `traffic_light_grid_pattern_rows` → `annunciator_pattern_rows`
   (`panels.rs:1508`). Touches `views/mod.rs:30,62`,
   `inspector/registry/defaults.rs:39`, `widgets.rs:23,357,489,554,794`,
   `panels.rs:21,579-650,1265`, `views/dashboard/mod.rs` add-flow labels.
   Display strings become "Annunciator".
4. **Keep old layouts loading.** `serialization_key()` returns `"annunciator"`;
   in `register_pane_item_deserializers` (`src/app.rs:1292`) add a
   `reg.register_erased("traffic_light_grid", …)` delegating to the same
   builder. Register the dashboard spec under both `WidgetKind` strings.
5. **Grow the config and the inspector surface.** New fields plus their facet
   attributes on `Annunciator` (`inspect::range(min=0,max=12)` on `columns`,
   `inspect::variants` on `source` and `alarm_when`). Extend the render-time
   rebind check (`:182`) to also compare a `bound_source`, so flipping the
   source in the inspector rebuilds the tile set the same frame. `alarm_when`
   feeds the condition at the stream callback, not at render, so a latch sees
   the inverted edge.
6. **Tile rendering**: labels, values, and the `columns` layout (chunk tiles into
   `flex_row`s of `columns`, `flex_1` each; `0` keeps `flex_wrap`). Wire the
   state→fill table above. Label derivation: a small pure
   `strip_glob_literals(pattern, name)` helper beside the regex compile, unit
   tested (`*.healthy`, `gnc.*`, no-wildcard, empty-strip fallback).
7. **Latch + first-out for component tiles**: a `Latch` per `Tile`, driven from
   the widened stream callback; the ack affordance; the min-`since` ring.
8. **Alarm source** — depends on plan 14 step 1 (`AlarmState::point`). Build
   tiles from `AlarmState::defs_iter()` filtered by the glob, `cx.observe` the
   store, ack through `AlarmStore::acknowledge`.
9. **Tests**: extend the JSON round-trip test in `panels.rs`
   (`panel_configs_round_trip_through_json`, `:1563`) with the new fields, and
   add a case asserting a legacy `{"pattern":"*.health"}` blob deserializes with
   every new field at its back-compat default (`alarm_when: On` included, so old
   grids keep lighting on truthy). Latch tests in `latch.rs` cover both
   polarities by feeding the pre-inverted condition.

## Open questions

- **Flashing.** ISA-18.1 flashes the unacked state. gpui can animate, but a
  flashing grid is hostile on a dense dashboard. Proposed: no flash, distinguish
  unacked by full saturation. Revisit if operators ask.
- **`AlarmDef` cannot say whether an alarm latches.** `AlarmSpec::latching`
  exists FSW-side (`libs/metor-fsw-2/src/alarm/mod.rs`) and gates whether an ack
  does anything, but `AlarmSpec::to_def` drops it. Adding the field is a
  trailing-field postcard change that breaks decode of already-persisted
  `AlarmDefs` records (`IngestSource` would silently drop them). Worth doing, but
  needs a compat decision — shared with plan 14.
- **A late-joining panel never learns about already-active alarms.** Only
  `AlarmDefs` is retained on the link; `AlarmRaised` is `Delivery::Log`, so
  alarm-sourced tiles read `Normal` for a real, ongoing alarm whenever the local
  db has no history for that FSW run. Should `AlarmSystem` publish a retained
  active-set snapshot? Until then the annunciator should render "no data" rather
  than "normal" for a def it has never seen an event for — but the two are
  indistinguishable from the store today.
- **A boot-disabled alarm still ships its def** with no marker
  (`AlarmRuntime::enabled` is boot-only and invisible on the wire), so an
  alarm-sourced annunciator shows a permanently dark tile that can never trip.
  Needs an `enabled` bit on the def or a health-derived marker.
- **Should `TrafficLight` fold into the annunciator** as a one-tile instance
  later? Kept separate for now; revisit if the annunciator's tile chrome makes
  the single widget redundant.

## Status

Steps 1–7 and 9 landed 2026-08-22. `src/alarms/latch.rs` holds the shared
`TileState`/`Latch`; `src/views/traffic_light_grid.rs` became
`src/views/annunciator.rs` with the config, tile rendering, polarity, latch,
ack, and first-out work above. `"traffic_light_grid"` survives as a pane-item
alias (`src/app.rs`) and as a second `WidgetKind` registration for the same
`WidgetSpec`, so pre-rename layouts and the Python dashboard builder keep
resolving.

**Step 8 (alarm-sourced tiles) and the `source` / `AnnunciatorSource` config
field are deferred to [14 — Alarm shelving + latching](./14-alarm-shelving-latching.md).**
They depend on `AlarmState::point`/`defs_iter`, and the crate forbids config
that nothing reads, so the shipped `AnnunciatorConfig` is `pattern`, `color`,
`alarm_when`, `show_labels`, `show_values`, `latch`, `columns`. `alarm_when`
is therefore always live rather than hidden behind an `Alarms` source.
