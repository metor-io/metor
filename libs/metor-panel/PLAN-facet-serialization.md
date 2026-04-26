# Facet-based serialization for metor-panel

## TL;DR

**Yes, this is feasible.** The fork at `sphw/facet-gpui` already gives us:

- A `Facet` impl for `Entity<T>` (exposed as a `Pointer` def with `inner = T::SHAPE`)
- `#[facet(skip)]` / `#[facet(opaque)]` / `#[facet(default)]` for the data-vs-state split
- The `Peek` (read) and `Partial` (build) reflection APIs

**The real missing piece is a JSON-format implementation.** The `facet` crate is format-agnostic and our local checkout has no `facet-json` sibling. We will write a small format walker in-tree — a few hundred lines — because it has to be aware of two metor-panel-specific concerns (Entity round-trip and `&App` access) that no off-the-shelf crate would handle anyway.

## Current state (what we are throwing out)

The "half-baked" serialization is contained:

- `tiles/serial.rs` (124 lines) — `SerializedTileGroup`, `SerializedMember`, `SerializedSplit`, `SerializedPane`, `SerializedItem`, `ItemRegistry`. Pure serde.
- `tiles/item.rs:17` — `PaneItem::serialize(&self, &App) -> serde_json::Value`. Custom per-panel.
- Each panel in `tiles/panels.rs` and `views/dashboard/mod.rs` writes its own `serde_json::Value` blob; some inner state is just lost on round-trip (e.g. `PlotPanel` only persists a label).
- `views/dashboard/mod.rs:37-93` — `WidgetId`, `WidgetRect`, `WidgetKind`, `DashboardWidget`. Pure serde with an opaque `serde_json::Value` payload per widget.
- `tiles/pane.rs:30` — `TabOrientation` derives serde.
- `tiles/drag.rs:52` — `SplitDirection` derives serde (transient drag payload — does not need to persist).

There is **no file I/O**. `TileGroup::serialize` / `deserialize` exist but nothing calls them with a real path. So the on-disk format is not yet fixed — we can replace freely.

## Why Facet wins here

The inspector already drives off Facet attributes. Today we mark transient fields `#[facet(skip)]` / `#[facet(opaque)]` so the inspector doesn't show them. With Facet-based serialization, the **same attributes also drive persistence** — there is one source of truth instead of "skip in inspector AND skip in serde AND remember to update both."

Examples that already match the desired model perfectly:
- `views/time_series/line_plot.rs:75` — `LinePlot` has POD fields (traces, ranges, overrides) and `#[facet(opaque)]` runtime state (db, tasks, gpu_state). Identical separation we want for serialization.
- `views/viewer_3d/mod.rs:100` — `Viewer3d` separates `models` / `camera_fov` from opaque GPU/DB state.
- `views/time_series/mod.rs:506` — `Trace` uses `#[facet(skip)]` for routing IDs and POD fields for everything else.

Once we have a Facet→JSON walker, the "data vs state" requirement is solved by attributes already on the structs.

## Architecture

### Document shape

```jsonc
{
  "version": 1,
  "root": <SerializedTileGroup>,             // walked from TileGroup
  "entities": {                              // global entity table
    "42": { "type": "TextPanel", "data": {...} },
    "43": { "type": "Trace",     "data": {...} },
    ...
  }
}
```

- Anywhere an `Entity<T>` is encountered during the walk, it serializes as `{ "$entity": "42" }` (the original `gpui::EntityId` as a string).
- The full body of each entity goes into the `entities` map, keyed by that same id.
- Cycles are tolerated: if we have already started writing entity 42, we just emit the reference and skip rewriting.

This matches the user's recommendation. The cost over a tree-only encoding is one extra hash-map lookup per Entity — negligible.

### Deserializer two-pass

GPUI requires `cx.new(|cx| T)` to return a fully-initialized `T` synchronously. To break the chicken-and-egg cycle (entity A holds `Entity<B>`, B holds `Entity<A>`):

1. **Pass 1 — allocation**: walk `entities` map. For each `id`, dispatch on `"type"` to a registered factory that produces a default `Entity<T>` (`cx.new(|_| T::default())`). Fill the id-remap table `old_id → Entity`.
2. **Pass 2 — population**: walk `entities` again. For each id, look up its `Entity<T>`, then `entity.update(cx, |t, cx| populate_via_facet_partial(t, json, &id_remap, cx))`. Whenever the populator hits `{"$entity": "..."}`, it resolves through `id_remap` and writes the existing `Entity<T>` into the field.
3. Finally, walk `root` to build the `TileGroup` itself, which is *not* an entity but does refer to entities via the same `$entity` mechanism.

`T: Default` is needed for every entity-inner type. All current panels already trivially construct from a `db` handle — we will require `T: PanelDefault` (a tiny trait we add) that takes `&App` so the factory can pull the DB / theme out of globals. This avoids requiring `Default` on types that legitimately need context.

### Trait-object dispatch (`Box<dyn PaneItemHandle>`)

Facet has no native trait-object support, so the dispatch must be explicit. Two viable options:

**Option A — Tagged enum.** Replace `Vec<Box<dyn PaneItemHandle>>` in `Pane.items` with a Facet enum:
```rust
#[derive(Facet)]
pub enum Panel {
    Text(Entity<TextPanel>),
    Table(Entity<TablePanel>),
    DataTable(Entity<DataTablePanel>),
    Browser(Entity<BrowserPanel>),
    Plot(Entity<PlotPanel>),
    Viewer3d(Entity<Viewer3dPanel>),
    Dashboard(Entity<DashboardPanel>),
}
```
The `PaneItemHandle` trait becomes a thin `match` instead of a vtable. New panels are added by extending the enum — a one-line change in three places (variant, match arms in the `PaneItemHandle` shim, registration at startup).

**Option B — String-keyed registry.** Keep the trait object, register a `(name → factory)` map exactly like the current `ItemRegistry`, but the factories build via Facet `Partial` instead of accepting `serde_json::Value`. Less invasive but keeps the indirection.

**Recommendation: Option A.** The set of panel kinds is closed (we own all of them), the enum is more searchable and refactor-safe, and the inspector's whole-type builder map already keys on `TypeId` so nothing in the inspector path needs to change. The downside is one large `match` per trait method, but those methods are short.

### The JSON walker

Not a from-scratch serde-equivalent — a focused converter targeting `serde_json::Value` (we already depend on it through metor-db). Two functions:

```rust
fn write<'a, T: Facet<'a>>(value: &T, cx: &App) -> Value;
fn read<'a, T: Facet<'a> + 'static>(value: &Value, cx: &mut App) -> Result<T, Error>;
```

Internally:
- **Write side** walks `Peek` and dispatches on `Def`:
  - `Scalar` — branch on `ConstTypeId` for primitives and `SharedString`/`Hsla`. `Display` round-trip for `parse`-aware scalars (Hsla already provides this).
  - `Struct` — emit `{}` of fields, skipping `FieldFlags::SKIP` (and treating opaque-without-proxy as skip).
  - `Enum` — externally tagged: `{ "VariantName": <fields> }` or just `"VariantName"` for unit variants.
  - `List` — array via the list vtable.
  - `Pointer` — if `module_path == "gpui"` and shape name is `"Entity"`, treat as Entity (via the special path described above). Other pointers (Box/Arc/Rc) walk through their pointee.
  - `Option` — `null` / value.
- **Read side** uses `Partial` to build values, walks the JSON in parallel. The Entity case looks up the id-remap and writes the `Entity<T>` into the field directly using the field's existing `set` machinery.

The walker lives in `tiles/serial.rs` (renamed) or a new `serial/` module. Estimated ~600-800 LOC including error handling, tests, and the registry of factories.

### Skip rules for serialization

We treat the following Facet attributes consistently:

| Attribute | Inspector | Serializer | Deserializer |
|---|---|---|---|
| `#[facet(skip)]` | hide field | omit | use `Default` |
| `#[facet(opaque)]` no proxy | render as opaque | omit | use `Default` |
| `#[facet(opaque, proxy = ...)]` | render via proxy | round-trip via proxy | round-trip via proxy |
| `#[facet(skip_serializing)]` | show | omit | required |
| `#[facet(skip_deserializing, default)]` | show | emit | use `Default` |

The default policy is "an unmarked field is persisted." This requires a sweep through the existing structs — most opaque fields already exist; we just have to verify each one is genuinely runtime-only. This is enumerated in Phase 4.

## File / module impact

| File | Change |
|---|---|
| `tiles/serial.rs` | **rewritten**: holds the JSON walker, entity table, and `PanelKind` enum. |
| `tiles/item.rs` | drop `serialize`/`serialization_key` from `PaneItem` and `PaneItemHandle`; the trait becomes purely runtime (tab title, view, can_close, inspectable_entity). |
| `tiles/mod.rs` | `Member`, `SplitAxis`, `TileGroup` derive `Facet`; `Member::serialize` deleted; `TileGroup::deserialize` becomes a thin call into the walker. |
| `tiles/pane.rs` | `Pane` derives `Facet`; runtime fields (`drag_split_direction`, `content_bounds`, `tab_scroll`) marked `#[facet(skip)]`. `TabOrientation` swaps serde for Facet. `Pane.items` becomes `Vec<Panel>` (Option A) or stays trait-object (Option B). |
| `tiles/drag.rs` | `SplitDirection` keeps no derives — never persisted. |
| `tiles/panels.rs` | every panel struct derives `Facet`, marks transient state, drops the custom `serialize`/`serialization_key`. Inner `Entity<T>` references are walked automatically. |
| `views/time_series/{mod,line_plot}.rs` | already use Facet — verify each `#[facet(opaque)]` is correct for serialization, and that POD fields like `traces` round-trip (they will, via the entity walker). |
| `views/viewer_3d/mod.rs` | same as above. The current `Viewer3dPanel::serialize` (camera state, model list as JSON) becomes redundant. |
| `views/dashboard/mod.rs` | drop serde derives from `WidgetId`/`WidgetRect`/`WidgetKind`/`DashboardWidget`, derive `Facet`. Replace the per-widget `config: serde_json::Value` with proper Facet types — this is the one place where structure is currently lost; we need a `WidgetConfig` enum mirroring the panel enum. Mark `widget_views` / `widget_entities` skip. |
| `app.rs` / startup | call a new `register_panel_factories(cx)` for the entity allocation pass. |

External crate touch:
- The fork at `metor-io/facet#sphw/facet-gpui` already has the `Entity<T>` `Facet` impl. We may need to extend `facet-core/src/impls/crates/gpui.rs` to expose entity id-extraction in a stable way (it currently does not — the walker has to use a separate `entity_id_of_any` helper from gpui). No upstream changes blocking us.

## Open questions to resolve before coding

1. **`PaneItemHandle` enum vs trait-object** — committing to Option A above. Confirm.
2. **`PanelDefault` trait shape** — does `fn default(cx: &App) -> Self` cover every panel? `PlotPanel` requires a `Vec<Trace>`; `Viewer3dPanel` requires a DB. They can default to "empty plot" / "empty 3d scene" and have the populator fill in real data, so yes.
3. **Versioning** — top-level `version: u32`. We bump on breaking field renames; old files load with `default` for missing fields. No formal migrations until v2 ships.
4. **Where do save/load actually get triggered?** Out of scope here; the user mentioned no file I/O exists yet. We expose `TileGroup::to_json(&App) -> Value` and `TileGroup::from_json(Value, &mut App) -> Result<Entity<TileGroup>>` and let the application layer own the file path.

## Implementation phases

Phases land independently; each compiles and the app keeps running between them.

1. **Walker scaffolding**: write `serial::write` / `serial::read` for primitives, structs, lists, options, enums. Round-trip a Facet-derived test struct with no Entity. Land tests.
2. **Entity round-trip**: extend the walker to recognize `Entity<T>`, build the entity table, two-pass deserialize. Add `PanelDefault` trait. Round-trip a synthetic struct with `Vec<Entity<X>>`.
3. **Pane / TileGroup shapes**: derive `Facet` on `Pane`, `SplitAxis`, `Member`, `TileGroup`, `TabOrientation`. Introduce `Panel` enum (Option A), update the `PaneItemHandle` impls to dispatch through it. Delete `PaneItem::serialize` / `serialization_key`. The old `ItemRegistry` is deleted — its job is now done by the `Panel` enum's Facet derive plus `PanelDefault`.
4. **Per-panel sweep**: for each panel struct, derive `Facet`, classify each field (persisted vs `#[facet(skip)]` vs `#[facet(opaque)]`). Delete the custom `serialize` impls. Verify by writing → reading → comparing in a test.
5. **Dashboard**: replace `serde_json::Value` widget configs with a `WidgetConfig` enum. Drop serde from `WidgetId`/`WidgetRect`/`WidgetKind`/`DashboardWidget`. (This is the largest behavior change because dashboard currently *loses* config on round-trip.)
6. **Cleanup**: drop `serde_json` from `metor-panel/Cargo.toml` if no longer needed (probably still needed transitively); drop `serde` derives. Update `STYLE.md` / `design.md` to point at Facet attributes instead of serde for any persistence guidance.

## Risk register

- **`Entity<T>` id stability across processes** — `gpui::EntityId` is not designed to be stable across runs; we use it only as an in-document key. New `Entity` allocations after load get fresh ids. Fine.
- **Type-name collisions in the entity table** — we key the `"type"` field by a stable string we choose (`"TextPanel"`, etc.), not by Rust's `type_name`. A `const TYPE_TAG: &str` on each panel via `PanelDefault`.
- **`Default` not always meaningful** — the two-pass scheme creates a default then mutates. Some types (e.g. `Trace`) carry a `ComponentId` they cannot fabricate. Mitigation: `PanelDefault` returns `Self`, but **populators are allowed to overwrite every field**. We initialize with placeholder zeros; pass 2 always overwrites.
- **`Facet` for trait-object trees** — confirmed unsupported. The `Panel` enum is the workaround; this is also why Option A is preferred.
- **Walker maintenance burden** — ~700 LOC of low-level reflection code we now own. The alternative (waiting for upstream `facet-json`) blocks indefinitely. We accept the burden because the gpui-context requirement makes a generic crate insufficient anyway.

## Estimated effort

- Phase 1 (walker scaffolding + tests): ~1 day
- Phase 2 (entity table + two-pass): ~1 day
- Phase 3 (Pane/TileGroup + Panel enum): ~half day
- Phase 4 (per-panel sweep): ~1 day
- Phase 5 (dashboard): ~1 day
- Phase 6 (cleanup): ~half day

Total: ~5 working days for a clean, end-to-end persisted layout with Facet driving everything.
