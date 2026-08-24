# 07 — Alarm coloring in tables and value strips

Item 7 of `docs/plans/telemetry-viz-additions.md`. Companion plan:
`08-staleness-indication.md` — both add shared per-value state to the same
rendering surface, and the two treatments must not collide (see *Interaction
with staleness*).

## Summary

Today only plots and the meter/gauge consult the alarm store; every alphanumeric
readout in the panel paints `text_primary` whether the value is nominal or in
alarm. Wire `ComponentValueStrip` — the one widget behind the component table,
data table, component browser, dashboard Monitor, and browser detail pane — to
the same lookup `views/binding.rs` already uses for instruments, so an
out-of-limit number reads as abnormal on every surface at once. No new widget,
no new config, nothing new persisted: limits and states stay data owned by the
control system.

## Reuse vs. new

- **Extended, not added.** All coloring lands inside
  `ComponentValueStrip::render` (`src/views/value_strip.rs:501`) and its two
  chrome builders (`build_cell_chrome:773`, `build_editing_chrome:712`). The
  five hosts (`component_table.rs:216`, `data_table/grid.rs:198`,
  `component_browser/mod.rs:770`, `monitor.rs:87`, and the browser detail row at
  `component_browser/mod.rs:1018`) need **no changes** — they already delegate
  every cell to a strip.
- **`binding::alarm_tint` generalizes.** `alarm_tint(ElementRef, &App)`
  (`src/views/binding.rs:350`) resolves one element at a time. It becomes a thin
  wrapper over a new `binding::element_alarms(ComponentId, &App) ->
  ElementAlarms`, which answers a whole strip's worth of cells from one store
  read. Meter and gauge (`meter.rs:331`, `gauge.rs:371`) keep their current call
  unchanged. Plan 03 (moving analog indicator) proposes the same split under the
  name `active_severity` — that is one function, not two: `element_alarms` is the
  whole-component form, `active_severity(at)` its single-element convenience.
- **`AlarmState` gains an index, not an API surface.** `active_severity_for`
  (`src/alarms/mod.rs:177`) scans every active occurrence and re-checks
  `targets` (`:193`) against the def map — fine for a handful of plot traces,
  quadratic for a table with several hundred cells per frame. Add a derived
  `HashMap<ComponentId, _>` index rebuilt on def/raise/clear (all rare), and
  reimplement `active_severity_for` over it so the existing plot caller
  (`views/time_series/mod.rs:564`) gets the same speedup for free.
- **No new theme colors.** `Theme::alarm_color` / `Theme::alarm_tint`
  (`src/theme.rs:168`, `:177`) already encode the severity palette; the
  background-tint-plus-colored-text pairing is the one `alarm_panel.rs:102`
  established.
- **Renames:** none required.

## Design

**Source of truth.** The panel never decides whether a value is in alarm — the
control system raises and clears, the store folds
(`src/alarms/mod.rs` module docs). Cell color therefore keys off
`active_severity_for`, exactly like the plot's out-of-bounds tint, *not* off a
panel-side comparison of the value against `limits_for`. A def with
`element_index: None` targets the whole component, so every cell of that strip
colors together; a def with an index colors one cell.

**Lookup shape.** `ElementAlarms` is a copy-cheap resolved snapshot:

```rust
/// Active alarm severity per element of one component, resolved once per
/// strip per frame. `None` for a component nothing targets — the common
/// case, answered by a single hash lookup.
pub(crate) struct ElementAlarms { whole: Option<usize>, per_element: … }
impl ElementAlarms { pub fn severity(&self, element: usize) -> Option<usize>; }
```

Severity is carried as the `severity_index` (`src/alarms/mod.rs:232`) so the
render path indexes the theme directly and never re-imports `Severity`.

**Cell treatment (HPHMI color budget).** Gray-scale ground stays the default;
saturated color appears only on the abnormal cell.

| state | Boxes and Dashboard presets |
| --- | --- |
| nominal | unchanged — `bg_secondary` (Boxes) / no bg (Dashboard), `text_primary` |
| in alarm | cell bg `theme.alarm_tint(sev)`, value text `theme.alarm_color(sev)` |
| in alarm, bool cell | toggle track keeps `control_active_track`; add a 1px `alarm_color(sev)` border |
| pending edit / editing | unchanged `drop_target` chrome — the operator's own transient state outranks the alarm |

The Dashboard preset gets the tint background too, even though it is otherwise
background-free: "color only when abnormal" is precisely the case the
background-free rule should yield to, and a Monitor tile is often the only place
a value appears.

The bool exception exists because a bool cell is a *control* (it toggles), not a
readout — recoloring its track would read as a state change.

**Interaction with staleness (plan 08).** The two treatments are orthogonal by
construction: **alarm state colors the cell chrome, staleness governs the value
glyphs.** A cell that is both stale and in alarm keeps the alarm tint background
(the condition is still raised) while its number desaturates and strikes through
(the number itself is old). Neither plan may reuse the other's channel.

**Repaint.** `ComponentValueStrip::new` adds
`cx.observe(&alarms::try_global(cx)?, |_, _, cx| cx.notify())`, the pattern used
by `AlarmView::new` (`src/views/alarm_panel.rs:30`) and the status bar
(`src/app.rs:103`). One observer per strip is fine: the store notifies only on
raise/clear/ack/def, never per sample.

## Implementation steps

1. **`src/alarms/mod.rs`** — add a private index to `AlarmState`
   (`HashMap<ComponentId, ComponentAlarms>` holding a whole-component severity
   plus a per-element map) and a private `reindex()` called at the tail of
   `apply_def`, `apply_raised`, and `apply_cleared`. Add
   `pub fn element_severities(&self, ComponentId) -> Option<&ComponentAlarms>`
   and reimplement `active_severity_for` over it. `limits_for` stays as-is —
   only plots and instrument scales call it, and never per cell.
2. **`src/alarms/tests.rs`** — extend the existing target tests (`:156`, `:172`)
   to cover the index: element-scoped def colors one element, component-scoped
   def colors all, clearing the last occurrence drops the entry, highest
   severity wins when two defs overlap.
3. **`src/views/binding.rs`** — add `ElementAlarms` + `element_alarms(component,
   cx)`; rewrite `alarm_tint` (`:350`) as a two-line wrapper over it. Update the
   module header's "Limits" paragraph (`:19-22`) to say the same store now also
   colors tables.
4. **`src/views/value_strip.rs`** — resolve `element_alarms(self.component_id,
   cx)` once at the top of `render` (`:501`); thread the per-cell
   `Option<usize>` into `build_cell_chrome` and `build_editing_chrome`. Add a
   pure helper `fn cell_alarm_style(severity: Option<usize>, is_pending: bool,
   preset: StripPreset, theme: &Theme) -> (Option<Hsla>, Hsla)` returning
   (background, text) so the precedence table above is expressible as one
   testable function rather than branches scattered through the builder. Add the
   store observer in `new` (`:213`).
5. **Bool cells** — in the `is_bool` early-return branch (`:795`), apply the
   border case rather than the track color.
6. **Verify hosts unchanged** — build and eyeball `component_table`,
   `data_table`, `component_browser`, and a dashboard `Monitor`; no host file
   should appear in the diff.
7. **`src/views/value_strip.rs` tests** — unit-test `cell_alarm_style` for the
   four rows of the table against `DARK`. Strip construction needs a DB, so keep
   the test on the pure helper, matching the existing `format_element` tests.
8. **`STYLE.md`** — add the color-budget rule the survey converged on: the alarm
   palette (`alarm_color`/`alarm_tint`) is reserved for abnormal conditions and
   never used decoratively. This is the natural place to land it, and plan 08's
   `Theme::stale` depends on it being written down.

## Open questions

- **Acked alarms.** ISA-18.2 and COSMOS distinguish unacked (attention-getting)
  from acked (steady). `AlarmState::is_acked` (`:157`) is already there. Cheapest
  differentiation: acked keeps the tint background but drops back to
  `text_primary`. Worth doing now or deferring to item 14 (shelving/latching)?
- **Panel-side limit state.** COSMOS and SCOS compute a limit *state* client-side
  by comparing the value against the limit set, and color even when nothing was
  raised. That would make coloring work against a control system that publishes
  `AlarmDef`s but no `AlarmRaised` — at the cost of the panel deciding when an
  alarm fires, which the alarm module explicitly refuses. Leaving it out; revisit
  only if a real target ships defs without raises.
- **Other readouts.** `ComponentText` (`src/views/component_text.rs`) and
  `json_tree` render values outside the strip. Both are single-value surfaces
  where the same rule would apply cleanly; folding `ComponentText` onto the
  strip is arguably a better fix than teaching it the lookup separately.
- **String and enum cells.** A string component collapses to one unlabeled cell
  (`format.rs:89`), so an element-scoped def can't address it. Treat any def on
  a string component as whole-component — confirm no target relies otherwise.
