# 14 — Alarm shelving + latching out-of-limits view

Survey item 14 (`docs/plans/telemetry-viz-additions.md:109`). Owns the shared
alarm-store work that [06 — Annunciator panel](./06-annunciator-panel.md)
consumes: the latched-occurrence retention and the `AlarmState::point` query.
`src/alarms/latch.rs` (`TileState`) is introduced by plan 06 and used here.

## Summary

Give the alarm panel the two semantics every mature alarm console has and this
one lacks: a **latch**, so an alarm that raised and self-cleared before anyone
looked is still there to be dismissed, and **shelving**, ISA-18.2's
time-limited, visibly-expiring suppression of a known-noisy point. Both fold
into `AlarmState`; shelves are published as messages on the same path
`AlarmAck` already takes, so they are shared between clients and survive a
restart rather than living in one view.

## Reuse vs. new

**No new widget.** This is an upgrade to `AlarmView`
(`src/views/alarm_panel.rs`) and to the store behind it
(`src/alarms/mod.rs`). The panel already has the shape this needs: a mode enum
switched from the header (`AlarmListMode`, `:15`), a severity-chip header, an
`Ack`/`Ack all` affordance, and two row builders (`active_rows`,
`history_rows`). Shelving adds a third mode and a per-row affordance; latching
changes what `active_rows` is given, not how it renders.

`AlarmListMode` gains a `Shelved` variant and the header toggle becomes a
three-way `AlarmListMode::cycle()` (STYLE.md: cyclic transitions are `cycle()`,
not `next()`), replacing the inline two-way flip at `:144`.

## Design

### What the control system already does, and what it does not

Worth stating plainly, because it decides everything below (surveyed
2026-08-22 against `libs/metor-fsw-2/src/alarm/mod.rs`):

- The FSW **does latch** — `AlarmSpec::latching` holds `Clear` until the ack
  arrives — and it **does consume `AlarmAck`**, uplinked because
  `LinkInfo::command_ids` advertises it (`libs/db/src/remote/fsw.rs:102`). For a
  latching alarm the round trip is already complete.
- But `AlarmSpec::to_def` **drops the `latching` flag**, so the panel cannot
  tell a latching alarm from a non-latching one, and an ack on the latter is
  inert FSW-side.
- For a **non-latching** alarm, a raise/clear pair that straddles a moment
  nobody was watching leaves no trace in the active list. That is the gap this
  plan closes, client-side.
- There is **no suppression concept anywhere in the FSW** — no inhibit, no
  disable, no command message shaped like one. The entire uplink surface is
  `SequenceCommand`, `ReloadSequences`, `AlarmAck`.

**So: shelving does not round-trip to the FSW, and should not pretend to.** It
is an operator-console function (which is what ISA-18.2 describes), and the
alarm keeps firing at the source while shelved. It is nevertheless *not* view
state: an operator shelving a chattering point wants every console to agree and
wants it to still be shelved after a panel restart. Publishing it as a message
into the db gets both for free — `ingest_all` (`src/msg_ingest.rs`) backfills
the persisted log before live-tailing, so a shelf replays on startup like any
other alarm event, and the FSW simply never subscribes.

### Latching in `AlarmState`

`apply_cleared` (`src/alarms/mod.rs:102`) currently drops the occurrence and its
ack. Instead:

- cleared **and already acked** → drop, as today (the operator has dismissed it);
- cleared **while unacked** → move into `latched: HashMap<AlarmId, LatchedAlarm>`,
  where `LatchedAlarm` is an `ActiveAlarm` plus `cleared_at`.

Keyed by `AlarmId`, not `OccurrenceId`: one pending latch per alarm point, so a
chattering alarm replaces its own entry instead of growing the map without
bound. `apply_ack` retires the latch. Nothing else can.

New/changed queries:

- `pending_sorted() -> Vec<PendingAlarm>` — active plus latched, each tagged
  with its `TileState` from `src/alarms/latch.rs`. This is what the panel
  renders; `active_sorted()` stays live-only for callers that mean "firing now".
- `point(&AlarmId) -> AlarmPoint { state, since, severity, value, occurrence }` —
  the per-def rollup the annunciator's alarm source reads (plan 06 step 8).
- `defs_iter()` — glob matching over declared alarms, also for plan 06.
- `unacked_count()` and `counts_by_severity()` count latched entries: "keep
  visible until dismissed" is the whole point, and these two feed the titlebar
  summary (`src/app.rs:551`).
- `active_severity_for` and `limits_for` (`:167`, `:177`) **stay live-only**.
  They drive plot tinting and limit lines; a cleared alarm must not tint a plot
  until someone acks it.

Two related fixes fall out while in here:

- **Escalation re-annunciates.** The FSW reuses the same `OccurrenceId` when an
  alarm escalates to a worse band. `apply_raised` overwrites the `ActiveAlarm`
  but leaves the occurrence in `acked`, so an acked warning that escalates to
  critical stays silently acked. It should be removed from `acked` when the
  incoming severity is higher than the stored one.
- `AlarmEventKind` gains `Shelved` and `Unshelved`, so the History tab is the
  shelving audit trail ISA-18.2 asks for without a second log.

### Shelving

```rust
pub struct Shelf {
    pub until: Timestamp,
    pub reason: Option<String>,
    pub operator: String,
    pub shelved_at: Timestamp,
    pub severity_at_shelve: Option<Severity>,
}
```

in `AlarmState::shelves: HashMap<AlarmId, Shelf>`. Rules:

- `pending_sorted()` and `counts_by_severity()` skip a shelved def; the alarm is
  still folded, still in history, and still visible on the Shelved tab with a
  live countdown. Nothing is silently dropped.
- **Expiry is evaluated at query time** against `Timestamp::now()`, not by a
  removal task — a query that lies for up to a tick is worse than a map that
  holds a stale entry. `AlarmStore` runs a 1 Hz `cx.notify()` ticker *only while
  `shelves` is non-empty*, so the countdown and re-appearance repaint. (The same
  ticker incidentally fixes `format_age`, which today only refreshes when some
  other alarm event arrives.)
- **Escalation defeats a shelf**: `apply_raised` for a shelved def at a severity
  above `severity_at_shelve` removes the shelf. Shelving must never mask an
  escalation, and this is a rule rather than an option.
- `MAX_SHELF_DURATION` const (8 h) with no "forever" option, per ISA-18.2. A
  point that needs permanent suppression needs a config change, not a shelf.

### Wire messages

Two new types in `libs/metor-proto/wkt/src/msgs.rs`, next to `AlarmAck`:

```rust
pub struct AlarmShelved { pub def_id: AlarmId, pub until: Timestamp,
                          pub reason: Option<String>, pub operator: String }
pub struct AlarmUnshelved { pub def_id: AlarmId, pub operator: String }
```

Ids come free from the blanket `impl Msg` (fnv1a of the schema name,
`libs/metor-proto/src/types.rs:600`) — no hand-allocation, just add them to the
`alarm_ids_are_pinned` pin test (`msgs.rs:1402`) and to the uniqueness sweeps
that enumerate the alarm ids. They are *not* added to any FSW `command_ids`, so
they never reach the uplink.

`AlarmStore::shelve(def_id, until, reason)` / `unshelve(def_id)` mirror
`acknowledge` (`:317`) exactly: postcard-encode, `db.push_msg`, and let the
store's own reader fold the result — fire-and-forget, so every client converges
through the same path.

### Panel UI

- **Header**: the mode chip cycles Active → History → Shelved. The Shelved chip
  carries a count so a shelved point is never invisible.
- **Active list**: latched rows render with the existing severity bar dimmed
  (`alarm_tint` instead of `alarm_color`) plus a "cleared" badge and the
  clear age; `Ack` retires them. Sorted after live rows of equal severity.
- **Row affordance**: a `Shelve` chip next to `Ack`. Clicking opens an anchored
  inspector page (`InspectorRequest` + `OpenInspectorCallback`, as
  `src/tiles/panels.rs` does) with duration rows — 15 m / 1 h / 8 h — and an
  optional reason via `TextField`. Not a right-click menu: right-mouse drag is
  unreliable on macOS trackpads, and content-area right-click is reserved.
- **Shelved tab**: def name, remaining time, reason, operator, `Unshelve`.
- `AlarmPanelConfig` (`src/tiles/panels.rs:79`) gains
  `mode: Option<AlarmListMode>`; the existing `show_history: bool` is kept as the
  fallback when `mode` is absent, so saved layouts restore unchanged.

## Implementation steps

1. **`AlarmState` latching** — `latched` map, `apply_cleared`/`apply_ack`
   changes, `pending_sorted`, `point`, `defs_iter`, count changes, escalation
   un-ack fix. Pure, so it lands with tests first.
2. **Update `src/alarms/tests.rs`** — `raise_then_clear_removes_active` (`:42`)
   asserts `unacked_count() == 0` after a clear and now expects `1`; that is the
   feature, not a regression. Add: clear-after-ack drops entirely; a second
   occurrence of the same def replaces its latch; escalation clears the ack.
3. **wkt messages** — `AlarmShelved`/`AlarmUnshelved`, roundtrip test, pin test,
   uniqueness sweep entries.
4. **Store shelving** — `shelves` map, fold via two new `IngestSource`s in
   `AlarmStore::new` (`:272`), listed after `AlarmRaised` so equal-timestamp
   records merge cause-before-effect; `shelve`/`unshelve` publishers; the
   expiry-aware queries; `MAX_SHELF_DURATION`; the escalation override. Tests
   for expiry-at-query-time and escalation-defeats-shelf.
5. **Notify ticker** in `AlarmStore`, live only while `shelves` is non-empty.
6. **`AlarmView`** — `AlarmListMode::Shelved` + `cycle()`, latched row styling,
   `Shelve` chip and its anchored duration page, `shelved_rows`, a
   `format_remaining` beside `format_age` (`:53`).
7. **`AlarmPanelConfig.mode`** plus its round-trip test in `panels.rs`.
8. **Titlebar** — confirm `render_alarm_summary` (`src/app.rs:551`) reads the
   new counts and that a shelved alarm does not light it.

## Open questions

- **Should the FSW learn about shelves?** Not needed for the display semantics,
  and adding an uplink command would be the first "operator suppresses the
  control system" surface in the system — a safety decision, not a UI one.
  Deliberately out of scope; revisit only with a real request.
- **`latching` is invisible on the wire.** Until `AlarmDef` carries it, the panel
  cannot label "ack required" and cannot warn that an ack on a non-latching
  alarm is inert. Adding a trailing field breaks postcard decode of persisted
  `AlarmDefs` records (they would fail to deserialize and be silently dropped by
  `IngestSource`), so this needs a compat plan — probably a new `AlarmDefV2`
  rather than a field append. Shared with plan 06.
- **Shelf granularity is the def**, matching COSMOS's "ignore item". An alarm
  whose target is a whole component (`element_index: None`) can only be shelved
  wholesale. Fine today; revisit if defs get coarser.
- **Latch policy is store-wide.** Every non-acked clear latches. The alternative
  is per-def opt-in, which needs the wire change above. If latch fatigue shows
  up before that lands, the escape hatch is a panel setting, not per-def config.
- **A late joiner sees no active alarms** (only `AlarmDefs` is retained on the
  link; `AlarmRaised` is `Delivery::Log`), so the latched set is only as good as
  the local db's history for that FSW run. A retained active-set snapshot from
  `AlarmSystem` would fix it for both plans.

## Status

All eight steps landed 2026-08-22, together with plan 06's deferred step 8.

- `AlarmState` gained `latched`/`shelves`, `PendingAlarm`/`AlarmPoint`/`Shelf`/
  `LatchedAlarm`, and the queries `pending_sorted`, `point`, `defs_iter`,
  `def_of`, `shelves_sorted`, `pending_count`, `highest_pending_severity`.
  `active_sorted`, `active_severity_for` and `limits_for` stayed live-only.
  Escalation now drops the ack and defeats a shelf taken at a milder band.
- `AlarmShelved`/`AlarmUnshelved` are in `libs/metor-proto/wkt`, ids `[83,33]` /
  `[200,230]` from the blanket `impl Msg`, pinned and swept for uniqueness. They
  are in no `command_ids`: the FSW is untouched.
- `AlarmStore` folds both through `IngestSource`s listed after `AlarmRaised`, and
  publishes through `shelve`/`unshelve` beside `acknowledge`. `acknowledge` now
  resolves latched occurrences too, so `Ack` retires a latch.
- The panel cycles Active → History → Shelved, dims the severity bar of a latched
  row and badges it "cleared", offers `Shelve` (15 m / 1 h / 8 h, optional reason
  typed into the anchored page's search field), and lists shelves with a live
  countdown and `Unshelve`.

Deviations worth knowing:

- **The 1 Hz ticker is always alive**, notifying only while `shelves` is
  non-empty, rather than being spawned and cancelled around a non-empty map. The
  fold path runs inside `IngestSource` closures, which get `&mut Self` and no
  gpui `Context`, so there is nowhere to arm the task from when a shelf arrives
  from another client. One idle timer wake a second, zero repaints.
- **The shelved count is its own header chip**, not a count on the mode chip: the
  mode chip names the tab you are on, so a count there would be invisible from
  Active and History — exactly what the rule was meant to prevent. The chip also
  jumps to the shelf list.
- **The titlebar reads `N pending, M unacked`.** `active_count` and
  `highest_active_severity` were replaced by `pending_count` and
  `highest_pending_severity` rather than kept alongside them, since nothing else
  called the live-only spellings.
- `DefaultActionRow` grew `new`/`optional` constructors (fields now private) so a
  shelf-duration row runs on a bare click instead of opening an inline editor for
  a reason the operator did not ask to type. Its seven existing call sites moved
  to `::new`.
