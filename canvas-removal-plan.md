# Canvas system removal

Decision (2026-08-30): drop the entire canvas system — the "Graph" tile (metor-expr
program editor + system flowchart), including program-run reconciliation. The
Execution Timeline pane supersedes the topology view; user computation remains
available through the `=` expression tier. Saved Graph panes degrade via the
existing UnknownPane fallback (renders "Missing view", blob round-trips
byte-identical — covered by `unknown_pane_round_trips_raw_kind_and_state`); no
migration code.

Keep untouched: `dynamic/expressions.rs` (`=` tier), `src/wiring/` (WiringStore),
`graph_canvas.rs` line primitives (exec_timeline + dashboard connectors),
`views/exec_timeline/`, metor-expr language (resample_*/window/fft builtins stay —
fsw2 shares the language).

## Deletions

- `src/canvas/` entirely (mod, model, edit, run, legacy, migrate, palette + their
  test modules; 32 tests die). `canvas/run.rs` was the only stage instantiator
  (`op_tag::EXPR_STAGE`); `legacy.rs`/`migrate.rs` were reachable only from the
  `node_editor` alias in app.rs.
- `src/views/system_graph/` entirely, after relocating one function set (below).
- `src/graph_layout/` entirely (canvas/system_graph were its only consumers).
- `src/lib.rs`: `pub mod canvas;`, `pub(crate) mod graph_layout;`.
- `src/views/mod.rs`: `pub mod system_graph;`.
- `app.rs` `register_pane_item_deserializers`: the `GraphCanvas` registration
  (claims `"system_graph"`), the `"program"` legacy alias block, the
  `"node_editor"` legacy alias block.
- `tiles/panels.rs`: the "Graph" `CommandRow` in `new_panel_rows`.
- `dynamic/ops/clock.rs` and `dynamic/ops/resample.rs` (canvas-only:
  `canvas/run.rs`/`legacy`/`migrate` were the sole non-test callers), plus
  `node.rs::require_clock` and the `op_tag::{FIXED_RATE_CLOCK, ZOH, LINEAR,
  EXPR_STAGE}` constants, and the resample-stage test
  `a_resample_stage_is_wired_from_the_manifest` in `ops/program_tests.rs`
  (`a_source_system_clocks_itself` etc. use the compiled `@system(rate=)` path
  and stay). Known consequence: `=` expressions containing `resample_*` compile
  but instantiate no stage — same as after run.rs goes regardless; the language
  keeps the builtins for fsw2.
- Now-dead halves of `graph_canvas.rs`: `RoutePoints`, `paint_grid`,
  `paint_route`, `hit_test_edges` (canvas was their only caller; no
  `#[allow(dead_code)]` per style). `distance_to_line`, `paint_line`,
  `drawn_polyline`, `paint_arrowhead`, `orthogonal_points`, `LineStyle`,
  `LineShape`, `LINE_HIT_RADIUS` stay (dashboard + exec_timeline).
- `app.rs` keybinding tests: delete `deleting_a_card_never_fires_from_inside_a_field`;
  rename the `"GraphCanvas"`/`"NodeEditor"` string literals in the surviving
  predicate-parsing tests (they test gpui parsing, any host name works).
- `docs/system-graph-plan.md`.

## Relocation

`views/system_graph/inspector_rows.rs` → keep only the wiring-backed detail
pages the Gantt gutter opens: `SelectedGraphNode` + `build_rows` +
`system_rows`/`slot_rows`/`scope_or_coordinator_rows`/`program_source`/
`scope_path_of`/`text_row`/`src_summary`/`params_summary`. Move into
`views/exec_timeline/inspector_rows.rs` (already has `register_inspector_rows`);
swap `super::layout::COORDINATOR_INSTANCE` → `exec_timeline::rows::COORDINATOR`
(already duplicated there). Drop the `GraphCanvas` type-builder half
(`build_panel_rows`, `direction_label`). Collapse the `app.rs:1199` registration
call into exec_timeline's. Update the consumer in `views/exec_timeline/mod.rs`.

## Docs

CLAUDE.md: delete the `### Canvas` section; reword the Disruptor line ("powers
reconciliation in the canvas"); update the ops list (`db_source`, `persist`,
`replay`, `program` remain); replace the `canvas::migrate::tests::` example
command; reword the `graph_canvas.rs` "node editor" line.

## Verification

`cargo build -p metor-panel`, full `cargo test -p metor-panel` (expect ~347 =
379 − 32 dead − 1 keybinding test + 0 new), `cargo clippy -p metor-panel`
(no new warnings, especially no dead_code), `cargo fmt -p metor-panel`.
Manual: panel launches, New Panel palette has no "Graph" row, a layout saved
with a Graph pane opens as "Missing view: system_graph" and re-saves intact,
Gantt gutter click still opens the system detail page, `=` expressions still
evaluate, dashboard connectors still draw.
