# Plan: split `tiles/panels.rs` and collapse its copy-paste

## Goal

`src/tiles/panels.rs` is 1,885 lines — the largest file in the tile shell — and
braids three unrelated concerns together: the persisted `*Config` mirror structs,
the thin `PaneItem` wrapper panels, and the palette's "New Panel" row builders.
Two of those concerns are also heavily copy-pasted.

Split the file into a `panels/` directory of coherent modules and erase the
duplication with two generic helpers, **without changing any `tiles::panels::X`
import path anywhere else in the crate** and **without changing a single
serialization-key string** (saved layouts must keep loading byte-for-byte).

## Current state

One file, `src/tiles/panels.rs`, 1,885 lines, declared `pub mod panels;` in
`src/tiles/mod.rs:4`. It contains three concerns.

### Concern A — persisted config mirror structs (≈ 570 lines, incl. tests)

Every panel pairs itself with a sister `*Config` struct (the `PaneItem::Config`
associated type) plus, for the richer panels, mirror structs for their inner
data model and hand-written `From`/`Default` impls that bridge the live view
types to the serializable ones. Measured:

| Type | Lines | Span |
|---|---:|---|
| `TextPanelConfig` | 6 | 27–32 |
| `AlarmPanelConfig` | 6 | 84–89 |
| `SequencePanelConfig` | 6 | 140–145 |
| `SequenceGridPanelConfig` | 4 | 196–199 |
| `TrafficLightPanelConfig` | 6 | 243–248 |
| `TrafficLightGridPanelConfig` | 6 | 312–317 |
| `TablePanelConfig` | 4 | 382–385 |
| `DataTablePanelConfig` | 3 | 429–431 |
| `BrowserPanelConfig` | 11 | 477–487 |
| `PlotPanelConfig` | 36 | 604–639 |
| `MeasurementPanelConfig` + 2 `From` (⇄ `PanelPosition`) | 33 | 641–673 |
| `MeasurementCursorConfig` | 12 | 675–686 |
| `TraceConfig` + `Default` + 2 `From` (⇄ `Trace`) | 66 | 688–753 |
| `YAxisConfig` + `Default` + 2 `From` (⇄ `YAxis`) | 42 | 755–796 |
| `XyPlotPanelConfig` | 11 | 955–965 |
| `XyTraceConfig` + `Default` + 2 `From` (⇄ `XyTrace`) | 64 | 967–1030 |
| `ListPlotPanelConfig` | 11 | 1112–1122 |
| `ListTraceConfig` + `Default` + 2 `From` (⇄ `ListTrace`) | 53 | 1124–1176 |
| `Viewer3dPanelConfig` | 6 | 1228–1233 |
| `ModelConfig` | 11 | 1235–1245 |
| `CameraConfig` + `Default` | 33 | 1247–1279 |
| `mod tests` (config round-trip) | 139 | 1747–1885 |

These structs are pure data + conversions. They interleave awkwardly with the
panels (e.g. `PlotPanelConfig`/`TraceConfig`/`YAxisConfig` sit in the 200-line
gap between `PlotPanel`'s struct and its `PaneItem` impl).

### Concern B — thin `PaneItem` wrapper panels (≈ 960 lines)

Thirteen wrapper structs, each `Entity<InnerView>` + a label, with an
essentially identical trio of impls: a `new`/`from_config` constructor, a
one-line `Render` (`div().size_full().child(self.inner.clone())`), and a
`PaneItem` impl (`tab_title`, `serialization_key`, `to_config`,
`inspectable_entity`). The richer panels (`PlotPanel`, `XyPlotPanel`,
`ListPlotPanel`, `Viewer3dPanel`, `BrowserPanel`) carry real `from_config`/
`to_config` logic; the rest are boilerplate. This concern stays as the module
root — it is the substance of the file.

The **serialization keys** live here, one per `PaneItem` impl (verified literals):
`component_text`, `alarm`, `sequence`, `sequence_grid`, `traffic_light`,
`traffic_light_grid`, `component_table`, `data_table`, `component_browser`,
`time_series_plot`, `xy_plot`, `list_plot`, `viewer_3d`.

### Concern C — palette "New Panel" row builders (357 lines, 1389–1745)

`new_panel_rows` (1389–1709) plus two helpers, `traffic_light_grid_pattern_rows`
(1711–1725) and `component_picker_rows` (1727–1745). This is where the
copy-paste lives.

**Duplication block C1 — three plot-wizard `NavRow`s (140 lines, 1401–1540).**
Time Series (1401–1446), XY (1448–1493), and List (1495–1540) are near-identical
~45-line blocks. Each: clones `db`/`pane`/`on_open_inspector` twice through
nested closures, calls a trace-picker wizard, and in the picker callback
constructs a plot panel, reads its `inner()`, adds it to the pane, then reflects
the inner entity into inspector rows and opens the inspector centered. The only
differences are:
- the row label string,
- the picker fn: `inspector::trace_picker::select_traces_wizard_rows` /
  `views::xy_plot::trace_picker::select_xy_trace_wizard_rows` /
  `views::list_plot::trace_picker::select_list_trace_wizard_rows`,
- the panel constructor: `PlotPanel::with_traces(db, traces, …)` vs
  `XyPlotPanel::with_traces(db, vec![trace], …)` vs
  `ListPlotPanel::with_traces(db, vec![trace], …)`.

Crucially, **all three pickers share one shape** (verified):
`fn(Arc<DB>, ColorBasis, Arc<dyn Fn(T, &mut Window, &mut App)>) -> Vec<Box<dyn InspectorRow>>`
where `ColorBasis = Arc<dyn Fn(&App) -> usize>` and `T` is `Vec<Trace>` /
`XyTrace` / `ListTrace`. The `ColorBasis` argument is literally `Arc::new(|_cx| 0)`
in all three. The callback tail (add to pane + reflect + open inspector) is
byte-identical modulo the inner entity type, which is always `.into_any()`-ed.

**Duplication block C2 — nine "construct panel and add" `CommandRow`s (112 lines,
1595–1706).** Component Table, Data Table, Component Browser, 3D Viewer,
Dashboard, Alarms, Sequences, Sequence Grid, Node Editor. Every one is the same
12-line shape: clone `db`/`pane`, `pane.update(|pane, cx| { let item = Box::new(cx.new(|cx| SomePanel::new(db, cx))); pane.add_item(item, cx); })`.
Differences: the label string and the `SomePanel::new` constructor (all take
`(Arc<DB>, &mut Context<_>)`).

**Duplication block C3 — two component-picker `NavRow`s (42 lines, 1542–1583).**
Component Text and Traffic Light: both call `component_picker_rows` and, per
selection, add a `TextPanel::new` / `TrafficLightPanel::new`. Smaller, but the
same shape.

### External reference surface (constrains the re-export seam)

`panels` is `pub mod`; every outside caller reaches items via the fully-qualified
`crate::tiles::panels::X` path — there are **no** `tiles::PanelX` re-exports at the
`tiles::` level to preserve. The paths that must keep resolving:

- `src/app.rs:14–18` imports 13 panel types: `AlarmPanel, BrowserPanel,
  DataTablePanel, ListPlotPanel, PlotPanel, SequenceGridPanel, SequencePanel,
  TablePanel, TextPanel, TrafficLightGridPanel, TrafficLightPanel, Viewer3dPanel,
  XyPlotPanel` — and calls each panel's associated `from_config` via
  `register_panel::<T>` (`app.rs:870–890`).
- `src/inspector/palette.rs:247` → `crate::tiles::panels::new_panel_rows(...)`.
- `src/transient/menu.rs:18` → `use crate::tiles::panels::new_panel_rows;`.
- `src/views/dashboard/widgets.rs:18` → `crate::tiles::panels::{PlotPanelConfig, TraceConfig}`.
- `src/views/dashboard/widgets.rs:281` → `crate::tiles::panels::YAxisConfig`.
- `src/views/time_series/mod.rs:1216` → `crate::tiles::panels::MeasurementCursorConfig`.

`component_picker_rows` is `pub(crate)` but used only inside `panels.rs` (the
identically-named function in `views/dashboard/mod.rs:546` is a different local
fn). It can move freely.

**Conclusion:** if the new child modules re-export their public items into
`panels` without renaming (`pub use configs::*; pub use new_panel::*;`), every
one of the paths above keeps resolving with **zero edits outside `tiles/`**.

## Proposed design

Turn `panels.rs` into a directory:

```
src/tiles/panels/
  mod.rs          // the 13 wrapper panels + Render + PaneItem impls + from_config/to_config
  configs.rs      // every *Config + mirror struct + Default/From impls + the round-trip test
  new_panel.rs    // new_panel_rows, the two picker helpers, and the two new generic row helpers
```

`mod.rs` header:

```rust
pub(crate) mod configs;
pub(crate) mod new_panel;

pub use configs::*;      // re-export without renaming: preserves tiles::panels::PlotPanelConfig, …
pub use new_panel::*;    // preserves tiles::panels::new_panel_rows
```

Child modules are `pub(crate)` per the house rule; the glob re-exports keep the
external `crate::tiles::panels::X` surface intact. `tiles/mod.rs:4` stays
`pub mod panels;` unchanged — `pub mod panels` resolves to `panels/mod.rs`
identically to the old `panels.rs`.

### Collapse C1 — generic plot-wizard row

Because all three pickers share one shape, one generic builder erases the block.
Opinionated signature:

```rust
/// One "New Panel → …Plot" row: run a trace-picker wizard, then drop the
/// resulting panel into `pane` and open its inspector on the inner plot.
fn plot_wizard_row<T, P>(
    label: &'static str,
    db: Arc<DB>,
    pane: Entity<Pane>,
    on_open_inspector: Option<OpenInspectorCallback>,
    picker: fn(Arc<DB>, ColorBasis, Arc<dyn Fn(T, &mut Window, &mut App)>) -> Vec<Box<dyn InspectorRow>>,
    build: impl Fn(Arc<DB>, T, &mut Context<P>) -> P + Clone + 'static,
    inner_any: impl Fn(&P, &App) -> AnyEntity + 'static,
) -> Box<dyn InspectorRow>
where
    T: 'static,
    P: PaneItem,
```

The shared callback tail (add to pane, reflect the inner entity, open inspector
centered) becomes a small private helper the callback calls:

```rust
fn add_plot_and_inspect<P: PaneItem>(
    plot: Entity<P>,
    inner_any: AnyEntity,
    pane: &Entity<Pane>,
    db: &Arc<DB>,
    on_open_inspector: &Option<OpenInspectorCallback>,
    window: &mut Window,
    cx: &mut App,
)
```

The three call sites then read:

```rust
plot_wizard_row("Time Series Plot", db.clone(), pane.clone(), oi.clone(),
    select_traces_wizard_rows,
    |db, traces, cx| PlotPanel::with_traces(db, traces, cx),
    |p, cx| p.inner().read(cx).line_plot().clone().into_any());   // (via existing accessors)
plot_wizard_row("XY Plot", …, select_xy_trace_wizard_rows,
    |db, t, cx| XyPlotPanel::with_traces(db, vec![t], cx), …);
plot_wizard_row("List Plot", …, select_list_trace_wizard_rows,
    |db, t, cx| ListPlotPanel::with_traces(db, vec![t], cx), …);
```

Net: 140 lines → ~40 (helper + three ~6-line calls). The `inner_any` closure is
the one wrinkle — today each block reflects `plot_panel.read(cx).inner()`. Reuse
the existing `PlotPanel::inner()` / `XyPlotPanel::inner()` / `ListPlotPanel::inner()`
accessors so the reflected entity stays exactly what it is today (the inner
`TimeSeriesPlot`/`XyPlot`/`ListPlot`, whose `inspectable_entity` already routes to
the line-plot). Verify against the current code that the reflected entity is the
`inner()` view, not the line-plot, so behavior is unchanged.

### Collapse C2 — generic command row

```rust
/// One "New Panel → …" row that constructs `P::new(db, cx)` and adds it.
fn add_panel_row<P: PaneItem>(
    label: impl Into<SharedString>,
    db: Arc<DB>,
    pane: Entity<Pane>,
    make: impl Fn(Arc<DB>, &mut Context<P>) -> P + 'static,
) -> Box<dyn InspectorRow>
```

Nine call sites collapse to one line each, e.g.
`add_panel_row("Alarms", db.clone(), pane.clone(), AlarmPanel::new)`. Node
Editor fits too (`NodeEditor::new(db, cx)`); Dashboard fits
(`DashboardPanel::new`). Net: 112 lines → ~20.

### Collapse C3 — component-picker row (optional but cheap)

```rust
fn component_pick_row<P: PaneItem>(
    label: &'static str,
    db: Arc<DB>,
    pane: Entity<Pane>,
    make: impl Fn(Arc<DB>, ComponentId, String, &mut Context<P>) -> P + Clone + 'static,
) -> Box<dyn InspectorRow>
```

Collapses Component Text + Traffic Light (42 → ~15 lines). Include it — it is the
same pattern and leaving two of three duplication families collapsed is worse
than doing all three.

## Step-by-step migration (each step compiles)

**Step 0 — rename to a directory module.**
`git mv src/tiles/panels.rs src/tiles/panels/mod.rs`. Nothing else changes;
`pub mod panels;` now resolves to `panels/mod.rs`. Build. This is a pure move,
zero-risk, and keeps history via `git mv`.
Files touched: `src/tiles/panels.rs` → `src/tiles/panels/mod.rs`.

**Step 1 — extract `configs.rs`.**
Create `src/tiles/panels/configs.rs`. Move every row from the Concern-A table
(all `*Config` structs, the mirror structs, and their `Default`/`From` impls) and
the `mod tests` block into it. Add to `mod.rs`:
`pub(crate) mod configs; pub use configs::*;`. Give `configs.rs` its own `use`
block — it needs `gpui::Hsla`, `metor_proto::types::ComponentId`, and the view
types the `From` impls bridge (`Trace`, `YAxis`, `XyTrace`, `ListTrace`,
`Override`, `PlotStyle`, `MeasurementKind`, `TimeFormat`, `PanelPosition`, plus
`OrbitCamera` for `CameraConfig::default`). `mod.rs` keeps its own imports; the
glob re-export puts the config names back in `mod.rs`'s scope for the
`from_config`/`to_config` bodies that stay behind. Build + `cargo test -p
metor-panel panel_configs_round_trip_through_facet_json`.
Files touched: `mod.rs`, new `configs.rs`.

**Step 2 — extract `new_panel.rs` (verbatim, no dedup yet).**
Create `src/tiles/panels/new_panel.rs`. Move `new_panel_rows`,
`traffic_light_grid_pattern_rows`, and `component_picker_rows` verbatim. Add
`pub(crate) mod new_panel; pub use new_panel::*;` to `mod.rs`. `new_panel.rs`
imports the panel types from `super::*` (they are defined in `mod.rs`) plus the
inspector/view picker paths it already names by full path. Build.
Files touched: `mod.rs`, new `new_panel.rs`.

**Step 3 — collapse C2 (`add_panel_row`).**
Add the `add_panel_row` helper to `new_panel.rs` and rewrite the nine
`CommandRow` entries. Build. This is the lowest-risk dedup — do it first to
validate the helper pattern.
Files touched: `new_panel.rs`.

**Step 4 — collapse C1 (`plot_wizard_row` + `add_plot_and_inspect`).**
Add both helpers and rewrite the three plot rows. Build. Cross-check the
reflected entity matches the pre-refactor `inner()` target.
Files touched: `new_panel.rs`.

**Step 5 — collapse C3 (`component_pick_row`).**
Add the helper and rewrite the two component-picker rows. Build.
Files touched: `new_panel.rs`.

No step touches any file outside `src/tiles/panels/`. `src/app.rs`,
`src/inspector/palette.rs`, `src/transient/menu.rs`, `src/views/dashboard/…`,
`src/views/time_series/…` are all unaffected because the re-export paths are
preserved.

## Risks and how to test

- **Serialization keys must stay byte-identical.** They ride along inside the
  `PaneItem` impls, which are moved-verbatim/untouched in Step 0 and never edited
  after. Guard it: capture `grep -oE '"[a-z_]+"' ` of the 13 `serialization_key`
  return literals before and after and diff them (they must equal
  `component_text, alarm, sequence, sequence_grid, traffic_light,
  traffic_light_grid, component_table, data_table, component_browser,
  time_series_plot, xy_plot, list_plot, viewer_3d`). Also assert nothing changes
  `SUPPORTED_LAYOUT_VERSION` or the `serial.rs` shapes.
- **Config wire format must not drift.** The round-trip test
  (`panel_configs_round_trip_through_facet_json`) moves into `configs.rs`
  verbatim, including the legacy-JSON string at line 1817 that still carries the
  removed `"default_measurements":[]` key (it exercises forward-compat and must
  stay as-is). Run it after Steps 1–5. Add nothing; changing a field name or
  order in a `*Config` would silently break old layouts.
- **Reflected-entity identity in the plot wizard.** The generic `plot_wizard_row`
  must reflect the *same* entity the hand-written blocks do. Verify by comparing
  the `inner_any` closures against the original `plot_panel.read(cx).inner()`
  usage before deleting the originals; a mismatch would open the inspector on the
  wrong data model (or nothing).
- **Picker fn pointers.** `plot_wizard_row` takes the picker as a `fn` pointer;
  confirm the three `select_*_wizard_rows` are free functions (they are) so they
  coerce to `fn`. If any turns out to need capture, fall back to
  `impl Fn(...) -> Vec<...>`.
- **Glob re-export collisions.** `pub use configs::*` and `pub use new_panel::*`
  must not re-export overlapping names; they don't today (disjoint sets). Keep it
  that way.
- **Verification:** `cargo build -p metor-panel`, `cargo clippy -p metor-panel`,
  `cargo test -p metor-panel`, and a manual smoke run (`cargo run -p metor-panel`)
  exercising New Panel → each plot type and loading a previously-saved layout.

## Estimated scope

- New files: `panels/configs.rs` (~570 lines incl. test), `panels/new_panel.rs`
  (~240 lines after dedup, down from 357).
- `panels/mod.rs`: shrinks from 1,885 to ~960 lines (wrappers + `from_config`/
  `to_config` only).
- Duplication removed: ~180 lines (C1 ~100 + C2 ~90 + C3 ~25), replaced by ~80
  lines of three generic helpers → net ~100-line reduction plus a far lower
  future edit cost (adding a panel becomes one `add_panel_row` line).
- Files edited outside `panels/`: **none**.
- Effort: ~half a day. Steps 0–2 are mechanical moves; Steps 3–5 are the only
  places real judgement is exercised, and each is independently revertible.

## Open questions

1. **Should `from_config`/`to_config` bodies move to `configs.rs` too?** They are
   panel *construction/snapshot* logic that happens to read/write config structs,
   so this plan keeps them in `mod.rs` with the panels. If `mod.rs` still feels
   heavy afterward, a follow-up could push the plot `from_config` bodies into
   `configs.rs` as `impl PlotPanelConfig { fn apply(self, …) }` methods — but that
   inverts the dependency (configs would need panel/view types) and is probably
   not worth it. Flagging, not recommending.
2. **`plot_wizard_row`'s `build` closure vs. a `with_traces` trait.** The plan
   passes `build` as a closure so `XyPlotPanel`/`ListPlotPanel` can wrap the
   single trace in `vec![t]`. An alternative is a small `SeedFromTraces<T>` trait
   implemented per panel. The closure is lighter and local; the trait only pays
   off if a fourth plot type appears. Defaulting to the closure.
3. **`ColorBasis` seed.** All three sites pass `Arc::new(|_cx| 0)`. The helper can
   hardcode that default internally (dropping it from the signature) unless a
   caller ever needs a non-zero color basis. Recommend hardcoding it inside
   `plot_wizard_row` for the smallest call sites, revisiting only if needed.
