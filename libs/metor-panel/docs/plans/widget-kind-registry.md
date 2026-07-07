# Widget-kind registry

## Goal

Define each hostable view's **kind layer** — its config struct, its
build-from-config, its label, and its serialize-live-state — exactly once, and
have *both* hosting systems consume it:

- the free-form dashboard (`src/views/dashboard/`), which pixel-positions
  widgets on a canvas, and
- the split-pane tiles system (`src/tiles/`), which stacks panels as tabs.

The two hosts are legitimately different and **stay separate** — canvas rect +
z-order + edit mode on one side, split tree + tab bar + `serialization_key` on
the other. What must collapse is the middle: today every shared view is
described *twice*, once as a `WidgetKind` + `*WidgetConfig` (dashboard) and once
as a `PaneItem` + `*PanelConfig` (tiles), with parallel builders, parallel
labels, parallel live-serialization, and parallel add-item pickers. They are
already drifting: the dashboard plot serializer (`serialize_widget_state`) drops
`cursors` and `measurement_panel` that the tiles `PlotPanel::to_config` keeps,
and the dashboard `MonitorWidgetConfig` display-field fix (`unit`,
`show_sparkline`) had to be patched in after the tiles side already handled its
equivalents. Every future field is a double-edit waiting to be forgotten on one
side.

Secondary cleanup in scope: the trailing `else` in
`dashboard::component_picker_rows` (`src/views/dashboard/mod.rs:576-578`) that
silently maps *any* non-`monitor`, non-`traffic_light` kind to
`TextWidgetConfig`. Still present — confirm and remove as part of making the
add-flow data-driven.

## Current state

### Two registries, two config vocabularies

**Dashboard** (`src/views/dashboard/`):

- `WidgetKind(SharedString)` newtype (`mod.rs:54`) — on-disk kind id, one
  constructor per built-in (`plot`, `text`, `table`, `image`, `monitor`,
  `viewer3d`, `traffic_light`, `traffic_light_grid`).
- `WidgetRegistry` gpui `Global` mapping `WidgetKind -> Arc<WidgetSpec>`
  (`widgets.rs:41`). `WidgetSpec` (`widgets.rs:31`) carries `default_size`,
  `label: Arc<dyn Fn(&DashboardWidget) -> SharedString>`, and
  `build: Arc<dyn Fn(&str, &Arc<DB>, &mut App) -> (AnyView, AnyEntity)>`.
- Per-kind config structs in `widgets.rs`: `TextWidgetConfig` (`:157`),
  `ImageWidgetConfig` (`:163`), `MonitorWidgetConfig` (`:174`),
  `TrafficLightWidgetConfig` (`:184`), `TrafficLightGridWidgetConfig` (`:191`).
  The plot kind already **reuses the tiles struct** `PlotPanelConfig` /
  `TraceConfig` / `YAxisConfig` (imported at `widgets.rs:18`).
- `serialize_widget_state` (`widgets.rs:246`) — live-state snapshot, a big
  `if *kind == …` ladder over `plot`, `traffic_light`, `traffic_light_grid`,
  `monitor`.
- Add-item pickers: `add_widget_rows` (`mod.rs:445`), `component_picker_rows`
  (`mod.rs:546`, the buggy `else`), `traffic_light_grid_pattern_rows`
  (`mod.rs:594`), `image_path_rows` (`mod.rs:612`).
- Persistence: `DashboardPanelConfig.widgets: Vec<DashboardWidget>`
  (`mod.rs:750`); each `DashboardWidget` carries `kind` + opaque `config`
  string. `deserialize_dashboard` (`mod.rs:798`) rebuilds via
  `create_widget_view`. The dashboard is itself a tiles `PaneItem`
  (`serialization_key = "dashboard"`, `mod.rs:764`).

**Tiles** (`src/tiles/panels.rs`): one concrete `PaneItem` struct per view, each
a thin `Render` wrapper over an inner view `Entity`, each with its own
`*Config`, `from_config`, `to_config`, `serialization_key`, `tab_title`,
`inspectable_entity`. Registered in `src/app.rs:865`
(`register_pane_item_deserializers`) into the string-keyed `ItemRegistry`
(`src/tiles/serial.rs:96`). Deserialization is keyed on the per-instance
`serialization_key` written into `SerializedItem.kind` (`serial.rs:79`).

### Every view kind hosted by both systems

The **key strings differ between the two hosts** — this is load-bearing for
back-compat (see Risks).

| View (inner entity) | Dashboard `WidgetKind` | Dashboard config | Tiles `PaneItem` / key | Tiles config | Shared? |
|---|---|---|---|---|---|
| `TimeSeriesPlot`/`LinePlot` | `plot` | `PlotPanelConfig` (already tiles') | `PlotPanel` / `time_series_plot` | `PlotPanelConfig` | **both** |
| `ComponentTable` | `table` (no cfg) | `TablePanelConfig{}` | `TablePanel` / `component_table` | `TablePanelConfig{}` | **both** |
| `Viewer3d` | `viewer3d` (no cfg) | — | `Viewer3dPanel` / `viewer_3d` | `Viewer3dPanelConfig` | **both** (dashboard drops model/camera state) |
| `TrafficLight` | `traffic_light` | `TrafficLightWidgetConfig{component,color}` | `TrafficLightPanel` / `traffic_light` | `TrafficLightPanelConfig{component,color}` | **both** (byte-identical shape) |
| `TrafficLightGrid` | `traffic_light_grid` | `TrafficLightGridWidgetConfig{pattern,color}` | `TrafficLightGridPanel` / `traffic_light_grid` | `TrafficLightGridPanelConfig{pattern,color}` | **both** (byte-identical shape) |
| `ComponentText` | `text` | `TextWidgetConfig{component}` | `TextPanel` / `component_text` | `TextPanelConfig{component}` | **both** (byte-identical shape) |
| `Monitor` | `monitor` | `MonitorWidgetConfig{component,unit?,show_sparkline?}` | — | — | dashboard-only today |
| `ImageWidget` | `image` | `ImageWidgetConfig{path}` | — | — | dashboard-only |

Dashboard-only: `image` (leaf renderer lives in `widgets.rs`). Tiles-only:
`AlarmPanel`, `SequencePanel`, `SequenceGridPanel`, `DataTablePanel`,
`BrowserPanel`, `XyPlotPanel`, `ListPlotPanel`, `NodeEditor`.

**Note on "monitor":** the refactor brief lists monitor as hosted by both; it is
actually dashboard-only right now (`Monitor` is a view at
`src/views/monitor.rs`, wrapped only by the dashboard). The shared registry
makes lifting it into a tiles tab a one-liner, but that is optional parity, not
a prerequisite.

### Where identity is anchored (must not break)

The inspector downcasts on **inner view entity types**, never on the host
wrapper: `LinePlot`, `XyLinePlot`, `ListLinePlot`, `Trace`, `Pane`,
`DashboardPanel`, etc. (`src/inspector/registry/builders.rs:203,234,270`,
`defaults.rs:375,478,587`, `palette.rs:211`). As long as the shared builders
produce the *same inner entities* and hand the *same inspectable entity* to the
inspector, this refactor is invisible to reflection. The kind layer must
therefore surface, per kind, both the inspectable entity (what the inspector
downcasts) and the view (what gets painted) — exactly the `(AnyView, AnyEntity)`
pair the dashboard already returns, plus one addition (below).

## Proposed design

Introduce one crate-level module, `src/widget_kind.rs` (or `src/widget_kind/`
if it outgrows a file), that owns the kind layer. Both hosts depend on it; it
depends on neither.

### The shared spec

```rust
/// One placement of a shared view: the painted view, the entity the inspector
/// downcasts, and the entity `serialize_live` reads to snapshot editable state.
pub struct Built {
    pub view: AnyView,
    pub inspect: AnyEntity,   // handed to the inspector (e.g. LinePlot)
    pub state: AnyEntity,     // read by serialize_live (e.g. TimeSeriesPlot)
}

pub struct KindSpec {
    pub default_size: (f32, f32),
    pub build: Arc<dyn Fn(&str, &Arc<DB>, &mut App) -> Built>,
    /// Snapshot live editable state back to a config blob. `None` = the view
    /// owns nothing beyond its cached blob (text, table, image).
    pub serialize_live: Arc<dyn Fn(&AnyEntity, &str, &App) -> Option<String>>,
    /// Label from a config blob (both hosts need this without a live entity).
    pub label: Arc<dyn Fn(&str) -> SharedString>,
    /// How the palette gathers args before placing a new instance.
    pub add_flow: AddFlow,
}

pub enum AddFlow {
    Immediate,                 // no config needed (table, viewer3d)
    ComponentPick,             // list components -> {component}
    PatternPrompt,             // glob text -> {pattern}   (traffic_light_grid)
    ImagePath,                 // file path -> {path}
    TraceWizard,               // trace picker -> pre-built entity (plot)
}
```

`serialize_live` gaining a `state` entity distinct from `inspect` is the one
new seam: it lets the shared plot serializer read the outer `TimeSeriesPlot`
(cursors, measurement-panel position) rather than only the inspectable
`LinePlot`. This **fixes the existing dashboard plot drift** as a side effect —
the dashboard just needs to store `state` alongside `inspect`.

The registry is the dashboard's current `WidgetRegistry`, promoted to the shared
module and keyed by the canonical `WidgetKind` (keep the newtype and its
`plot()`/`text()`/… constructors — they are already the dashboard's on-disk
strings and are the natural canonical ids). Downstream `register`/override stays.

### The config structs move, and collapse to one per kind

The byte-identical pairs collapse to a single struct owned by the shared module
(or, where the tiles struct is already the canonical one, re-exported from
there): `text`, `traffic_light`, `traffic_light_grid`. `plot` is already shared
(`PlotPanelConfig`). `monitor` and `image` have no tiles twin — move them into
the shared module unchanged. Field names and order are preserved exactly (see
Risks — no version bump).

### How each host consumes it

**Dashboard** — nearly a no-op structurally, since its `WidgetSpec` *is* this
shape. Replace `WidgetSpec`/`WidgetRegistry` with the shared `KindSpec`/registry,
store `Built.state` in a new parallel map (or fold into the existing
`widget_entities` if `inspect == state`), and delegate
`serialize_widget_state` and `create_widget_view` to the spec. The add-item
pickers become a data-driven loop over `AddFlow`.

**Tiles** — keep the thin per-kind `PaneItem` wrappers (they earn their
existence via the static `serialization_key` and the `ItemRegistry`
string-dispatch, which the inspector and `SUPPORTED_LAYOUT_VERSION` machinery
depend on). Rewrite their bodies to delegate:

- `from_config` → look up the kind spec, call `build`, keep `Built.view` +
  `Built.inspect` + `Built.state`.
- `to_config` / `serialize` → call `serialize_live(state, cached, cx)`, fall
  back to the cached blob.
- `inspectable_entity` → `Built.inspect`.
- `serialization_key` / `tab_title` → unchanged (host-owned identity).

A tiny per-host map (`serialization_key <-> WidgetKind`) bridges the differing
key strings (`"time_series_plot"` ⇄ `plot`, `"component_table"` ⇄ `table`,
`"viewer_3d"` ⇄ `viewer3d`, `"component_text"` ⇄ `text`; `traffic_light` and
`traffic_light_grid` are already equal). This map lives on the tiles side; the
shared module never sees tiles' key strings, and the dashboard never sees them
either — both on-disk vocabularies are preserved.

**Rejected alternative:** a single generic `WidgetPane<K>` tiles `PaneItem`
replacing all the wrapper structs. It collides with `serialization_key()` being
an associated (self-less) fn — you'd need zero-sized marker types per kind
(`WidgetPane<PlotKind>`, …), which trades N thin structs for N marker structs
plus a generic, for no real gain and more churn against the inspector and
registration paths. Keep the thin wrappers.

## Step-by-step migration (each step compiles)

Every step is independently buildable and testable with
`cargo build -p metor-panel` / `cargo test -p metor-panel`.

**Step 1 — kill the picker bug, no structural change.** In
`src/views/dashboard/mod.rs`, make `component_picker_rows` exhaustive: replace
the trailing `else` (`:576-578`) with an explicit `text` arm and a
`debug_assert!`/`unreachable!` (or route unknown kinds to a logged no-op).
Touches: `src/views/dashboard/mod.rs`. Regression test: adding a `text` widget
still serializes `{component}`.

**Step 2 — introduce the shared module, dashboard-only consumer.** Create
`src/widget_kind.rs`. Move `WidgetKind` (from `dashboard/mod.rs`), the
dashboard config structs (`Text/Image/Monitor/TrafficLight/TrafficLightGrid`),
`WidgetSpec`→`KindSpec` (+ `Built`, `AddFlow`), `WidgetRegistry`,
`serialize_widget_state`, and the build fns out of `dashboard/widgets.rs`.
Re-export from `dashboard` for source-compat (`pub use
crate::widget_kind::{WidgetKind, WidgetRegistry, …}`). Register the module in
`src/lib.rs`. Keep `dashboard/widgets.rs` for the dashboard-only leaf renderers
(`ImageWidget`, `PlaceholderWidget`). Touches: `src/widget_kind.rs` (new),
`src/views/dashboard/widgets.rs`, `src/views/dashboard/mod.rs`, `src/lib.rs`,
`src/app.rs` (`WidgetRegistry::init` path). Behavior identical; tests in
`widgets.rs` move with the structs.

**Step 3 — add the `state` seam and fix dashboard plot drift.** Extend `Built`
with `state`; update the dashboard to store it and pass it to
`serialize_live`. Rewrite the shared `plot` build + `serialize_live` to the
*full* `PlotPanel` semantics (cursors, `measurement_panel`, alarm flags), reading
the outer `TimeSeriesPlot`. Touches: `src/widget_kind.rs`,
`src/views/dashboard/mod.rs` (`widget_entities`/new `widget_states` map,
`ensure_views`, `add_widget*`, `remove_widget`, `to_config`,
`deserialize_dashboard`). Test: a dashboard plot with a locked cursor
round-trips (previously lost).

**Step 4 — tiles shared-kind wrappers delegate to the registry.** Rewrite
`from_config`/`to_config`/`serialize`/`inspectable_entity` bodies of
`TextPanel`, `TrafficLightPanel`, `TrafficLightGridPanel`, `TablePanel`,
`Viewer3dPanel`, `PlotPanel` to call the shared spec, keeping their
`serialization_key`/`tab_title`. Delete the now-duplicate tiles config structs
that the shared module owns (`TextPanelConfig`, `TrafficLightPanelConfig`,
`TrafficLightGridPanelConfig`) and re-point `type Config` at the shared struct;
keep `PlotPanelConfig`/`Viewer3dPanelConfig`/`TablePanelConfig` where the tiles
struct is the canonical one. Add the `serialization_key <-> WidgetKind` bridge
map. Touches: `src/tiles/panels.rs`, `src/widget_kind.rs`. Registration in
`src/app.rs` is unchanged (still one `register_panel::<T>` per wrapper). Run the
`panel_configs_round_trip_through_facet_json` test unchanged — it pins the wire
format and must still pass.

**Step 5 — data-drive both add-item pickers.** Generate the shared-kind rows in
`dashboard::add_widget_rows` and `tiles::panels::new_panel_rows` from a single
iteration over the registry's `AddFlow`, so a new shared kind appears in both
menus without touching either. Keep the host-only rows (dashboard: none extra;
tiles: Alarm/Sequence/DataTable/Browser/Xy/List/NodeEditor) as hand-written
entries. This retires `component_picker_rows`'s per-kind ladder entirely (the
Step-1 fix becomes moot but harmless). Touches: `src/views/dashboard/mod.rs`,
`src/tiles/panels.rs`.

**Step 6 (optional parity) — lift `monitor` (and `image`) into tiles.** Add
`MonitorPanel`/`ImagePanel` thin wrappers with fresh `serialization_key`s
(`"monitor"`, `"image"`), register them in `src/app.rs`. Pure addition; no
back-compat impact. Do only if the product wants monitors/images as tabs.

## Risks and how to test

**Back-compat is the whole ballgame.** Two independent on-disk formats must keep
loading:

- **Tile layouts** — `SerializedItem.kind` strings (`time_series_plot`,
  `component_text`, …) and each `*Config`'s field names/order. The refactor
  preserves both: wrappers keep their `serialization_key`; unified structs keep
  identical fields. **No `SUPPORTED_LAYOUT_VERSION` bump** (currently `3`,
  `src/tiles/mod.rs:56`) — the format is field-additive with facet defaults and
  nothing changes shape. If any step is forced to rename a persisted field,
  that step bumps the version in lockstep with `TileGroup::serialize` and adds
  a `#[facet(default)]`.
- **Dashboards** — `DashboardWidget.kind` strings (`plot`, `text`, …) and the
  per-kind config blobs, all inside the `"dashboard"` `PaneItem`'s state.
  Preserved identically.

Tests:

1. Extend `panels.rs::panel_configs_round_trip_through_facet_json` and the
   `widgets.rs` monitor tests — they already pin wire formats; they must pass
   unchanged through Steps 2–4.
2. Add golden-blob load tests: a hand-written pre-refactor tile-layout JSON and
   a pre-refactor dashboard JSON (with a plot carrying legacy single-axis
   bounds and a traffic-light color) must deserialize to the same live state
   after the refactor.
3. Add the Step-3 regression: dashboard plot with a locked measurement cursor +
   pinned panel survives `to_config` → `deserialize_dashboard`.
4. Inspector smoke: after Step 4, right-click a tiles `PlotPanel` and a
   dashboard plot widget and confirm the inspector still resolves `LinePlot`
   rows (the `inspect` entity is unchanged). Manual, via `cargo run -p
   metor-panel`.

Other risks:

- **`inspect` vs `state` mix-up** handing the inspector the outer
  `TimeSeriesPlot` instead of `LinePlot` silently breaks trace-config rows.
  Mitigation: `Built` names the two explicitly; the tiles wrapper's
  `inspectable_entity` and the dashboard's inspector dispatch both read
  `Built.inspect`.
- **Key-bridge omission** a missing `serialization_key <-> WidgetKind` entry
  makes a tiles panel fail to find its spec at load and silently drop. Cover
  with a test asserting every shared-kind wrapper's key resolves to a
  registered `KindSpec`.
- **Downstream `register`/override** the registry is a gpui `Global`;
  consumers may register extra kinds at startup. Keep `register`/`spec` public
  and the placeholder fallback (`widgets.rs:219`) so unknown kinds in stale
  files still render a "? unknown kind" tile on both hosts.

## Interaction with other plans

`docs/plans/panels-split.md` **does not exist** in this tree — there is nothing
to sequence against it. The adjacent `docs/plans/plot-shell-unification.md` is
*orthogonal*: it collapses the plot *rendering shells* (`XyPlot`/`ListPlot`
wrappers, legend, GPU scaffold — the inner view layer), whereas this plan
touches the *hosting* layer above the views. They meet only at
`PlotPanelConfig`/`XyPlotPanelConfig` round-tripping. Recommended sequencing:
**land this widget-kind registry first** — it is mechanical, config-format-
stable, and does not reshape any plot view; plot-shell-unification then rides on
a single already-consolidated plot builder instead of two. If plot-shell lands
first instead, Step 4's plot delegation simply targets whatever the unified
shell exposes — no hard conflict either way, but doing this first is lower risk.

## Estimated scope

Medium. ~6 steps, touching `src/widget_kind.rs` (new, ~250–350 LOC absorbed
from `widgets.rs`), `src/views/dashboard/{mod,widgets}.rs`, `src/tiles/panels.rs`
(delegation rewrites, net **reduction**), `src/lib.rs`, `src/app.rs` (imports
only). No new deps. The net line count should drop: ~5 parallel config structs
and ~2 parallel builder/label/serialize families collapse to one each, and two
add-item pickers merge. Steps 1–2 are low-risk mechanical moves; Step 3 carries
the only genuine behavior change (dashboard plot state gains cursors/panel — a
fix); Steps 4–5 are the bulk of the delegation work; Step 6 is optional.
```