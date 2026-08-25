# 08 — Staleness indication

Item 8 of `docs/plans/telemetry-viz-additions.md`. Companion plan:
`07-table-alarm-coloring.md` — both add shared per-value state to the same
rendering surfaces, and the two treatments occupy deliberately separate visual
channels (see *Interaction with alarm coloring*).

## Summary

A view repaints when its stream's `next()` resolves and otherwise holds the last
frame forever, so a producer that dies leaves a confident-looking number on
screen indefinitely. Give every binding a stamped sample time and a per-component
threshold, and render an over-age value as *dead* — chroma drained, struck
through, age in the tooltip. The stamping, the threshold resolution, and the
wake-up timer all live in the one place every value view already goes through,
`spawn_seeded_stream` in `src/views/binding.rs:155`.

## Reuse vs. new

- **The timestamp already exists and is thrown away.** WAL frames are
  `[Timestamp][value]`; `WalComponentStream::next` (`src/lib.rs:159`) computes
  `offset = … + size_of::<Timestamp>()` and `WalView` (`:139`) exposes only the
  value. Add a defaulted method on the existing `AsComponentView` trait
  (`src/lib.rs:51`) rather than a parallel trait:
  `fn sample_time(&self) -> Option<Timestamp> { None }`, overridden by `WalView`
  to read the eight bytes at `offset - size_of::<Timestamp>()`. Every live
  stream in the crate is a `WalComponentStream` — DB components and dynamic
  nodes alike (`src/dynamic/mod.rs:38`) — so one override covers everything.
- **The seed path already has it too.** `spawn_seeded_stream` reads
  `component.time_series.latest()` (`binding.rs:183`); that returns a
  `TimestampRef` (`libs/db/src/time_series_2.rs:958`) whose `timestamp()`
  (`:326`) is currently unused.
- **The stream task becomes the timer.** No global clock entity, no per-view
  animation. The task each binding already owns races `stream.next()` against
  `cx.background_executor().timer(remaining)` (`futures-lite` is already a
  dependency; `background_executor().timer` is the crate's established idiom —
  `component_table.rs:139`, `traffic_light_grid.rs:254`).
- **`Freshness` is new; the spawn helpers' return type changes.** Each helper
  returns a `Binding` (task + freshness) instead of a bare `gpui::Task<()>`, so
  every streaming view *has* freshness whether or not it renders it — a general
  defaulted capability on the standard path rather than an opt-in bolted onto
  the views that happen to want it.
- **Threshold reuses the metadata map.** `ComponentMetadata` already exposes
  typed accessors over its `HashMap<String, String>`
  (`libs/metor-proto/wkt/src/metadata.rs:56`, `:77`). Add `stale_after()` /
  `with_stale_after()` beside them.
- **Renames:** `theme::FontSettings` (`src/theme.rs:890`) already holds the
  whole `PanelConfig`, not just the font, and this plan adds the second reader.
  Rename it `Settings` (fields `family`, `config`); `font_family(cx)` and
  `set_font(cx, …)` keep their names. Also move `alarm_panel.rs`'s private
  `format_age` (`:53`) to `src/views/format.rs` and share it.

## Design

**What "age" means.** Age is `Timestamp::now() - sample_time` in microseconds
(`Timestamp` is unix µs, `libs/metor-proto/src/types.rs:936`) — the age of the
*data*, not of its arrival, which is what NASA/COSMOS/Yamcs all report and what
stays correct across a replayed recording. A source that yields no timestamp
falls back to arrival time. A negative age (producer clock ahead) clamps to zero.

**`Freshness`** is a cloneable shared cell — the stream task is the only writer,
the render pass the only reader:

```rust
/// Wall-clock freshness of one binding's samples.
///
/// One per binding, not per view: the 3D viewer and dashboard connectors bind
/// several components to a single entity and each goes stale on its own.
#[derive(Clone, Default)]
pub(crate) struct Freshness(Arc<FreshnessCell>);   // last: AtomicI64, threshold: AtomicI64

impl Freshness {
    /// Age of the newest sample, or `None` before the first one arrives.
    pub fn age(&self) -> Option<Duration>;
    /// Newest sample older than this component's threshold. False before the
    /// first sample: an unbound view is blank, not stale.
    pub fn is_stale(&self) -> bool;
}

/// A live binding: the stream task plus the freshness it stamps. Views store
/// this where they stored a bare `Task`; dropping it cancels the stream.
pub(crate) struct Binding { _task: gpui::Task<()>, freshness: Freshness }
```

Both atomics are written by the task and read without a `&App`, so `is_stale`
and `age` are pure and unit-testable.

**Threshold resolution.** Per component from metadata key `stale_after`
(seconds, `0` disables); otherwise the global default
`PanelConfig::stale_after_secs` (`src/config.rs:37`, default `3.0`,
`#[facet(default)]` like its neighbors). The task resolves it after
`into_stream` returns — which is also when a late-registering component finally
appears — and re-resolves on each timeout, so metadata that lands later takes
effect without a respawn.

**Wake-up.** Exactly one timeout event per quiet period, and none in steady
state: after each sample the task waits on `next()` raced against a timer for
the remaining time; if the timer wins it stamps nothing, calls
`this.update(cx, |_, cx| cx.notify())` once, and then waits on `next()` alone
until data resumes. An idle panel costs zero wakeups.

Racing is safe: `Reader::next` (`libs/db/src/disruptor.rs:263`) parks on
`wait_for_value` and only materializes a `ReadGrant` on success; the reader
cursor advances in `ReadGrant::drop` (`:315`), so dropping a pending read loses
nothing.

**Rendering — desaturated, never a fourth alarm color.** Add a derived theme
helper beside `Theme::dim` (`src/theme.rs:186`):

```rust
/// A value color drained of chroma for data that stopped arriving. Staleness
/// reads as dead, never as another alarm severity.
pub fn stale(color: Hsla) -> Hsla { Hsla { s: 0.0, a: 0.5, ..color } }
```

Per surface, all keyed off `Freshness::is_stale()`:

- **Value strips** (`src/views/value_strip.rs:897`) — value text becomes
  `Theme::stale(base)` and gains `.line_through()` (gpui `Styled`,
  `gpui-0.2.1/src/styled.rs:518`); the label, unit, and cell background are
  untouched. A `TooltipText::build` (`src/views/tooltip.rs:16`) tooltip shows
  `format_age`. This one change covers the component table, data table,
  component browser, browser detail pane, and the dashboard Monitor.
- **Meter / gauge** (`meter.rs:322`, `gauge.rs:360`) — fill color becomes
  `Theme::stale(self.color)`; limit marks and the alarm tint keep their colors
  (they describe the *channel*, which has not gone stale — only the reading has).
- **Traffic light / state chip** (`traffic_light.rs`, `state_chip.rs`) — lamp
  color desaturated; the lit/unlit geometry is unchanged, so a stale lamp reads
  as "was on" rather than "off".
- **Attitude, 3D viewer, connectors** — out of scope for the first pass; they
  get `Freshness` from the shared `Binding` and can adopt the treatment later.

**No-data is not staleness.** Before the first sample a strip already renders
its placeholder in `text_tertiary` (`value_strip.rs:699`); `is_stale()` stays
false so nothing changes there.

**Interaction with alarm coloring (plan 07).** Alarm state colors the cell
*chrome* (background tint, border); staleness governs the value *glyphs* (color,
strikethrough). A cell that is both keeps the alarm tint and strikes the number.
Neither plan writes the other's channel.

## Implementation steps

1. **`src/lib.rs`** — add `AsComponentView::sample_time` with a `None` default;
   override in `impl AsComponentView for WalView` (`:145`) reading the frame's
   leading `Timestamp` (`Timestamp::from_le_bytes`).
2. **`libs/metor-proto/wkt/src/metadata.rs`** — `stale_after(&self) ->
   Option<Duration>` and `with_stale_after(Duration)`, beside `is_string` /
   `is_hidden`.
3. **`src/config.rs`** — `PanelConfig::stale_after_secs: f64` with
   `#[facet(default = …)]` and a hand-written `Default`; extend the round-trip
   test.
4. **`src/theme.rs`** — rename `FontSettings` to `Settings` (one call site each
   in `font_family`, `set_font`, `src/app.rs:1087`); add `Theme::stale`.
5. **`src/views/binding.rs`** — add `Freshness`, `Binding`, and a
   `stale_after(db, component, cx)` resolver. Rework `spawn_seeded_stream`
   (`:155`) to stamp the seed's `latest.timestamp()` and each sample's
   `sample_time()`, race the timer, and return a `Binding`. Extend the module
   header with a fourth bullet — Seeding, Late binding, Limits, **Freshness**.
6. **Propagate the return type** — `spawn_scalar_stream` (`:215`),
   `spawn_elements_stream` (`:249`), `spawn_on_stream` (`:286`) return
   `Binding`; update the field types at `meter.rs:127`/`:198`,
   `gauge.rs:128`/`:197`, `state_chip.rs:106`/`:171`, `attitude.rs:169`/`:299`,
   `traffic_light.rs:61`, `traffic_light_grid.rs:286`,
   `dashboard/connectors.rs:325`, `viewer_3d/mod.rs:675`,
   `value_strip.rs:222`, `component_text.rs:33`. Mechanical, no logic per site;
   no `StreamUpdate` variant is added, so every existing `apply` closure
   compiles untouched. Plan 06 widens `spawn_on_stream`'s `apply` to carry the
   value alongside the on/off bit — orthogonal to this change (argument type vs.
   return type); land whichever comes first and rebase the other.
7. **`src/views/format.rs`** — move `format_age` here from `alarm_panel.rs:53`
   and point the alarm panel at it.
8. **Render treatments** — strips first (`value_strip.rs:897` plus the tooltip),
   then meter/gauge/traffic-light/state-chip.
9. **Tests** — pure-unit coverage in `binding.rs`: never-stamped is not stale;
   a sample inside the threshold is fresh; one past it is stale; `stale_after =
   0` disables; a future-dated sample clamps to zero age. Plus a `format_age`
   test moved with the function.
10. **Manual check** — run the panel, kill the producer, confirm every surface
    flips within the threshold and recovers on the next sample.

## Open questions

- **Threshold source of record.** `stale_after` as free metadata means every
  producer must set it or inherit one global number. A better long-run answer is
  a declared publish rate (`rate_hz`) with the panel deriving a threshold as a
  multiple of the period — that is one value a target already knows. Worth
  designing now, or shipping `stale_after` and adding the derivation later?
- **Plots.** A stale trace shows a flat line running to the right edge, which is
  arguably self-evident — but Open MCT and Yamcs both badge the plot too. A
  "no data for 12s" chip in the plot chrome would reuse `Freshness`, but plots
  own their sampling and never call `spawn_seeded_stream`; it needs its own
  wiring. Deferred.
- **Instrument-level badge.** Desaturation alone may be too quiet on a wall
  display. A small dot in `Theme::stale(...)` (`Icon::Dot`, no new asset) beside
  the value is the cheap escalation if operators miss it.
- **Connection-level staleness.** When the whole link drops, every binding goes
  stale at once and the screen turns gray. The connections store
  (`src/connections/mod.rs`) already knows the link is down — a link-level banner
  probably should suppress or explain the per-value treatment rather than
  letting hundreds of cells report it independently.
