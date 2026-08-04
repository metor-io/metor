# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this crate is

`metor-panel` is a desktop UI for live telemetry, built on [gpui](https://www.gpui.rs) (Zed's UI framework) and `metor-db` (a time-series telemetry database). It maps streams of component values to gpui elements: plots, tables, 3D viewers, traffic-light grids, etc. Users compose layouts of tiles and define data transformations as a graph of "operations" (dynamic nodes).

The binary boots a `metor-db` server on `127.0.0.1:2240` and opens the panel UI against the same DB instance. See `src/main.rs`.

## Common commands

Run from the workspace root (`/Users/sphw/code/metor/metor`) — this is a member of the larger metor workspace, not a standalone crate.

- Run the panel: `cargo run -p metor-panel`
- Build (dev): `cargo build -p metor-panel`
- Build (release): `cargo build -p metor-panel --release`
- Test the crate: `cargo test -p metor-panel`
- Run a single test: `cargo test -p metor-panel <test_name>` (e.g. `cargo test -p metor-panel node_editor::tests::`)
- Lint: `cargo clippy -p metor-panel`

The workspace pins `rustc 1.94.0` (`rust-toolchain.toml`). The `facet` family of crates is patched to a fork (`metor-io/facet`, branch `sphw/facet-gpui`) — see the workspace `[patch.crates-io]` block. Don't bump those past the pinned 0.44.x versions without coordinating the patch chain.

## Architecture

### Data flow: `ComponentStream` → views

The central abstraction is `ComponentStream` (`src/lib.rs`), an async iterator that yields borrowed `ComponentView`s. Views never own their data; they hold a stream and re-render when `next()` resolves.

- `WalComponentStream` subscribes to a component's WAL `Disruptor` (a broadcast ring buffer in `metor-db`). Each grant may contain multiple framed `[Timestamp][value]` messages — `WalView` always points at the *latest* message in the grant; views only need the freshest sample to repaint.
- `ComponentStreamBuilder` resolves any source (a `Component`, a bare `ComponentId`, or an `Arc<dyn DynamicNode>`) into a `ComponentStream`. The `ComponentId` impl waits on `db.vtable_gen` until the component appears, so views can subscribe before the producer registers.

### Dynamic nodes (`src/dynamic/`)

User-defined components are computed lazily as a graph of producer tasks. Each `DynamicNode` writes `[Timestamp][value]` samples into its own `Disruptor`. Identity is a content hash of `(op, args, parents)`, so the same expression always yields the same `NodeId` — this is what powers reconciliation in the node editor.

Construction goes through one of the `ops/` modules (`clock`, `generators`, `derive` for single-input, `compose` for multi-input, `resample`, `db_source`, `persist`). Dropping the last `Arc<dyn DynamicNode>` cancels the task. Subscribers either iterate every sample (`NodeReader`, used by downstream derivations) or pull only the latest (`WalComponentStream::from_disruptor`, used by views).

### Node editor (`src/node_editor/`)

Phase 2 of the dynamic system: the visual graph editor. Owns the serializable `NodeSpec` (per-node op + args), `NodeGraph` (data model), `GraphCoordinator` (multi-editor alive-set aggregator), `OpDescriptor` registry (palette/canvas/validator metadata), and `validate` (connection validation + edge coloring). The `worker` rebuilds dynamic nodes when the spec changes.

### Tiles (`src/tiles/`)

The split-pane layout system. `TileGroup` is the root; the tree is a recursive `Pane` / split. `SplitPath` is a `SmallVec<[usize; 4]>` locating a node by member index. Layout serialization is versioned via `SUPPORTED_LAYOUT_VERSION` — bump it lockstep with `TileGroup::serialize` when the document shape changes. `panels.rs` contains the concrete `PaneItem` types (PlotPanel, TablePanel, etc.).

### Inspector (`src/inspector/`)

A unified row-list overlay that serves as both command palette (centered) and right-click property editor (anchored). All "drill into another view" actions push a new page onto a stack inside the same inspector instead of opening separate windows. Modes are `Anchored(Point)` vs `Centered`.

- Rows live in `rows/` (one file per widget: `BoolRow`, `NavRow`, `ColorRow`, `ScalarRow`, …). When extending row capabilities, prefer adding constructors to the existing row file rather than creating parallel row types.
- Field rendering is driven by **facet attributes** (`#[facet(inspect::label = "…")]`, `inspect::range(min=…,max=…)`, and `inspect::variants = "…"`). The grammar is defined in `src/inspect.rs` via `facet::define_attr_grammar!`. The grammar lives outside the derive crate because the macro needs to resolve `Attr` at the call site.
- `registry/` chooses a row builder based on the facet type + attributes.
- Inspector requests cross pane boundaries via the `InspectEntity` gpui action and a global `OpenInspectorCallback`.

### Views (`src/views/`)

Concrete renderers — plots (`time_series`, `xy_plot`, `list_plot`), tables (`data_table`, `component_table`, `table`), 3D scene (`viewer_3d`, built on Bevy as a render pipeline embedded in gpui via wgpu), traffic lights, value strips, dashboards, etc. They consume `ComponentStream`s and emit `Inspectable` rows.

Scalar instruments (`meter`, `gauge`, `state_chip`, `attitude`) share `views/binding.rs`, which owns stream seeding, late metadata binding, and reading warn/critical limits out of the alarm store. A new instrument binds through it rather than growing its own copy, and never takes limits as configuration.

Every one of those is registered on **both** surfaces from a single config type: a `PaneItem` in `tiles/panels.rs` plus a `WidgetKind` in `views/dashboard/widgets.rs`. `TrafficLight` is the reference for the pattern.

`views/dashboard/connectors.rs` adds schematic lines over the dashboard canvas — anchors resolve against live widget rects each frame, `on_top` picks whether a line paints under the widgets (a pipe) or over them (a callout leader), and `bind` colours a line from telemetry. Line geometry lives in `graph_canvas.rs` alongside the node editor's.

### Theme (`src/theme.rs`)

All colors live in the `Theme` struct, accessed as `DARK.selection_bg` etc. **Never** hardcode `Hsla` literals outside `theme.rs`. `hex(0xRRGGBB, alpha)` is a `const fn` for theme tables.

## Conventions

The crate has its own style guide at `STYLE.md` and design notes at `design.md` — read both before non-trivial changes. Highlights:

- Doc comments (`///`) explain *design intent and how a type fits the system*, not what the code does. Inline comments only for non-obvious logic. No section dividers.
- Getters drop `get_`. Cyclic transitions use `cycle()` not `next()`. Name for intent (`drop_tab`) not implementation (`move_or_insert_tab`).
- `pub(crate)` for internal submodules; re-export from the parent without renaming (`tiles::Pane`, not `tiles::TilePane`).
- `SmallVec` for small clone-heavy vectors (with type aliases near use). `SharedString` instead of `String` for display text. `&[T]` over `Vec<T>` in parameters when read-only.
- `Arc` and gpui `Entity` clones are cheap — don't contort code to avoid them. The `'static` closure clones gpui forces (canvas, event handlers) are acceptable.
- No permanent `#[allow(dead_code)]`; no event variants that never fire.

The crate-wide `#![allow(clippy::arc_with_non_send_sync, clippy::type_complexity)]` in `lib.rs` is load-bearing — gpui's APIs require `Arc<dyn Fn>` for non-Send closures, and the closure types fall out of gpui's API.

## gpui

gpui sources for this checkout live at `/Users/sphw/code/os/zed/crates/gpui` — read them when behavior is unclear. gpui is single-threaded by design; that's why `Arc<dyn Fn + 'static>` is everywhere instead of `Rc`.
