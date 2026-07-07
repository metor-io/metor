# Node-editor worker: batch rebuilds, stop idle polling

## Goal

Two independent wins in the dynamic-node build path:

1. **One round-trip per rebuild, not one per node.** A rebuild of a freshly
   hydrated N-node graph currently makes N sequential blocking calls onto the
   worker thread. Ship the whole topo-ordered build batch as a single message,
   keyed by `FlowId`, and block the gpui main thread exactly once.
2. **No idle wakeups.** The worker loop polls its inbox on a 2 ms timer forever.
   Replace the poll with an executor-friendly wait so the worker sleeps until a
   message actually arrives — zero CPU when nothing is building.

Constraint that shapes everything below: the worker thread runs
`stellarator::run(...)` and the dynamic-node *producer* tasks (clocks,
generators, `persist`) are `stellarator::spawn`ed onto **that same thread's
executor**. So the worker's "wait for a message" step must *yield to the
executor* — a plain thread-blocking `crossbeam` recv would freeze every
producer and stop telemetry. See "Why not a synchronous blocking select".

## Current state

Files: `src/node_editor/worker.rs`, `src/node_editor/graph.rs`
(`rebuild_into`), plus callers `src/node_editor/coordinator.rs` and
`src/node_editor/pane.rs` (200 ms debounce via `schedule_rebuild`).

### The per-node round-trip

`NodeGraph::rebuild_into` (`graph.rs:204`) walks the graph in topological order
and, for each node that must be constructed, calls `WorkerHandle::run`
(`graph.rs:271`) with a build closure:

```rust
w.run(Box::new(move || build_spec(&spec_for_worker, parent_arcs, &db_for_worker)))
```

`WorkerHandle::run` (`worker.rs:59`) sends one `BuildJob` and then blocks the
gpui main thread on `reply_rx.recv()` (`worker.rs:72`). The worker
(`run_worker`, `worker.rs:128`) sits in a loop that `try_recv`s both channels
and, when empty, `stellarator::sleep(Duration::from_millis(2)).await`
(`worker.rs:147`).

Each build must be its own round-trip because the closure for node *k* captures
`parent_arcs` — the `Arc`s of *k*'s parents — which only exist after the parents
finish building earlier in the same loop. So the loop is inherently sequential
across the worker boundary today.

### Cost for an N-node hydrated graph

"Hydrated" = a saved layout just loaded, so `DynamicRegistry` is empty:
`topo_order` yields all N nodes, none hit `registry.get`, none are
idempotent-skippable. Every node takes the `w.run` path.

- **Round-trips: N** (one blocking `send` + `recv` per node).
- **Main-thread stall: up to ~2·N ms, ~N ms average.** Each `recv` waits for the
  worker, which is parked in `sleep(2ms)`; the reply lands 0–2 ms later
  (~1 ms mean) *plus* the build cost. These stalls are sequential and all run
  **inside a single `graph.update` on the gpui main thread**. At N = 30 that is
  up to ~60 ms — 3–4 dropped frames at 60 Hz, felt as a hitch when opening a
  saved dashboard.
- **Idle burn: ~500 wakeups/sec, forever.** Even with no graph open, the worker
  wakes every 2 ms to run two `try_recv`s and re-arm the timer, keeping the core
  from idling (battery cost on laptops).

### How per-node errors are reported today (must preserve)

Each node's outcome lands in `NodeEntry.build: BuildState`
(`graph.rs:56`, `BuildState` at `graph.rs:28`):

- Cycle members → `BuildState::Error(BuildError::Cycle)` up front
  (`graph.rs:213`).
- Any parent not `Built` → `BuildState::Error(BuildError::ParentFailed)`,
  `computed_id = None`, and the node is skipped (`graph.rs:246`).
- Build success → `BuildState::Built(arc)`, `computed_id = Some(new_id)`, and
  `registry.insert(arc)` (`graph.rs:284`, `:293`).
- Build failure → `BuildState::Error(e)` carrying that node's own `BuildError`
  (`graph.rs:296`).

These states are read back by the inspector (`inspector_rows.rs:273` renders
"built"/"pending"/error text) and drive the alive set
(`nodes.values().filter_map(|n| n.build.id())`, `graph.rs:303`). **The batched
design must set the exact same per-`FlowId` `BuildState`.**

### Primitives already in the tree (no new deps)

- `crossbeam-channel` — already used for both worker channels (`worker.rs:25`).
- `stellarator::sync` re-exports `maitake::sync` (`libs/stellarator/src/lib.rs:39`),
  which provides **`WaitCell`** (`maitake/sync/src/wait_cell.rs`) with
  `wake()`, `wait()`, and `wait_for(predicate)` (subscribe-then-recheck built
  in, so no lost-wakeup race).
- `thingbuf` is a dep but its async mpsc requires `T: Default + Recycle`; a
  boxed `FnOnce` closure isn't, so it is the wrong fit for closure messages.
  Keep `crossbeam` for payloads and add a `WaitCell` as the doorbell.
- `SharedString` (= `FlowId`) wraps `ArcCow<'static, str>` → `Send`, so it can
  key a result map that crosses the channel. `BuildError` and
  `Arc<dyn DynamicNode>` (`DynamicNode: Send + Sync`) are already `Send` —
  `BuildError` crosses the reply channel today.

## Proposed design

Two orthogonal changes. Do them as separate steps; either is useful alone.

### 1. Executor-friendly doorbell (kills idle polling + per-trip latency)

Keep both `crossbeam` channels for payloads. Add a shared
`Arc<maitake::sync::WaitCell>` "doorbell":

- Every `send` on `WorkerHandle` (`run_batch`, `dispose`) is followed by
  `doorbell.wake()`.
- `run_worker` drains both channels, then parks on
  `doorbell.wait_for(|| !rx.is_empty() || !dispose_rx.is_empty()).await`.
  `wait_for` registers the waker *before* re-checking the predicate, so a send
  racing the park is not lost. Awaiting yields to the stellarator executor, so
  producer tasks keep ticking while the worker is parked.

Result: worker sleeps at 0 Hz when idle; wakes within scheduler latency
(microseconds) of a real send instead of up to 2 ms.

#### Why not a synchronous blocking select

A `crossbeam_channel::Select` / `recv()` blocks the OS thread. That thread *is*
the stellarator executor; blocking it stops polling of the spawned producer
tasks, so clocks/generators/`persist` freeze. The existing 2 ms `sleep().await`
is a deliberate *async* yield for exactly this reason (see `worker.rs:143`
comment). The replacement must stay async — hence `WaitCell::wait_for`, not a
thread-blocking select.

### 2. One batched round-trip (kills N sequential stalls)

Move the topo-ordered build *execution* to a single message. `graph.rs` still
owns all planning, registry access, and `BuildState` writes on the main thread;
the worker only executes construction closures, threading freshly-built parent
`Arc`s between them.

New worker surface (`worker.rs`):

```rust
/// One node's construction, ready to run on the worker.
pub struct BatchNode {
    pub flow_id: FlowId,          // result key (SharedString: Send)
    pub expected_id: NodeId,      // for the id assert + batch-local dedup
    pub parents: SmallVec<[ParentSource; 4]>,
    pub build: BuildClosure,      // FnOnce(Vec<Arc<dyn DynamicNode>>) -> Result<Arc, BuildError> + Send
}

pub enum ParentSource {
    Ready(Arc<dyn DynamicNode>),  // resolved on main thread (registry hit / unchanged built)
    InBatch(FlowId),              // built earlier in this same batch
}

// pub(crate): shared by both the worker thread and the tests' None path.
pub(crate) fn execute_batch(
    nodes: Vec<BatchNode>,
) -> HashMap<FlowId, Result<Arc<dyn DynamicNode>, BuildError>>;
```

`WorkerHandle::run_batch(&self, nodes: Vec<BatchNode>) -> HashMap<FlowId, Result<Arc<dyn DynamicNode>, BuildError>>`
sends one job carrying the whole `Vec`, then blocks once on the reply. On a dead
worker it returns every `flow_id → Err(BuildError::ParentFailed)` (capture the
keys before moving `nodes`), matching today's graceful `ParentFailed`
degradation.

`execute_batch` runs the nodes in order, keeping two maps:

```rust
let mut out: HashMap<FlowId, Result<Arc, BuildError>> = ...;
let mut by_id: HashMap<NodeId, Arc<dyn DynamicNode>> = ...; // batch-local dedup
for node in nodes {
    // resolve parents: Ready(arc) directly; InBatch(fid) from out[fid] (Ok → clone, else fail)
    let arcs = ...;                         // any missing/failed parent → Err(ParentFailed)
    let res = if failed_parent { Err(ParentFailed) }
        else if let Some(a) = by_id.get(&node.expected_id) { Ok(a.clone()) } // dedup within batch
        else { match (node.build)(arcs) {
            Ok(a) => { debug_assert_eq!(a.id(), node.expected_id, "..."); by_id.insert(node.expected_id, a.clone()); Ok(a) }
            Err(e) => Err(e),
        }};
    out.insert(node.flow_id, res);
}
```

`by_id` preserves the current behavior where two nodes with an identical
`(spec, parents)` hash build only once (today's serial `registry.get` after the
first `insert` catches the second; the batch does registry inserts only *after*
it returns, so it needs its own dedup). The `debug_assert_eq!` on the built id
(today at `graph.rs:279`) moves here.

### `rebuild_into` restructured into plan → execute → apply

Keep the signature `rebuild_into(&mut self, db, registry, worker: Option<&WorkerHandle>)`
so the seven test call sites and `rebuild` are untouched. Internally, three
passes:

**Plan (main thread; pure + `registry.get` reads).** Topo order; mark cycle
members `Error(Cycle)` as today. Walk `order`; for each node resolve parent
dispositions (already classified because we go in topo order) and compute
`new_id = compute_node_id(spec, parent_ids)`:

- a parent is `Error`/absent → set this node `Error(ParentFailed)`,
  `computed_id = None`; do **not** queue it.
- idempotent skip (`computed_id == new_id` and built arc matches) → leave built.
- `registry.get(new_id)` hit → set `Built(arc)`, `computed_id = Some(new_id)`.
- otherwise → push a `BatchNode` (`build = move |arcs| build_spec(&spec, arcs, db)`),
  and remember `(flow_id, new_id)`. Each parent becomes `ParentSource::Ready`
  if it already resolved to an `Arc` this pass, or `ParentSource::InBatch(pid)`
  if it is itself queued.

**Execute (one round-trip).**
`let results = match worker { Some(w) => w.run_batch(batch), None => execute_batch(batch) };`
The `None` path runs `execute_batch` inline on the caller's thread — tests
install their own runtime via `#[stellarator::test]`, so `build_spec`'s spawns
land correctly, exactly as the current inline `build_spec` does.

**Apply (main thread).** For each remembered `(flow_id, new_id)`, read
`results[&flow_id]`: `Ok(arc)` → `registry.insert(arc.clone())`,
`build = Built(arc)`, `computed_id = Some(new_id)`; `Err(e)` → `build = Error(e)`,
`computed_id = None`. Then the alive set is unchanged:
`self.nodes.values().filter_map(|n| n.build.id()).collect()`.

This unifies the `Some`/`None` paths onto one code path (planning + `execute_batch`)
instead of the current `match worker` fork inside the loop — less duplication,
and the live and test paths build identically.

### Public surface

`DynamicWorker`, `WorkerHandle`, `dispose` stay. `WorkerHandle::run` (single
node) and the old single-node `BuildClosure` shape are **replaced** by
`run_batch` + the new `BuildClosure` (now takes `Vec<Arc>`); `run` has exactly
one caller (`graph.rs:271`) and leaving it unused would violate STYLE.md's
no-dead-code rule. `BatchNode`/`ParentSource` are exported from
`node_editor/mod.rs` alongside the existing `pub use worker::{DynamicWorker, WorkerHandle}`.

## Step-by-step migration

Each step compiles and keeps `cargo test -p metor-panel` green.

**Step 1 — doorbell (worker.rs only).** Add `doorbell: Arc<WaitCell>` to
`WorkerHandle` and thread a clone into `run_worker`. After each `tx`/`dispose_tx`
send, call `doorbell.wake()`. Replace the `sleep(2ms)` tail of `run_worker` with
`doorbell.wait_for(|| !rx.is_empty() || !dispose_rx.is_empty()).await`; break the
loop on `Err(Closed)`. Leaves the single-node `run` API intact, so `graph.rs`
and all tests are untouched. Removes idle polling and cuts per-trip latency
immediately. *Touches:* `src/node_editor/worker.rs`.

**Step 2 — batching (worker.rs + graph.rs together).** In `worker.rs`: add
`ParentSource`, `BatchNode`, the new `BuildClosure` signature, `execute_batch`,
and `WorkerHandle::run_batch`; delete the single-node `run` and adjust
`BuildJob` to carry the `Vec<BatchNode>` + a `HashMap` reply sender. In
`graph.rs`: rewrite `rebuild_into` into the plan → execute → apply passes above.
Export the new types from `mod.rs`. Doing both files in one step avoids
transient dead code. *Touches:* `src/node_editor/worker.rs`,
`src/node_editor/graph.rs`, `src/node_editor/mod.rs`.

**Step 3 — docs + tests.** Rewrite the stale `worker.rs` module doc (the "we
recv-block on the reply" / "2 ms poll is invisible" paragraphs) and the
`rebuild_into` doc to describe batching and the doorbell, in the house style
(design intent, no play-by-play). Add coverage per "How to test". *Touches:*
`src/node_editor/worker.rs`, `src/node_editor/graph.rs`,
`src/node_editor/tests.rs`.

## Risks and how to test

- **Freezing producers with a sync wait (highest risk).** If the doorbell wait
  ever blocks the thread instead of yielding, every clock/generator stops.
  `WaitCell::wait_for(...).await` yields correctly; a `crossbeam` `recv`/`Select`
  would not. *Test:* run the panel, add a `FixedRate` clock + a downstream plot,
  leave it idle, and confirm the plot keeps advancing (producers ticking) while
  the worker is parked. A `#[stellarator::test]` can also spawn a `fixed_rate`
  node via the worker and assert samples keep arriving after the build returns.
- **Lost wakeup (send races the park).** Mitigated by `wait_for`'s
  register-before-recheck. *Test:* a stress test that fires many `run_batch`
  calls back-to-back and asserts each returns (no hang).
- **Per-node error parity.** The inspector reads `BuildState`. *Tests:* extend
  `node_editor/tests.rs` — a `ParentFailed` chain (a bad parent marks all
  descendants `Error(ParentFailed)`), a `Cycle`, and a per-node `Error(e)` for a
  node whose own `build_spec` fails, asserting the exact `BuildState` per
  `FlowId`. The existing `reconcile_adds_and_removes_built_nodes` and
  `dedup_across_two_graphs_share_node` (`tests.rs:497`, `:519`) must still pass
  unchanged — they exercise the `None` path and cross-call registry dedup.
- **Within-batch dedup regression.** Two nodes hashing to the same `NodeId` in
  one rebuild. *Test:* a graph with two identical-spec, identical-parent nodes;
  assert `registry.len()` counts the id once and both `FlowId`s report `Built`.
  Guarded by `execute_batch`'s `by_id` map.
- **`SharedString`/`BuildError` crossing the channel.** Compile-time `Send`
  check; both are already `Send`. Low risk.
- **Batch build error isolation.** One node's `Err` must not abort the batch —
  `execute_batch` records it in `out` and continues so independent siblings
  still build. Covered by the per-node error parity tests.

Manual: `cargo build -p metor-panel && cargo test -p metor-panel node_editor::`
then run the panel, open a saved multi-node layout, and confirm no hitch on load
and idle CPU at rest (worker no longer waking every 2 ms).

## Estimated scope

Small–medium. ~3 focused steps, ~3 files of production code
(`worker.rs`, `graph.rs`, `mod.rs`) plus tests. `worker.rs` (~150 lines) is
substantially rewritten; `rebuild_into` (~100 lines) is restructured but reuses
all existing helpers (`topo_order`, `parents_of`, `compute_node_id`,
`build_spec`, `registry.get/insert`). No new dependencies, no public API removed
beyond swapping single-node `run` for `run_batch`. The two changes are
independent — Step 1 can land and ship on its own.

## Open questions

1. **Graceful worker shutdown.** Today the thread is never joined (OS reaps at
   exit). With the doorbell, if `DynamicWorker` is dropped and senders close,
   `wait_for` returns `Err(Closed)` only if we `close()` the cell. Worth a clean
   shutdown, or keep parity with today and ignore it?
2. **`SmallVec` inline size for `parents`.** `[ParentSource; 4]` matches typical
   composer arity; confirm no common op exceeds 4 parents often enough to matter
   (variadic composers can).
3. **Should `run_batch` stay strictly blocking?** It preserves the current
   synchronous `rebuild` contract (caller is inside `graph.update`). An async
   variant would need `rebuild` to become async and restructure `schedule_rebuild`
   (`pane.rs:239`) — out of scope here, but flagging it as the natural next step
   if the single remaining blocking round-trip ever shows up in a trace.
