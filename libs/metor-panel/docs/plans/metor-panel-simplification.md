# Metor Panel simplification plan

## Goal

Reduce the amount of code and indirection in `metor-panel` while keeping the
remaining behavior easy to follow. Prefer deletion of genuinely dead internals,
one coherent extension mechanism, and abstractions that remove more code than
they add. Dashboard behavior and downstream view registration are first-class
requirements, not vestigial features.

This plan follows a module-by-module review of all 157 Rust files, followed by
an independent whole-crate review. The current baseline is approximately 58,479
lines of Rust across 24 top-level modules, with 24 `PaneItem` implementations.

No implementation change should be justified only by moving code between files.
A simplification should delete behavior, remove a concept, establish one owner
for state, or produce a measurable net reduction.

## Guiding rules

1. Delete a genuinely unused internal feature before generalizing it. Absence
   of an in-workspace caller is not proof that a public library API is unused.
2. Keep one canonical config/build/snapshot path per view.
3. Make built-in and downstream views use the same runtime registration path.
   One registration should cover hosting, persistence, inspection, and creation
   instead of requiring edits to parallel hard-coded lists.
4. Share concrete helpers before introducing generic engines or marker types.
5. Maintain one clearly versioned current layout format. Do not carry readers,
   mirrored fields, or migrations for older layout versions.
6. Keep performance work separate when it adds caches, pools, batching, or
   synchronization state rather than simplifying the model.

## Fixed architectural constraints and remaining decisions

### 1. Dashboard is a core workflow

The dashboard is about 3,555 lines:

- `src/views/dashboard/mod.rs`: 1,785 lines
- `src/views/dashboard/widgets.rs`: 747 lines
- `src/views/dashboard/connectors.rs`: 564 lines
- `src/views/dashboard/interaction.rs`: 413 lines
- `src/views/dashboard/chrome.rs`: 46 lines

Its unique behavior—absolute positioning, schematic connectors, image widgets,
and monitor widgets—is part of the shipped product. Keep the dashboard and its
specialized interaction/connector code. Simplify the duplicated view-kind layer
it shares with tiles, not the dashboard host itself.

### 2. `metor-panel` is both an application and a library

Downstream users adding new views is a first-class use case. Preserve public
registration, palette-provider, initialization, overlay, hosting, and
inspection extension points. Workspace-only call-site searches are useful for
finding private dead code, but cannot justify deleting these APIs.

The current problem is not that registries exist; it is that registration is
partial. Dashboard build/label dispatch uses `WidgetRegistry`, while add flows,
snapshotting, inspection, tile hydration, and panel creation live in separate
hard-coded paths. Replace these parallel mechanisms with one coherent public
view registration contract used by built-ins and downstream views alike.

### 3. List Plot is a core feature

List Plot is the vector/FFT visualization surface and must remain. Simplify its
duplicated interaction shell with XY Plot in Phase 5 while retaining its picker,
config, registered kind, and implicit-index data semantics.

### 4. Old layouts are not supported

Delete old-layout compatibility rather than migrating it:

- legacy single-axis plot fields in `src/tiles/panels.rs:910-920,1124,1199-1200`
- `LatestAt`, which is semantically identical to zero-order hold
- persisted dashboard ID counters that can be derived

Remove their readers, writers, fallback branches, and legacy tests immediately.
Whenever a persisted-shape change lands, bump the current layout version and
reject older versions without conversion; no coordinated migration release is
required.

Historical telemetry recordings are a separate compatibility question. This
answer does not authorize deleting the per-`AlarmDef` ingest path retained beside
the current `AlarmDefs` message; keep it until recording compatibility is decided.

## Phase 1: internal cleanup and authorized layout break

This phase must not remove public extension contracts merely because the
workspace has no caller. Most items are private duplication or provably
unobservable work; the layout removals are intentionally breaking and authorized
by the old-layout policy above.

1. Delete the duplicate Node Editor palette route.
   - Remove `src/node_editor/palette_provider.rs` and its registration at
     `src/app.rs:105`.
   - The normal panel provider already exposes `NodeEditor`; downstream palette
     registration remains supported through the general provider API.

2. Remove useless runtime work and GPU metadata.
   - Delete `ComponentRow::_task` and its per-row WAL reader in
     `src/views/component_table.rs:25-33,76-87`; the row is not rendered or
     observed, while its child strips own their subscriptions.
   - Remove the dead `component_id` fields and `#[allow(dead_code)]` attributes
     from crate-private `AxisSource` variants in
     `src/views/time_series/gpu.rs:59-89`.

3. Flatten private duplicate control paths.
   - Match `ChordAction` directly and remove the private `Dispatch` result layer
     in `src/transient/node.rs:60-120`.
   - Reuse `open_inspector_with` from `open_entity_inspector`
     (`src/app.rs:137-153,291-311`).
   - Move values directly into the `FnOnce` application startup closure instead
     of wrapping every value in `Option` and calling `take`
     (`src/app.rs:1076-1083,1124-1163`). Keep all public `PanelApp` hooks.
   - Make `TileGroup::has_items` consult its maintained flat pane list rather
     than recursively walking the tree (`src/tiles/mod.rs:565-572`).
   - Replace repeated node-editor label/from-label scans with typed lookup
     helpers (`src/node_editor/inspector_rows.rs:993-1144`).
   - Build node-graph indegrees and children in one edge pass instead of
     rescanning all edges per node (`src/node_editor/graph.rs:153-162`).

4. Remove internal bookkeeping only after proving it is not part of an
   extension-facing contract.
   - `logs::level_index` and `WiringState::updated_at` have no internal reader.
   - `EventSource::generation` and its `pushed`/`history_pushed` counters have no
     internal reader because repainting observes source entities directly.
   - If these types remain public, remove them through the crate's normal
     deprecation/breaking-release process rather than an unannounced deletion.

5. Retain public symbols and bundled icons until their API status is explicit.
   - Workspace search found unused methods such as `TileGroup::from_pane`,
     `RowList::is_editing`, `DynamicRegistry::is_empty`, and several view
     getters, plus 26 icon variants with no in-tree caller.
   - These are candidates for deprecation, not unconditional deletion, because
     downstream view implementations may use them.
   - `PaneItem::can_close` is a useful host-extension hook even though built-ins
     do not override it; keep it.

6. Delete old-layout compatibility now.
   - Remove legacy single-axis plot min/max fields, fallback seeding, and mirrored
     writes at `src/tiles/panels.rs:910-920,1124,1199-1200`.
   - Remove the `LatestAt` node spec, descriptor, implementation, and tests; keep
     zero-order hold as the one semantic operation.
   - Stop persisting dashboard next-ID counters and derive them from loaded
     widget/connector IDs.
   - Delete legacy-layout tests and fixtures rather than replacing them with
     migrations. Keep tests for the one current layout shape.

Expected reduction: approximately 200-400 Rust lines without narrowing the
supported library surface, including deletion of the authorized layout shims.

## Phase 2: make view registration complete and singular

Registries are required for downstream views. The simplification is to replace
several incomplete registries and hard-coded side tables with one public
registration contract.

1. Define one stable view-kind registration.
   - A registration owns the stable kind ID and callbacks for build-from-config,
     live snapshot, display label, inspectable entity, and palette/add flow.
   - The build result should carry the painted `AnyView`, the entity inspected
     by reflection, and the entity read for live serialization; these differ for
     views such as Time Series.
   - The add-flow surface should be callback/provider based so downstream views
     can implement arbitrary setup UI. A closed `AddFlow` enum would recreate
     the extensibility problem.
   - Built-ins register through the same API as downstream views.

2. Add host-specific metadata without duplicating the view definition.
   - Dashboard metadata includes default size and any placement constraints.
   - Tile metadata includes the stable serialized pane key and tab-label policy.
   - A view may support Dashboard, tiles, or both. Host support is declared in
     the registration rather than inferred from separate lists.

3. Make both hosts consume the shared registration.
   - Extend or promote `WidgetRegistry` (`src/views/dashboard/widgets.rs:30-214`)
     instead of replacing it with closed dispatch.
   - Fold tile hydration's `HashMap` of 22 startup closures
     (`src/tiles/serial.rs:29-76`, `src/app.rs:1285-1334`) into the same kind
     registry or a thin host adapter over it.
   - Drive palette creation and inspection routing from registered callbacks;
     retain the public palette-provider registry for non-view commands.
   - Preserve unknown kind/config blobs so a layout round-trips even when a
     downstream plugin is temporarily unavailable.

4. Use a generic registered-pane host for ordinary views.
   - `PaneItem::serialization_key` currently being static prevents one runtime
     wrapper from hosting arbitrary registered kinds. Make the stable kind an
     instance property at the erased handle boundary.
   - A `RegisteredPane` can hold the registration ID, `AnyView`, inspect/state
     entities, and config blob, replacing most thin built-in `*Panel` wrappers
     while also hosting downstream views.
   - Keep specialized `PaneItem` implementations only for panels with real
     host-specific behavior, including Dashboard itself.

5. Preserve and document the application/library hooks.
   - Keep `PanelApp::{overlay, command_provider, command, on_init}` and the
     connection/address/server helpers at `src/app.rs:886-1051`.
   - Add an explicit `view`/`view_kind` registration builder if `on_init` is too
     indirect for the primary extension path.
   - Keep connection options and target-shipped presets: both may be consumed by
     downstream connection backends or targets even without an in-workspace
     producer.

6. Make the inspector attribute grammar a real downstream extension surface.
   - `src/inspect.rs` currently advertises attributes, but the reflection walker
     never reads them and no built-in field uses them.
   - Wire `Range` and `Label` into the field walker, add the enum allow-list
     needed to replace the remaining `FieldOverride` cases, and migrate built-in
     overrides onto their fields.
   - Delete `FieldOverride` after migration so built-in and downstream types use
     the same declarative path.
   - Either implement `Widget`/`ReadOnly` completely or remove those variants;
     do not continue advertising attributes that have no behavior.

Expected reduction: the registry itself may grow modestly, but it replaces
parallel widget specs, tile deserializers, per-host config builders/snapshotters,
most thin wrappers, and duplicated add-menu routing. Judge this phase by total
net code and by whether adding a downstream view requires one registration
instead of edits across multiple modules.

## Phase 3: make views own their state and make hosts thin

This is the central architectural simplification and should precede any split of
`tiles/panels.rs`.

1. Move canonical persisted configuration into each concrete view module.
   - Time series owns plot, trace, axis, cursor, measurement-panel, and event
     overlay configs now in `src/tiles/panels.rs:897-1256`.
   - XY and List plots own configs now around
     `src/tiles/panels.rs:1288-1563`.
   - Viewer 3D owns model/camera config now around
     `src/tiles/panels.rs:1565-1721`.
   - Text, traffic-light, and traffic-light-grid views own the currently
     duplicated tile/dashboard config shapes.
   - Existing scalar views (`Meter`, `Gauge`, `StateChip`, `Attitude`, and
     `SequenceControl`) are the model: their configs already live with them.

2. Give each view one `from_config` and one live `to_config`/snapshot path.
   - Tiles and dashboard must call the same code.
   - This fixes existing dashboard drift: plot restore/snapshot omits or
     incompletely handles x range, cursors, the measurement panel, alarm flags,
     and event overlays compared with the tile path
     (`src/views/dashboard/widgets.rs:321-370,437-464` versus
     `src/tiles/panels.rs:1109-1246`).
   - Viewer 3D dashboard config is currently ignored even though inspector edits
     mutate live model state.

3. Remove the backwards `views -> tiles` dependency.
   - Current examples are `src/views/dashboard/widgets.rs:19,350,362`,
     `src/views/dashboard/mod.rs:994,1103,1110`, and
     `src/views/time_series/mod.rs:1393`.
   - Hosts may depend on views; views must not depend on panel wrappers.

4. Replace thin built-in wrappers with the generic registered-pane host from
   Phase 2 where they have no special host behavior.
   - Preserve serialization key strings and inspect/state entity identity.
   - Start with Text, Meter, Gauge, StateChip, Attitude, and SequenceControl,
     whose wrappers at `src/tiles/panels.rs:30-86,391-598` mainly store an
     entity/label and delegate `Render`/`PaneItem`.
   - Retain a concrete wrapper only when it owns host-specific state that the
     generic registered host cannot represent.

5. Make Dashboard consume the complete registry while retaining its host logic.
   - Replace its partial `WidgetSpec` with the shared registered view spec, so
     build, label, snapshot, inspect, and add flow cannot drift.
   - Combine `widget_views` and `widget_entities` into one
     `WidgetLive { view, entity }` map (`src/views/dashboard/mod.rs:132-140`).
   - Remove the special `add_widget_with_entity` route and feed plot wizard
     output through the normal config/build path (`:285-310,865-879`).
   - Derive next widget/connector IDs from maximum loaded IDs rather than
     persisting counters (`:1496-1505,1546-1559`).

6. Remove local creation duplication by registering add flows once.
   - The eleven default-panel command rows (`src/tiles/panels.rs:2036-2173`)
     and repeated Time/XY/List creation tails (`:1741-1880`) should be generated
     from each registered kind's provider callback.
   - Keep truly host-specific placement in the tile/dashboard adapter, but do
     not repeat view-specific pickers and construction callbacks in both hosts.

Expected reduction: approximately 500-900 lines, plus one-way dependencies,
one serialization path per view, and fewer opportunities for host drift.

## Phase 4: flatten core data and editor abstractions

1. Retain the public component-stream abstraction.
   - `ComponentStream`/`ComponentStreamBuilder` let downstream views and sources
     participate without depending on the WAL implementation. Do not collapse
     them to `WalComponentStream` based only on current in-tree implementors.
   - Simplify the trait stack only if the revised public API retains borrowed
     custom stream implementations; otherwise keep `src/lib.rs:48-77,143-166`.

2. Establish one seeded subscription primitive.
   - `src/views/binding.rs:164-305`, `component_text.rs:19-45`, and
     `value_strip.rs:223-280` independently combine latest-state seeding,
     metadata resolution, WAL loops, and notifications.
   - Provide one concrete “latest value, then WAL updates” helper and build
     scalar, element, formatted, and on/off consumers on it.
   - Reuse it in Meter, Gauge, StateChip, TrafficLight, ComponentText,
     ValueStrip, and Viewer 3D.

3. Centralize value formatting.
   - Move the duplicated metadata/value rules from
     `src/views/value_strip.rs:1007-1103` into `src/views/format.rs` and use the
     resolved formatter from ComponentText, ComponentTable, and ValueStrip.

4. Keep `EventSource` extensible and make its contract honest.
   - Do not replace the public trait with a closed built-in enum.
   - If downstream event sources are supported, add a registration path and
     share adapter helpers among Logs/Alarms/Sequences/Msg.
   - If `generation` remains unused, deprecate and remove that method in the
     next breaking release without closing the trait.

5. Simplify small runtime stores.
   - Replace the sequence `HashMap` plus parallel declaration-order vector with
     one ordered `Vec<ChannelState>` (`src/sequences/mod.rs:66-110,196-205`).
   - Let From DB adopt the component's existing disruptor instead of copying
     every WAL sample into another ring (`src/dynamic/ops/db_source.rs:1-48`),
     using the output-adoption path already present in `DynamicNode`.
   - Keep zero-order hold as the single latest-value resampling operation after
     Phase 1 deletes the duplicate `LatestAt` layout/spec variant.

6. Remove duplicated node-editor identity and lookup layers without preventing
   downstream operation registration.
   - Assess whether stable `NodeSpec::op_tag()` can be the public descriptor
     identity instead of repeating `NodeSpecKind`, descriptor `kind`, and
     `family_op_id` (`src/node_editor/spec.rs:92-158`, `registry.rs:69-447`).
     Preserve an extensible operation descriptor API.
   - Replace seven label/from-label scan pairs with two typed lookup helpers
     (`src/node_editor/inspector_rows.rs:993-1144`).
   - Build topological indegrees/children in one edge pass instead of having
     every node rescan all edges (`src/node_editor/graph.rs:153-162`).

7. Share concrete graph helpers.
   - Use one cubic curve sampler/path builder for paint and hit testing in
     `src/graph_canvas.rs:154-257,341-459`.
   - Remove `LayoutInput::tie_break`; every caller passes identity order, so
     node indices already provide the deterministic tie break
     (`src/graph_layout/mod.rs:109-116`).

Expected reduction: approximately 200-450 lines and fewer counters, duplicate
stores, identity concepts, and repeated helper paths without closing public
stream or event-source abstractions.

## Phase 5: narrow plot sharing to proven duplication

Share the XY/List shell without building a generic plot engine.

1. Move the common legend into `views/plot_common.rs` and use it from Time
   Series, XY, and List plots.
2. Move the numeric underlay/overlay painters out of XY into `plot_common` so
   List Plot no longer imports an XY-named implementation.
3. Extract the nearly identical XY/List canvas, pan/zoom, reset, legend, and
   inspector interaction wrapper behind a small concrete trait or helper set.
4. Keep `XyLinePlot` and `ListLinePlot` concrete. Extract free functions for
   override snapshots, title derivation, and GPU canvas submission only where
   the resulting call sites are smaller.
5. Do not introduce `LineBackend`, `LinePlotCore<B>`, generic Facet-derived
   aliases, or extension slots for Time Series. Time Series has distinct
   multi-axis, cursor, measurement, alarm, and event behavior.

Expected reduction: approximately 250-450 lines with lower TypeId/reflection
risk than the fully generic design.

## Module disposition

This table records the module-by-module scan so cohesive modules are not churned
merely for completeness.

| Module | Disposition |
|---|---|
| `lib.rs` | Retain intentional stream/view extension APIs; document the supported public surface. |
| `main.rs` | Keep; server/connection lifecycle is already direct. |
| `app.rs` | Keep builder/extension hooks; remove `Option::take` startup scaffolding and add direct view registration. |
| `config.rs` | Share only a small `panel_data_dir`; keep tolerant JSON reads explicit. |
| `hydration.rs` | Keep; the cloneable synchronized global is justified. |
| `gpu_context.rs` | Keep; adapter/device fallback policy is real behavior. |
| `msg_ingest.rs` | Keep; stable backfill and boundary dedup are subtle and shared. |
| `alarms/` | Deprecate unused generation counters if public; after compatibility review remove old per-definition ingest. |
| `logs/` | Remove private dead indexing; deprecate public generation state before removal; retain filtered indexing. |
| `connections/` | Keep downstream backend/options APIs; simplify only implementation state proven unobservable. |
| `dynamic/` | Delete duplicate `LatestAt` outright, adopt DB WAL directly, and retain/deprecate public methods deliberately. |
| `sequences/` | Replace map + order vector with one ordered store; remove generations. |
| `wiring/` | Deprecate `updated_at` if public before removal; retain latest-good/error fold. |
| `plot_events/` | Keep the source trait extensible; remove/deprecate unused generation and share built-in adapter helpers. |
| `presets.rs` | Keep disk and target-shipped presets as library/target extension surfaces. |
| `tiles/` | Move config ownership to views; consume the shared registry through a generic registered-pane host. Keep `drag.rs`. |
| `node_editor/` | Keep the feature; delete duplicate palette/identity/lookups. Keep worker/validation/config boundaries. |
| `graph_canvas.rs` | Share curve sampling and painting. |
| `graph_layout/` | Remove identity tie-break input; retain the tested rank/order/coords/route stages. |
| `inspector/` | Wire the attribute grammar, collapse `FieldOverride`, and keep public type/palette registration plus live row widgets. |
| `transient/` | Remove `Dispatch`; retain the chord tree and menu. |
| `theme.rs` | Keep runtime theme ownership extensible; simplify clones only if custom themes remain supported. |
| `icons.rs` | Treat variants/assets as public until deprecated; remove the hard-coded dark tint. |
| `window_controls.rs` | Keep; platform-specific CSD and resize hit testing justify the code. |
| `views/dashboard/` | Keep as a core host; consume the shared registry, use view-owned configs, one live map, and derived IDs. |
| `views/time_series/` | Own config/snapshot; remove dead GPU IDs; share legend only. Keep focused axis/bounds/cursor/event/measurement modules and shaders. |
| `views/xy_plot/` | Share the numeric shell with List; remove mutex state from the two-step trace picker. |
| `views/list_plot/` | Keep as a core vector/FFT view; share only the numeric shell. |
| `views/{meter,gauge,state_chip}/` | Reuse one seeded scalar binding; keep distinct visual/config types. |
| `views/attitude.rs` | Keep; its multi-element semantics are distinct. |
| `views/component_browser/` | Remove the internal unused DB argument; retain or deprecate the public event deliberately. Keep tree/filter behavior. |
| `views/component_table.rs` | Remove the dead row WAL task; reuse shared trace construction/formatting. |
| `views/component_text.rs` | Keep small and read-only; use shared subscription/formatting. |
| `views/{column_browser,table}.rs` | Keep separate; generic table/browser abstractions and scrollbar behavior are justified. |
| `views/data_table/` | Keep; lazy cell materialization is performance work, not a source simplification. |
| `views/value_strip.rs` | Keep its real editing behavior; reuse subscription and formatting only. |
| `views/{traffic_light,traffic_light_grid}.rs` | Keep distinct; use seeded subscription and view-owned configs. |
| `views/viewer_3d/` | Own config/snapshot and simplify repository construction; retain/deprecate public mutators deliberately. Keep Bevy bridge modules. |
| `views/system_graph/` | Keep; config/layout/inspector separation is sensible and tested. |
| `views/{sequence_panel,sequence_grid,sequence_control}.rs` | Keep distinct full/detail/compact/control surfaces. |
| `views/{alarm_panel,log_panel,monitor,json_tree}.rs` | Keep; each has a live, focused role. |
| `views/{binding,format,plot_common,scrollbar,tooltip,lazy_pool}.rs` | Retain as the homes for the small shared behavior described above. |

## Existing plan disposition

| Existing plan | Decision |
|---|---|
| `widget-kind-registry.md` | Retain and revise. Build one public cross-host registration; use callback-based add providers rather than a closed `AddFlow` enum. |
| `panels-split.md` | Narrow and defer. Keep local row dedup; reassess file splits after configs/wrappers leave the file. |
| `plot-shell-unification.md` | Retain narrowly. Share legend, numeric painters, and XY/List wrapper; skip the generic line core. |
| `inspect-attr-grammar.md` | Retain option A: wire the grammar, migrate overrides, and delete variants that remain unimplemented. |
| `node-editor-worker-batching.md` | Exclude from simplification. The doorbell may be a valid operational fix; batching adds concepts and should require profiling. |
| `render-path-perf.md` | Keep as a separate performance backlog; its pools/caches/memos add state. |
| `service-discovery-phase2-gossip.md` | Keep/defer as a separate cross-workspace feature plan. |

## Validation and delivery

Each implementation pull request should:

1. Run `cargo fmt --all -- --check`, `cargo check -p metor-panel`,
   `cargo test -p metor-panel`, and `cargo clippy -p metor-panel` from the
   workspace root.
2. Treat public APIs as used unless they are explicitly deprecated and removed
   under the crate's compatibility policy. A workspace search alone is not
   sufficient evidence for deletion.
3. Add or preserve golden JSON fixtures only for the current layout/config
   shape. Do not add old-layout migrations or compatibility fixtures.
4. Add an external-style integration test that registers a custom view once,
   creates it in Dashboard and tiles, opens its inspector, snapshots it, and
   reloads it from both host formats.
5. Verify unknown registered kinds round-trip their raw config while the
   providing plugin is absent, then hydrate correctly when it is installed.
6. Smoke-test panel creation, inspection, save/reload, and deletion for every
   affected built-in pane/widget kind, including Dashboard connectors and edit
   interactions.
7. For plot-shell changes, manually verify pan, zoom, reset, legend toggles,
   inspector opening, resize/readback, and layout round-trips on XY and List.
8. Record before/after non-test Rust LOC. Reject abstractions that do not achieve
   a net reduction or a clear single-owner invariant.

## Expected result

The estimates overlap and should not be summed mechanically.

- With Dashboard, List Plot, and the public library surface retained: roughly
  1,000-1,800 lines removed, depending on how many thin panel/config/add-flow
  paths the shared registry replaces.

The more important outcome is structural: views as the sole owners of their
state, Dashboard and tiles reduced to placement/interaction hosts, and one
complete public registry replacing incomplete registries plus hard-coded side
tables. Adding a downstream view should require one registration and no edits
to built-in dispatch code.
