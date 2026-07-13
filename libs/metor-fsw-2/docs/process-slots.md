# Design — process-mode slots

> **Status: implemented** (2026-07-08). Built as planned in
> `docs/process-slots-plan.md` W1–W7, with the §10 open questions resolved to
> this document's recommendations: runtime `Stop` is kill, `Loading` is a
> `SlotState` variant with wire code 5, and there is no auto-restart. Two
> deltas from this text are worth calling out: the shipped runtime command
> set has no `Unload` verb (§3's table row is aspirational — a slot returns
> to `Empty` only at mission teardown, and `Stop`/`Reset`/`Load`-over-terminal
> cover the runtime teardowns), and `add_slot` takes no mode flag — it
> derives a slot's process mode from backing uniformity over the allowed set
> (`OccupantBacking::Artifact` throughout; a mixed set is rejected, per §8's
> per-slot-means-all-occupants rule).
>
> **Pack update (2026-07-11, `docs/packs.md`):** the single-system dl ABI this document's
> spellings assume became the pack ABI v5, and the sequence stack was unified away. Read
> `fsw_create`/`fsw_bind_init`/`fsw_execute`/`fsw_destroy` below as
> `fsw_pack_create(index, mount)`/`fsw_pack_bind_init`/`fsw_pack_execute`/`fsw_pack_destroy`,
> `DlSystem::open` as `DlPack::open` + entry selection, and `run_seq_execute`/
> `run_seq_shutdown` as the occupant-mount `Driver` path (`docs/packs.md` §9) —
> `DlSlot::step_seq` and the worker's `RunMode::Sequence` fold survive as described, with
> `WorkerManifest::Run` additionally carrying the pack entry name. Occupants are no longer
> sequence-only: any pack entry can occupy a process slot, and each occupant worker runs
> `pack()` in its own address space (no shared pack state across the boundary). The
> lifecycle, spawn-per-Load model, and reclamation below are otherwise as shipped.

Runtime slots (`docs/sequences-slots.md`) load their occupants with `DlSystem::open` and run
them in the coordinator's address space; process systems (`docs/process-systems.md`) run a
fixed artifact in a worker process. This design combines the two: a slot declared
`process=#true` runs its occupants **out of process**, with the same isolation rule as process
systems — **the host never dlopens a process slot's occupant artifacts**, at resolve or at any
`Load`.

The one-sentence shape: **a process slot is a `SlotRunner` whose `Load` spawns a worker instead
of calling `fsw_create` — one worker per occupant Load, driven through the existing ctl
lifecycle, and torn down by kill + `reclaim_owner`, the process twin of the hard-drop.**

## 1. What already carries over

Almost everything. The slot layer and the process layer were built against the same seams, and
this design is mostly the observation that they compose:

- **The runner's command machinery is occupant-agnostic.** `SlotRunner`
  (`src/coordinator/slot.rs`) drains its `commands` fan-in, filters by instance name, publishes
  `SlotStatus`, emits `SequenceChannelEvent`s, drains the occupant's `SequenceStatus` through
  the declared `SelfTap`, and writes the `Abort` cancel frame through its host-side
  `Output<SlotControlIn>`. None of that touches the occupant's address space; all of it works
  unchanged when the occupant is behind a process boundary.
- **The ctl lifecycle *is* the occupant lifecycle.** `CtlHost`/`CtlWorker` (`src/proc/ctl.rs`)
  already spell exactly the transitions a `Load` needs: spawn→`Attached` is `fsw_create`,
  `InitReq`→`Ready` is `fsw_bind_init` (which claims the ring roles and builds the future),
  the doorbell is `fsw_execute`, and worker exit is `fsw_destroy`. No new ctl verbs.
- **The status word already carries the terminal.** `FswStatus::Done` crosses the ctl block
  today (`CtlWorker::done`/`StepOutcome::Acked`); `ProcSlot` merely folds it to keep-running
  because a build-time system has no outcome to refine it with (`src/proc/host.rs`), exactly as
  `DlSlot::step` does. The slot runner has the refinement — its `SequenceStatus` self-tap — so
  the seq path is a different *fold*, not a different protocol.
- **The data path is done.** `shared_outputs`/`alloc_rings` (`src/coordinator/mod.rs`) already
  allocate crossing rings as mmap files with recorded paths; `bind_slot` already gathers the
  occupant's rings by the prefix/tail split; rings are backing-erased, so the runner's
  host-side taps and writers cannot tell mmap from heap.
- **Reclamation and the non-blocking pipeline exist.** `RingBuffer::reclaim_owner`,
  `ProcSlot::kill_reap_reclaim`, and the polled Backoff/Attaching/Initing phase machine are the
  primitives a spawn-per-Load lifecycle needs — the process-systems plan explicitly reserved
  this application ("the occupant Load/Unload lifecycle would map onto spawn/kill").
- **Describe workers keep the host clean.** `describe_via_worker` (`src/proc/host.rs`) is the
  resolve-time descriptor source that never dlopens; `resolve_slot`'s contract validation
  (`compatible()` over descriptors, `src/wiring/mod.rs`) is descriptor-driven and needs no
  change beyond where the descriptors come from.

The new machinery is: a **seq mode** in the worker's run loop, a **`SeqWorker`** host-side
handle (the per-Load twin of `ProcSlot`, sharing its spawn/poll/kill pieces), a **`Loading`**
slot phase, and the alloc/wiring plumbing to mark a slot's crossing rings and describe its
allowed set.

## 2. Worker granularity: one worker per occupant Load

**Decision: spawn a worker on `Load`, end it on `Stop`/`Unload`/`Reset`/`Load`-over-terminal.**
Not a persistent per-slot worker that dlopens occupants internally.

- *Isolation cleanliness.* A fresh address space per occupant means nothing an occupant leaked,
  latched in a `static`, or corrupted survives into the next Load. A persistent worker would
  need `dlclose` between occupants, and `dlclose` is exactly the unreliable primitive the
  allowed-set design avoided host-side (TLS destructors, unload-unsafe crates, leaked
  registrations); pushing it into a worker relocates the hazard without removing it.
- *Lifecycle fit.* The ctl states map one-to-one onto occupant transitions (§1). A persistent
  worker needs new verbs (`LoadReq(name)`, `UnloadReq`), a second state machine layered over
  the existing one, and a protocol version bump. Worker-per-Load reuses the block verbatim.
- *Reclaim per cycle for free.* Each Load/Unload cycle ends in a worker exit; clean exit
  releases the ring roles through the occupant's own `Drop`s (§6), and `reclaim_owner(pid)`
  after the reap covers every unclean exit. A persistent worker's per-occupant claims cannot be
  reclaimed by pid — its pid never changes.
- *Cost: Load latency.* Every Load pays spawn + dlopen + create. That is real but operator-paced
  (Loads arrive at command cadence, not cycle cadence), and §4 keeps it entirely off the cycle
  loop. A warm persistent worker is a legitimate future optimization if a mission measures the
  latency and cares; it is not the v1 shape.

Host-side, `SlotRunner` is extended in place rather than twinned: `SlotReg` gains a
`process: bool`, and the runner's live occupant becomes a two-variant seam —
`Occupant::Dl(DlSlot)` as today, or `Occupant::Proc(SeqWorker)` where `SeqWorker`
(`src/proc/host.rs`) wraps the `CtlHost`, the `Child`, the reclaim ring set, and the load
pipeline, factored from `ProcSlot`'s existing spawn/`poll_worker`/`kill_reap_reclaim` pieces.
Everything above the occupant seam (commands, events, status, progress drain, terminal fold)
stays one body of code.

## 3. The seq lifecycle across the ctl protocol

The worker grows a **mode flag on `WorkerManifest::Run`** (`mode: Cyclic | Sequence`) rather
than a third manifest variant — attach, ring mapping, `DlSystem::open`, `make_slot`, and the
lifecycle reports are identical; only the step loop differs. In sequence mode the loop calls a
seq-aware step (a small `DlSlot::step_seq` beside `execute_raw` in `src/dl.rs`) instead of
`DlSlot::step`:

- `FswStatus::Running` and `FswStatus::Panicked` behave as today (panic destroys the foreign
  state, freeing its ring roles, and the worker keeps serving `Panicked` acks until shutdown).
- `FswStatus::Done` is **latched**: the ack carries `Done`, the foreign state is *not*
  destroyed (matching in-process, where a `Done` occupant holds its ports until
  `Reset`/`Load`/`Unload`), and any further doorbell re-serves the latched `Done` without
  polling — a `Ready` future must never be polled again, and the latch makes that a worker
  invariant instead of trusting the host to stop ringing.

The outcome pipeline needs nothing new: `run_seq_execute` (`src/abi/mod.rs`) publishes the
terminal `SequenceStatus` record *before* returning `Done`, the ack's Release/Acquire pair
publishes both, and the runner's existing `drain_progress` over the (now mmap-backed) self-tap
latches `run_state` for `emit_terminal_done` — the same code path as in-process, fed by
`StepOutcome::Acked(FswStatus::Done)` instead of `execute_raw`.

The command mapping, against the ctl lifecycle:

| Command | In-process today | Process mode |
|---|---|---|
| **Load** | `fsw_create` + `bind_init` | end old worker if any; spawn pipeline (§4) → `Attached` → `InitReq` → `Ready` |
| **Start** | begin polling | begin ringing the doorbell each cycle (host scheduling only) |
| **Stop** | hard-drop the future | **SIGKILL + reap + `reclaim_owner`** — see below |
| **Abort** | write the cancel frame | identical: the runner's `Output<SlotControlIn>` writer stays host-side; the frame crosses on the mmap control ring |
| **Reset** | destroy + create + bind | end worker; spawn pipeline with the same occupant |
| **Unload** | destroy | end worker → `Empty` |

**Stop is kill, not graceful shutdown.** The in-process `Stop` is a hard drop with no async
cleanup (`sequences-slots.md` §2.1), and a sequence has nothing graceful to lose —
`run_seq_shutdown` is a documented no-op and the future's `Drop` is the only teardown, which a
kill skips only at the cost of ring roles `reclaim_owner` frees anyway. Kill is immediate and
non-blocking; a graceful `ShutdownReq` would need a polled reaping phase plus a kill fallback
for a hung occupant, buying nothing (flagged in §10). Mission shutdown
(`SlotRunner::shutdown`) stays graceful — `ShutdownReq`, the `SHUTDOWN_GRACE` wait, then kill —
matching `ProcSlot::shutdown`, since blocking is acceptable there.

## 4. Load latency: the non-blocking pipeline and the `Loading` phase

Spawn + dlopen + create must not stall the cycle loop, and `Load` is applied at the head of the
slot's step. The pipeline is `ProcSlot`'s restart machine minus the backoff: on `Load`, the
runner kills/reaps/reclaims any previous worker synchronously (cheap — kill and an atomic sweep
over the ring set), recreates the ctl file (`CtlHost::create` truncates, resetting the
lifecycle and sequence words exactly as the restart path does), spawns, and then polls **one
phase per cycle** — `Attaching { deadline }` (wait `Attached`, i.e. dlopen + `fsw_create`
completed in the worker) then `Initing { deadline }` (request `InitReq`, wait `Ready`, i.e.
`fsw_bind_init` claimed the rings and built the future) — reusing `poll_worker`'s
failed/exited/deadline folding.

On the state machine and the wire: `SlotState` gains a **`Loading`** variant (runtime slots
only, like `Empty`/`Loaded`) with wire phase code **5** in `SlotStatus::phase` — existing codes
0–4 are untouched. This buys the command guards for free: `do_load` accepts only
`Empty | Done | Stopped`, `do_start` only `Loaded`, so a command arriving mid-pipeline is
ignored by the same match arms that exist today, with no new special cases. The spawn emits a
`SequenceEventKind::Loading { name }` on the sequences channel — the load window's begin, for
consumers that fold events rather than the phase byte; in-process loads bind synchronously and
skip it. The pipeline ends
by setting `Loaded` and emitting the `Loaded` event (so observers see the event when the
occupant is actually bound, exactly as in-process); a pipeline failure (spawn error, `Failed`
report, deadline, early exit) kills/reclaims and lands `Stopped { ProcessDied }` plus a
`Failed` event naming the stage — the operator's existing `Reset`/`Load` recovery applies.

The **initial occupant** (`SlotRunner::init`) loads **blocking**, inside the coordinator's init
barrier — the mirror of the build-time `ProcSlot::spawn`, which blocks deliberately because
init is not cycle time. `initial state="running"` then just sets `Running` before the first
step.

## 5. Which rings cross

Verified against `bind_slot`'s gathering (`src/coordinator/mod.rs`): the occupant prefix
crosses, the runner tail does not.

- **Occupant outputs** — the slot descriptor's `occupant_outputs` (`src/coordinator/slot.rs`):
  the user outputs, then `SequenceStatus`, health, log. All become mmap files; the runner's
  `SequenceStatus` self-tap and the registry/telemetry taps use the coordinator's own mapping.
- **Producers of the occupant's Edge inputs** — the `cons_edges` producer ring behind each
  `occupant_inputs` port with `PortConn::Edge`. `shared_outputs` extends from "inputs of
  `Reg::Proc` systems" to also cover the occupant-input range for process `Reg::Slot`s (it
  excludes the `commands` fan-in and the self-tap by construction — those sit in the framework
  tail, after the occupant inputs).
- **The Host `SlotControlIn` ring** — today allocated heap-only in the dedicated
  `host_input_rings` pass. For a process slot it becomes an mmap file
  (`<slot>.<port>.ring`, path recorded like an output's): the *writer stays host-side* (the
  runner's cancel `Output`), the occupant's read `View` attaches in the worker. This is the one
  genuinely new alloc case — a Host-connected input crossing outward — and it is the same
  file-or-heap fork `alloc_rings` already runs for outputs.
- **Host-side, uncrossed**: the `commands` fan-in producer rings (the runner drains them), the
  `SlotStatus` output, and the events channel — the runner tail never leaves the coordinator.

The session directory condition extends from "any `Reg::Proc`" to "any `Reg::Proc` or process
`Reg::Slot`". Because the allowed set shares one port contract and the rings are the *slot's*,
every occupant sees the same ring files — so the bind arm writes **one manifest per allowed
occupant** (`<slot>.<occupant>.manifest`, differing only in artifact path and params) at
`build()`, and a `Load` only picks a manifest and spawns. The ctl file is `<slot>.ctl`,
recreated per spawn.

## 6. Ring attach/release per Load cycle

The reader budget is unchanged. Today each Load claims the occupant's reader slots and writer
roles and each Stop/Unload releases them through the occupant's `Drop`s; the budget (edge
fan-out per producer, `1 + slack` on the control ring) is sized for one occupant at a time.
Worker-per-Load preserves the invariant because teardown is ordered before the next bind: the
runner kills/reaps/reclaims the old worker *before* spawning, and the new worker claims nothing
until `InitReq` (`fsw_bind_init` is where `attach_raw` claims roles — `fsw_create` touches no
rings).

Does a **clean** worker exit release the roles via `Drop`? Yes: the worker's shutdown path
(`run_system` in `src/proc/worker.rs`) runs `slot.shutdown()` then drops the `DlSlot`, whose
`fsw_destroy` drops the occupant's ports inside the `.so` — and a `View::drop`/`Writer::drop`
is a plain atomic store into the shared region (`FREE_SLOT` / `0`), process-agnostic by
construction. Reclaim-after-clean-exit is therefore not *needed*, but the runner runs
`reclaim_owner` after every reap anyway (it is a no-op over released slots), which collapses
clean, killed, and crashed exits into one teardown path — the same belt-and-braces
`kill_reap_reclaim` already takes.

## 7. Death and restart: no auto-restart for occupants

**Decision: a dead occupant worker does not auto-restart.** Death while `Running` (ctl timeout
+ `try_wait` reaped, or an `Acked(Panicked)`) folds to `Stopped { ProcessDied }` /
`Stopped { Panicked }` — the slot's existing terminal states — with a `Failed` event, and the
operator recovers with `Reset` or `Load`, the recovery path slots already have.

The rationale is the shape of the occupant, not the cost of the machinery (which exists): a
cyclic system is steady-state, so re-attaching it resumes the same computation and restart is
obviously right; a sequence is a **one-shot activity with external side effects** — restart
means re-running it *from the beginning*, silently re-issuing every command it already sent.
That is a mission-level decision, not a supervisor default. The restart budget machinery stays
available to a future opt-in (`restart=#true` on the slot node) without redesign; flagged in
§10.

Step timeouts with a live worker are handled exactly as `ProcSlot` does: count, drain through
`CyclicSlot::drain_timeouts` into coordinator health, keep going — lateness is self-healing
under the sequence protocol.

## 8. Wiring surface

```kdl
slot "adcs" process=#true {
    input frame="sensors"
    output frame="mode"
    allow occupant="commissioning"
    allow occupant="safe_mode"
    initial occupant="commissioning" state="loaded"
}
```

`SlotSpec` gains `process: bool` (`#[serde(default)]`; omitted documents deserialize
unchanged), parsed beside the slot node's children in `parse_slot` and mirrored by a
`SlotSpecBuilder::process()` toggle. Per-slot means all-occupants: the isolation boundary is
the slot's position in the cycle, and a mixed allow set would make `Load` silently change the
fault domain.

`resolve_slot` forks per occupant exactly where `resolve` forks per system: the process path
calls `describe_via_worker` for **each allowed occupant's** built artifact instead of
`open_occupant`'s `DlSystem::open`, decodes descriptor + params schema through
`decode_descriptor_msg`, and encodes the occupant's params value tree through the same
schema-guided `encode_value_params` — the `resolve_proc` recipe, once per allowed occupant, each a short-lived
bounded child with stderr folded into the diagnostic. `AllowedOccupant` therefore grows a
backing seam: `{ name, params, descriptor: SystemDescriptor, backing: Dl(DlSystem) |
Artifact(PathBuf) }` — the contract checks in `resolve_slot` and `add_slot` (`ports_match`
over `compatible()`) already operate on descriptors and change only their accessor. On
unsupported targets a process slot is `LoadError::ProcessUnsupported`, as for process systems.

## 9. Telemetry

A process slot's worker joins the coordinator status frame's worker list through the same
`CyclicSlot::proc_info` hook `ProcSlot` uses — `SlotRunner` returns `Some` when process-mode:
the worker **pid** (`0` when `Empty` or between workers), a count of **unplanned worker
deaths** in the `restarts` field (Loads are commanded, deaths are the anomaly the counter
should surface), and the run state mapped `Loading → Restarting` ("half-born, inside the
pipeline" already describes it), live worker → `Running`, none → `Stopped`. The occupant-level
story (which occupant, what phase, the outcome) stays on `SlotStatus` and the events channel,
where it already lives — the worker list answers "is there a process behind this slot and is
it alive", nothing more.

## 10. Open questions

- **Stop-by-kill vs graceful-with-fallback.** §3 recommends kill for runtime teardown
  (hard-drop parity, zero new phases, `run_seq_shutdown` is a no-op). The counterargument: a
  future occupant kind with a real shutdown hook, or `.so`-internal state that dislikes SIGKILL
  (files mid-write). Genuinely contested; kill is reversible later since the ctl path exists.
- **`Loading` as a `SlotState` variant + wire code 5.** The alternative is keeping the previous
  phase on the wire and signaling only through events. The variant is recommended (command
  guards fall out; operators see the pipeline), but it touches the `SlotStatus` wire contract
  and the panel must learn the code. *Resolved (the plan's R3):* the variant shipped, and the
  panel learned the window through events, not the phase byte — the spawn emits a new
  `SequenceEventKind::Loading { name }` that the panel's sequence UI folds (the occupant name
  with a loading status line, resolved by the bind's `Loaded`).
- **Opt-in auto-restart of a `Running` occupant.** §7 says operator-only; a mission whose
  sequences are idempotent may want `restart=#true` with the existing budget knobs. Deferred
  until asked for.
- **A persistent pre-warmed worker per slot** (spawn at build, dlopen on Load) as a Load-latency
  optimization, if measured latency ever matters. Explicitly future work; it reopens the
  `dlclose` and ctl-verb questions §2 closed.
