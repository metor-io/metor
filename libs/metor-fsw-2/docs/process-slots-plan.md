# Implementation plan — process-mode slots

> **Status: proposed**

Design: `docs/process-slots.md`. Summary of what we build: a **sequence mode** in the worker's
run loop (latch `FswStatus::Done`, never re-poll a `Ready` future), a host-side **`SeqWorker`**
(the per-Load twin of `ProcSlot`, factored from its spawn/poll/kill pieces), an
**occupant-backing seam** in `SlotRunner` (`Occupant::Dl(DlSlot)` vs `Occupant::Proc(SeqWorker)`
with a polled `Loading` pipeline and a new `SlotState::Loading`, wire phase 5), the **alloc
extensions** that mark a process slot's crossing rings (occupant output prefix, its Edge-input
producers, and the Host `SlotControlIn` ring as mmap files), and the `process=#true` **wiring
surface** on `slot` nodes (describe workers over the allowed set at resolve; the host never
dlopens a process slot's occupant artifacts). Zero ring change, zero ctl protocol change, zero
dl ABI change (`FSW_ABI_VERSION` stays 4).

Built in **7 waves**. Dependency graph (strict edges →):

```
W1 (alloc: slot ring backing) ──────────────┐
W2 (worker seq mode) ──▶ W3 (SeqWorker) ────┼─▶ W4 (SlotRunner proc backing + telemetry) ─▶ W5 (wiring) ─▶ W6 (e2e) ─▶ W7 (docs)
```

- **W1 and W2 are independent** (alloc pass vs worker loop); either order, one commit each.
- W3 needs W2 only for the manifest mode flag in its spawn spec; the factoring of
  `ProcSlot` internals is otherwise self-contained.
- Build + test after each wave. Gates that stay green throughout: the existing slot
  integration tests (`tests/` slot/sequence suites), `tests/proc_integration.rs`,
  `cargo build -p metor-fsw-2 --no-default-features`, and the non-linux/macos cfg build
  (process slots must degrade to the same clean errors process systems do; no new cfg outside
  `src/proc` and the two resolve/bind forks).

---

## W1 — coordinator: ring backing for process slots

**Files:** `src/coordinator/mod.rs` (`shared_outputs`, `alloc_rings`, `RingAlloc`).

- `SlotReg` gains `process: bool` (default false; set by W4's `add_slot` surface — land the
  field now with a hardcoded `false` so W1 is testable via a builder-internal seam, or land it
  behind `add_slot`'s new parameter directly if W4's signature change is pulled forward; keep
  the wave boundary at "alloc behavior" either way).
- `shared_outputs`: for each `Reg::Slot(slot_reg)` with `process`, insert the occupant output
  prefix `(sid, 0..n_occ_outputs)` and the producer endpoints of every Edge input in
  `0..n_occ_inputs` (from `cons_edges`). Indices `n_occ_inputs..` (the `commands` fan-in and
  the `SequenceStatus` self-tap) are excluded by the range; assert that in a test, not a
  comment.
- Session-dir condition: `Reg::Proc` **or** process `Reg::Slot`.
- Host-input allocation: the dedicated `PortConn::Host` ring pass grows the same file-or-heap
  fork the output pass has — for a process slot, `alloc_ring_at` into
  `<session>/<instance>.<port>.ring` and a path record. `RingAlloc.ring_paths` today keys
  `(sid, out_idx)` for outputs; add `host_input_paths: HashMap<(usize, usize), PathBuf>`
  (keyed like `host_input_rings`) rather than overloading the output map.
- **Tests** (in-crate, no worker): a process slot's backing selection marks exactly the
  occupant prefix outputs + Edge producers + control ring and nothing else (tail rings and
  command producers stay heap); a non-process slot allocates all-heap and no session dir; a
  mixed graph (process system + process slot) shares one session dir.

## W2 — worker: sequence mode

**Files:** `src/proc/worker.rs`, `src/dl.rs`.

- `WorkerManifest::Run` gains `mode: RunMode` (`#[derive(Serialize, Deserialize)]` enum
  `Cyclic | Sequence`). Manifests are same-build ephemera, so the postcard shape change needs
  no migration; keep `Cyclic` first so existing fixtures' bytes stay stable if any test pins
  them.
- `src/dl.rs`: `pub(crate) fn DlSlot::step_seq(&mut self, now: Timestamp) -> FswStatus` beside
  `execute_raw`: early-return the latched status when stopped/null (as `step`), call the
  execute export, fold `Panicked` with the same destroy-and-null policy as `step`, and **latch
  `Done`** in a new field (or fold into `slot_state` as `Done { outcome: 0 }`) so a further
  call re-serves `Done` without polling — the worker-side guarantee that a `Ready` future is
  never polled twice.
- `run_system`: thread the mode to the step loop; `Sequence` uses `step_seq` and passes the
  raw status (`Running`/`Done`/`Panicked`) to `ctl.done`. `Done` does **not** break the loop or
  destroy state (the occupant holds its ring roles until shutdown, matching in-process
  `Done`); `Panicked` keeps today's serve-until-shutdown behavior. Shutdown path unchanged.
- **Tests:** manifest round-trip with both modes; `step_seq` latch behavior against a stub (if
  a no-dlopen seam is impractical, defer the latch test to W6's fixture and unit-test only the
  manifest here).

## W3 — proc host: `SeqWorker`, the per-Load worker handle

**Files:** `src/proc/host.rs`.

- Factor the pieces of `ProcSlot` that the design shares — `spawn_child` (fresh
  `CtlHost::create` over the ctl path + `Command` spawn), `poll_worker` (failed/exited/deadline
  folding toward a wanted state), `kill_reap_reclaim`, `pid()` — into either free helpers or a
  small `WorkerHandle` struct both `ProcSlot` and `SeqWorker` embed. Pure refactor first
  (existing proc tests stay green), then build on it.
- `pub(crate) struct SeqWorker`: the manifest path for the selected occupant, ctl path, exe,
  reclaim ring set, step timeout, and a pipeline phase (`Attaching { deadline }` /
  `Initing { deadline }` / `Ready`). API surface consumed by W4:
  - `SeqWorker::spawn(...)` — kill nothing (caller tears down first), recreate ctl, spawn,
    enter `Attaching`. Non-blocking.
  - `poll_load(&mut self) -> LoadPoll { Pending, Ready, Failed }` — one phase per call,
    reusing the factored `poll_worker`; drives `Attached → InitReq → Ready`.
  - `spawn_blocking(...)` — the init-barrier variant for the initial occupant: spawn then wait
    `Attached`/`Ready` with the existing `SPAWN_TIMEOUT`/`INIT_TIMEOUT`.
  - `step(&mut self, now) -> StepOutcome` — delegate to `ctl.step(now, step_timeout)`.
  - `end(&mut self)` — kill/reap/reclaim, idempotent; also the `Drop` body.
- **Tests:** the `ProcSlot` factoring is covered by the existing proc unit tests; add
  `SeqWorker` pipeline tests driven by a fake `CtlWorker` on a thread (the ctl protocol is
  address-space agnostic): spawn→Attached→Ready happy path over two polls, worker `Failed`
  during attach folds to `Failed`, deadline lapse folds to `Failed`, `end` after each.

## W4 — `SlotRunner`: the proc backing, `Loading`, telemetry

**Files:** `src/coordinator/slot.rs`, `src/coordinator/mod.rs` (`SlotState`, `add_slot`,
`bind_slot`).

- `SlotState::Loading` (runtime slots only): `code() == 5`, `stop_reason() == None`; extend the
  `SlotState` docs' variant map. The `SlotStatus` doc comment's phase legend gains
  `Loading=5`.
- `AllowedOccupant` grows the backing seam from the design §8:
  `{ name, params, descriptor: SystemDescriptor, backing: OccupantBacking }` with
  `OccupantBacking::{ Dl(DlSystem), Artifact(PathBuf) }`. `add_slot`'s contract checks read
  `descriptor` (one accessor change); the Dl constructor keeps today's call sites working.
- `SlotRunner.slot: Option<DlSlot>` becomes `Option<Occupant>` with
  `Occupant::{ Dl(DlSlot), Proc(SeqWorker) }`; `build_occupant` forks on the backing:
  - Dl: today's `make_slot` + `init` path, state → `Loaded` synchronously.
  - Proc: tear down any live worker (`end`), `SeqWorker::spawn` from the per-occupant
    manifest, state → `Loading`. A `poll_loading` step at the head of `step` (after the
    command drain, before `publish_status`) advances the pipeline: `Ready` → `Loaded` + the
    `Loaded` event; `Failed` → `Stopped { ProcessDied }` + a `Failed` event naming the stage.
  - `init` with an initial occupant and a proc backing uses `spawn_blocking`.
- Step-the-occupant fork: `Running` + `Occupant::Proc` rings the doorbell via
  `SeqWorker::step`; `Acked(status)` feeds the **existing** fold (`drain_progress`, `Done` →
  `emit_terminal_done` + `Done { outcome }`, `Panicked` → `Stopped`, plus `end()` on
  `Panicked`); `TimedOut` forks on `try_wait`: dead → `end()` + `Stopped { ProcessDied }` +
  `Failed` event (no auto-restart, design §7), alive → `timeouts += 1`.
- `do_stop`/`do_reset`/`do_load`-over-terminal on a proc occupant call `end()` where the dl
  path drops the `DlSlot`; `shutdown` on a live proc occupant goes graceful
  (`ShutdownReq` + grace + kill), matching `ProcSlot::shutdown`.
- `bind_slot` proc arm: gather the occupant ring **paths** (Edge producers + control ring from
  W1's records, output prefix from `ring_paths`) in descriptor order, write one
  `<slot>.<occupant>.manifest` per allowed occupant (`mode: Sequence`), and hand the runner
  the `SeqWorker` spawn ingredients (ctl path `<slot>.ctl`, exe override, step timeout,
  reclaim ring set = the same handles `bind_proc` collects). No worker is spawned at build.
- Telemetry: `SlotRunner::proc_info` returns `Some(ProcInfo)` when process-mode (pid, death
  count in `restarts`, `Loading → Restarting` mapping per design §9); `drain_timeouts`
  forwards the counter. The coordinator's `update_status` worker scan needs no change — it
  already walks `proc_info` over every cyclic slot.
- **Tests:** state-machine tests with a fake worker thread (mirror W3's harness): Load →
  Loading → Loaded event ordering; Start/Stop guards during Loading (commands ignored by the
  existing match arms); Done fold from an acked `Done` + a staged `SequenceStatus` record;
  worker death while Running lands `Stopped { ProcessDied }` with no respawn; Reset spawns a
  fresh pipeline; `proc_info` pid/state across the cycle.

## W5 — wiring surface

**Files:** `src/wiring/model.rs`, `src/wiring/parse.rs`, `src/wiring/de.rs`,
`src/wiring/builder.rs`, `src/wiring/mod.rs` (`resolve_slot`), `src/wiring/error.rs`.

- `SlotSpec.process: bool` (`#[serde(default)]`); KDL `process=#true` property on the `slot`
  node, parsed in `parse_slot` beside the name; `SlotSpecBuilder::process()` toggle; extend
  the KDL↔builder front-end-equivalence test.
- `resolve_slot`, process path (cfg'd like `resolve_proc`, with the same
  `LoadError::ProcessUnsupported` on other targets): per allowed occupant, `find_built_artifact`
  → `describe_via_worker` → `decode_descriptor_msg` → `encode_kdl_params` against the decoded
  schema (reserved keys/skip_args as in the dl path) → `AllowedOccupant` with
  `OccupantBacking::Artifact`. The existing `ports_match`/`compatible()` cross-check and the
  declared-contract validation run unchanged over the decoded descriptors. Describe failures
  reuse the `ProcDescribe`-shaped diagnostic, naming the slot and occupant.
- **Tests:** KDL parse round-trip of `process=#true` on a slot; serde default (old documents
  unchanged); unsupported-target error path; front-end equivalence.

## W6 — end-to-end integration tests

**Files:** `tests/proc_slot_integration.rs` (new, `harness = false` with `worker_entry()`
first, mirroring `tests/proc_integration.rs`), reusing the sequence fixture the slot e2e tests
already build (add a park-forever occupant param if the existing fixtures complete too fast to
kill mid-run).

- **Lifecycle e2e:** a process slot with two allowed occupants; Load → assert `Loading` then
  `Loaded` phases on `SlotStatus` and the `Loaded` event; Start → Progress events flow (over
  the mmap `SequenceStatus` ring); completion → `Done` phase + `Completed` event; Reset +
  re-run; Unload → `Empty` and the worker is reaped (no child left, roles released — a
  subsequent Load succeeds, proving the reader budget survived the cycle).
- **Occupant swap:** Load A, run to Done, Load B; assert B's worker has a new pid (worker list)
  and B runs clean over the same rings.
- **Death:** SIGKILL the occupant worker mid-run; assert `Stopped { ProcessDied }`, the
  `Failed` event, **no respawn**, upstream producers unblocked (reclaim), and that an operator
  `Reset` brings a fresh worker up.
- **Abort across the boundary:** Abort while Running; assert the occupant's safing branch runs
  (its `Aborted` outcome arrives) — the host-side cancel writer over the mmap control ring.
- **Isolation:** assert the host process never maps the occupant artifact (e.g. the fixture's
  load-time constructor writes a canary file; the canary appears for worker pids only).

## W7 — documentation pass

- `docs/sequences-slots.md`: a status-banner note pointing here for the process mode.
- `docs/process-systems.md` / `-plan.md`: replace the "slots are future work" closers with a
  cross-reference.
- `docs/wiring.md`: `process=#true` on `slot`; `docs/coordinator.md`: the `Loading` phase and
  the worker-list entry for slots; `DESIGN.md` document map entry.

---

## Risks / decisions to watch

- **R1 — polling a completed future.** The whole guard is W2's `Done` latch plus the runner's
  phase check. A host/worker version skew or a runner bug that rings the doorbell after `Done`
  must hit the latch, never the poll; make the latch the first thing `step_seq` checks and
  test it directly.
- **R2 — teardown-before-bind ordering.** The reader budget argument (design §6) rests on
  kill/reap/reclaim completing before the next worker's `InitReq`. The pipeline makes this
  structural (teardown is synchronous in `do_load`, claims happen at `Initing`), but the
  blocking-init path and `shutdown` must keep the same order; assert with the swap e2e.
- **R3 — `SlotStatus` wire code 5.** The panel and any `SequenceChannelSpec` consumers must
  learn the `Loading` phase; land the code and the panel change in the same train, like the
  ring v3 bump.
- **R4 — pipeline latency visible to operators.** dlopen of a large occupant can hold
  `Loading` for many cycles. That is by design (the loop never stalls), but the deadline
  (`SPAWN_TIMEOUT`) must comfortably exceed worst-case artifact load; consider a per-slot
  override only if a real mission asks.
- **R5 — pid-reuse window on reclaim.** Same argument as process systems (reclaim immediately
  after reap keeps the window nil), but slots reclaim far more often (every Load cycle); keep
  `end()` as the single reap+reclaim site so the invariant has one owner.
- **R6 — `ProcSlot` factoring regressions.** W3 refactors a shipped, tested path. Do the pure
  factoring as its own commit with zero behavior change, gated on the existing proc unit and
  e2e suites, before `SeqWorker` lands on top.
