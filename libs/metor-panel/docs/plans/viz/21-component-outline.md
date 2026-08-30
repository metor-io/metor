# 21 — Component outline (collapsible tree-table)

## Summary

A pane that shows the whole component namespace as an outline: one row per
node of the dot-delimited tree, disclosure triangles on branches, live value
strips on components. Collapsed branches summarize their subtree (component
count now; staleness / alarm state as a follow-up). Columns: name, value,
unit, type, and an optional sparkline (off by default, toggled from the
pane's inspector page).

This is the structural view the data table tried to be. The data table
inferred "groups" from an opt-in `group_name` tag and rendered one group at a
time; the outline renders the tree the names already form, so every frame
shows up, nesting is arbitrary, and the user folds what they don't need.

## Reuse vs. new

Reused:
- `component_browser::component_tree::{build_tree, ComponentNode}` — the
  namespace tree with single-child chain compression.
- `component_browser::prune_to_matches` — filter-bar pruning.
- `Table` / `TableDelegate` — virtualized rows, resizable columns, hscroll.
- `VisibleEntityCache` — bounded live strips / sparklines for visible rows.
- `ComponentValueStrip`, `behavior_snapshot`, `edit_click`,
  `right_click_plot`, `shift_hover_listener` — the same value cell and
  gestures the browser's detail column has.
- `FilterBar` + `Query`; the `ToggleFilterBar` action with a new
  `ComponentOutline` key context.
- `JsonTree`'s disclosure model: one set of paths toggled away from a depth
  default.

New:
- `views/outline/model.rs` — `Disclosure` + `flatten(tree, disclosure,
  query) -> Vec<OutlineRow>` (pure, tested).
- `views/outline/mod.rs` — `ComponentOutline` (filter bar + table host) and
  `OutlineDelegate` (the `TableDelegate`).
- `OutlinePanel` / `OutlinePanelConfig` in `tiles/panels.rs`, key
  `component_outline`; palette entry; `app.rs` registration + Cmd-F binding;
  inspector rows (sparklines, filter bar) in `registry/defaults.rs`.

## Design

- Default disclosure: depth-0 branches open, everything beneath folded.
  Click a branch row's name to toggle it; alt-click toggles its whole
  subtree.
- While the filter bar has a query the outline shows the pruned tree fully
  expanded; disclosure state is kept but ignored until the query clears.
- Branch rows: segment name, `N components` in the value column. A branch
  that is itself a component (both `component_id` and children) shows its
  strip instead.
- Leaf rows: segment name (shift-hover previews a plot), value strip
  (click edits, right-click plots an element), unit from metadata, type as
  `f64` / `f32[3]` / `u8[4×4]` from the schema.
- Sparkline column exists only when enabled; row height grows to fit it.
- Persisted: filter text, bar visibility, sparklines flag, toggled paths.

## Pivot (landed)

Right-click a branch → **Pivot**. Its branch children become instance rows
and the union of their leaf paths (relative, e.g. `motor.temp`) become
fixed-width cells; leaf children of the branch keep ordinary rows above the
grid. Detection is structural — no `group_name` opt-in — and a sibling
missing a field shows `—`. The header row and instance rows share one
`ScrollHandle` so the grid scrolls sideways as a unit. Pivoted paths
persist; a folded pivot keeps its choice for when it reopens. The same
menu offers **Expand all** / **Collapse all**.

## Frame types (landed)

Right-click any branch with components → **Pivot alike frames**. Its shape
(sorted relative leaf paths, `model::signature`) becomes a `FrameType`
named after the branch's segment, and `model::alike` collects every
subtree anywhere in the namespace with exactly that shape — `dut1.psu`
and `dut2.bay.psu` in one grid, rows labelled by full path. Types render
as synthetic branches (`type:<label>`) above the tree, always as a pivot.
Right-click a type row → **Focus** (the outline shows only that grid, with
a bar offering **Show all**) or **Remove type**. A query narrows a type's
instances by path. Types and focus persist with the pane.

## Follow-ups

- Branch summaries beyond count: worst alarm state, stale leaf count.
  Needs a cheap per-component `last_timestamp` + alarm read (plan 08).
- Two-tier pivot headers (`motor ▸ temp, current`) with collapsible column
  groups, and a colour cell mode (instances × fields heatmap).
- Retire the data table pane once this covers its uses.
