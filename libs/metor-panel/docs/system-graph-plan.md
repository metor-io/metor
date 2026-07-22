# System Graph tile — plan of record

A read-only node-and-wire view of a running FSW's wiring topology, fed by the
`WiringManifest` wkt telemetry message (`metor-proto-wkt`, id `[224, 61]`,
payload `{ ir_version: u32, ir_json: String }` where `ir_json` is the
`metor-fsw-2` `Wiring` IR serialized as JSON). The tile renders systems, slots,
edges, and scope groups with a deterministic auto-layout; positions and view
state persist panel-side like any other tile.

## Milestones

Each milestone is a separate commit on the worktree branch.

1. **`refactor(panel): extract shared graph canvas primitives from node_editor`**
   Pull the paint / hit-test / pan primitives out of `node_editor/pane.rs` into
   a shared module (`src/graph_canvas.rs`), parameterized on caller data:
   `paint_grid`, `paint_bezier`, `hit_test_edges`, `cubic_bezier_point`,
   `point_segment_distance`, and the pan-only `Viewport` convention
   (`screen = graph - viewport`). `NodeEditor` is rewired onto it with **zero
   behavior change** — same constants, same math. Verified by
   `cargo build`/`cargo test -p metor-panel` plus a diff review confirming the
   extracted bodies are byte-identical to the originals.

2. **`feat(panel): WiringManifest store`**
   A store module (`src/wiring/`) shaped like `alarms`/`sequences`: a pure
   `WiringState` fold plus a `WiringStore` gpui entity with a `Global` handle
   and `try_global`. Single-source, latest-wins fold over `WiringManifest`,
   ingested through the existing `msg_ingest::IngestSource`/`ingest_all` path.
   Decode is `serde_json::from_str::<metor_fsw_2::ir::Wiring>(&ir_json)`.
   Failure modes are held as an error string in state, never a panic:
   bad JSON → error state; `ir_version` mismatch → surfaced error string.
   Unit-tested with a synthetic manifest (fold replaces on re-emit; bad JSON;
   version mismatch).

3. **`feat(panel): system graph tile`**
   The tile itself: layout, scope groups, rendering, interaction, persistence,
   registration, palette entry, theme tokens, inspector rows. Unit-tested for
   deterministic layout (incl. a cycle broken by a delayed edge, and a scoped
   fixture) and collapse/re-route logic.

The plan doc itself is committed first as
`docs(panel): system graph tile plan`.

## Key decisions

- **Reuse over reinvention.** The node_editor rendering/interaction layer is the
  reusable machinery; its data model (`NodeGraph`/`GraphCoordinator`/worker/
  `OpDescriptor`) is dynamic-signal-specific and is *not* reused. M1 extracts the
  primitives so both panes share one canvas layer.
- **Store shape mirrors `alarms`/`sequences`.** Pure fold + gpui entity + global,
  fed by `msg_ingest`. The store always reflects the *latest* manifest (live
  fold); no historical scrub (no infrastructure for it — out of scope).
- **Nodes.** One card per `system` and per `slot`, visually distinct: slots show
  their occupant list + initial occupant; systems show `ty` and a `process`
  badge. The reserved `coordinator` instance appears only if it has edges.
- **Edges.** Bezier wires. `kind: Frame` vs `Msg` distinguished by theme tint;
  `delayed: true` rendered dashed. Edge hover/click selects and shows detail
  (`from.out → to.in_`, kind, delayed), reusing the bezier hit-test. Edge
  identity is structured (endpoints + ports) so live-data overlay could join
  against telemetry later.
- **Scope hierarchy.** Scopes (from `scopes[].parent`) render as collapsible
  group containers; a collapsed scope becomes one aggregate node and edges to its
  members re-route to the group node. Flat targets (no scopes) render flat.
- **Auto-layout.** Deterministic layered layout: topological layering over
  non-delayed frame edges (delayed and msg edges excluded from layering, drawn as
  back/side edges); layers left→right; within-layer order minimized by a
  barycenter pass. Manual drag overrides layout. Overrides, the collapsed-scope
  set, and viewport pan persist in the `PaneItem` `Config`. A "re-layout"
  affordance clears overrides.
- **Details.** Clicking a node opens the standard inspector (via
  `InspectEntity` + a proxy entity registered with `InspectorRegistry`), showing
  name/ty/artifact/params-summary/src anchor as read-only rows — no bespoke
  popup.
- **Theming.** Zero hardcoded `Hsla` outside `theme.rs`. New tokens as needed
  (frame-edge vs msg-edge tint) added to the `Theme` struct.

## Out of scope

- Historical scrub of the manifest (no infrastructure; store holds latest only).
- Live edge-activity overlay (edge identity is structured so it can be added
  later, but no rendering of it in this pass).
- Editing the topology — the tile is strictly read-only.
- Any change to `metor-fsw-2` (dependency only; a sibling agent owns that crate).

## Merge notes

Files a sibling might also touch: workspace `Cargo.toml` / `Cargo.lock`
(adding `metor-fsw-2` + `serde_json` deps to `metor-panel`), and nothing inside
`metor-fsw-2` (consumed as a dependency only).
