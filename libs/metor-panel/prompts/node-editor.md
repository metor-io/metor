I'd like to design the next phase of dynamic components in metor-panel: a node-based editor that lets users compose dynamic components at runtime, on top of the toolkit that already landed.

## Phase 1 (already shipped) — `metor-panel/src/dynamic/`

The runtime toolkit is in place. Familiarize yourself before planning:

- **`DynamicNode` trait** (`src/dynamic/node.rs`): every node owns a `metor_db::disruptor::Disruptor` whose frames are `[Timestamp i64][value bytes]`. Identity is `NodeId(u64)` — a content hash of `(op_tag, args, parent_ids)` so re-issuing the same construction returns the same node.
- **`ValueType`**: `Clock` (timestamps only) or `Value(ComponentSchema)` (PrimType + dim). Each node also exposes `parent_clock_id(): Option<NodeId>` so composers can require co-clocked inputs.
- **Op constructors** (`src/dynamic/ops/`):
  - `clock::fixed_rate(hz)`, `clock::clock_of(node)`
  - `generators::{sin, square, random, constant}`
  - `derive::{scale, offset, abs, neg, log}` (f64 scalar)
  - `compose::{add, sub, mul, mean}` (require co-clocked inputs; `BuildError::ClockMismatch` on violation)
  - `resample::{zoh, linear, latest_at}`
  - `db_source::from_db(db, component_id)` — bridge a real DB Component into the graph
  - `persist::persist(db, name, node)` — register the node as a real `db::Component`, getting the on-disk TimeSeries + every existing view integration (browser, plot, monitor) for free
- **`DynamicRegistry`** (`src/dynamic/registry.rs`): gpui global, installed in `app.rs::run`. Holds `HashMap<NodeId, Arc<dyn DynamicNode>>`. `reconcile(alive: &HashSet<NodeId>)` is set-diff reconciliation — drops nodes not in `alive`, which cancels their producer tasks via `JoinHandleDropGuard`.
- **View bridge**: `Arc<dyn DynamicNode>` implements `ComponentStreamBuilder` (the trait was generalized with an associated `Stream` type). Any existing view that takes `impl ComponentStreamBuilder + Send + 'static` accepts dynamic nodes unchanged.

What's intentionally **not** in Phase 1: the editor UI, the graph data structure that drives `reconcile`, graph serialization, and inspector entry points to add nodes.

## What I want for Phase 2 — the editor

A runtime node-based editor where users compose dynamic components visually:

1. **UI base**: fork `gpui-flow` at `/Users/sphw/code/os/gpui-flow` and rebrand against the metor-panel theme (`src/theme.rs`). Match the chrome of inspector / tiles / dashboard.
2. **Graph as data**: a serializable graph (`Vec<NodeSpec>` + edges, or some equivalent) whose nodes carry `(op_tag, args, input_ids)`. The graph computes a `NodeId` per node by hashing the same way `dynamic::node::hash_id` does, so the runtime registry can be driven directly off the graph.
3. **Typed sockets**: each socket has a `ValueType` (`Clock` | `Value(schema)`). Edges only connect compatible types. Surface `BuildError` from the toolkit constructors as red edges / inline errors. Sockets should also color-code by `parent_clock_id` so users can see at a glance whether a composer's inputs are co-clocked.
4. **Persistence**: serialize alongside dashboard presets (`src/presets.rs`, currently using `facet_json` against `~/.config/metor/panel/presets/*.json`). Bump the `SUPPORTED_LAYOUT_VERSION` if needed. The node graph is per-preset so swapping presets swaps the active graph.
5. **Inspector entry**: add a palette provider that lets the user spawn nodes via `cmd-p`. Reuse existing inspector rows (`BoolRow`, `NavRow`, `EnumRow`, etc., per `feedback_extend_rows_in_place.md`) for argument editing — don't fork a new row family.
6. **Reactive reconciliation**: when the graph mutates (add/remove node, edit arg, reroute edge), recompute `NodeId`s and call `DynamicRegistry::reconcile`. Hash-based identity means edits naturally invalidate downstream nodes, and unchanged subtrees stay live across edits.
7. **Pane integration**: a new pane item type "Node Editor" that hosts the gpui-flow canvas. Persist its open/closed state through the existing tile serialization (`src/tiles/serial.rs`).

## Things to think about while planning

- **Where the graph lives**: a gpui `Entity<NodeGraph>` per editor pane, or a single app-wide global with multiple views? Probably per-pane — multiple graphs feels right.
- **Inspector palette of ops**: each op constructor needs metadata (name, category, arg specs, socket types) to render in a palette. A `OpDescriptor` registry hashed by `op_tag` is the natural shape.
- **Validation timing**: do we run constructors eagerly on every edit (catches errors immediately, rebuilds the world on every keystroke) or only on commit? A debounced "rebuild on idle" is probably the sweet spot.
- **Visual surface for `persist`**: it's the only op with a side effect (creates a real DB component). Should be visually distinct, and the `name` arg should validate against existing component names.
- **Keep Phase 2 testable**: graph serialization round-trip and a `graph::reconcile_against(&mut DynamicRegistry)` should both be unit-testable without launching the UI.

## Non-goals for Phase 2

- Re-doing anything in `src/dynamic/` — the toolkit is the contract. If a new op is needed, add it under `ops/`.
- A Python/JS scripting interface — keep it visual-only.
- Multi-user collaboration on the same graph.

----

To start make a plan for the node editor, read the existing `src/dynamic/` code and `gpui-flow`, and plan to make most changes in a new `src/node_editor/` module (next to `inspector/`, `tiles/`, `views/`). Please ask any clarifying questions before finalizing.
