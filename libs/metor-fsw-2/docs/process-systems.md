# Design — cross-process systems

> **Status: implemented** (2026-07-08). Built as planned in
> `docs/process-systems-plan.md` W1–W8; the two deltas from this document are
> noted inline (§5 worker-exe override, §7 builder signature).

Systems currently run in one process, statically linked or dlopen'd. This design adds a third
mode: a system running in its **own OS process**, exchanging frames with the rest of the graph
over the same shared-memory rings. Nothing about the existing modes changes; a mission mixes all
three freely.

The one-sentence shape: **a process system is a dlopen system whose `dlopen` happens in a worker
process, with mmap-backed rings for data and a futex doorbell for stepping.**

## 1. What already carries over

The design leans on properties the tree already has:

- The ring region is offset-addressed with no process-local pointers, validated on attach
  (magic/version/arch tag), and the `mmap` feature (`RingBuffer::create_mmap` /
  `attach_mmap`) already produces cross-process-capable regions. Two processes mapping the same
  file at different base addresses run the identical atomic protocol.
- The writer claim lives in the region, so single-writer is already enforced cross-process.
- The dl ABI (`src/abi`) already reduces a system to serialized bytes plus `(base, len, role)`
  ring handles, binds positionally in descriptor order, and runs every wake endpoint as `NoWake`
  because cyclic systems poll their inputs each step. None of that changes: the worker process
  runs the **same** `DlSystem::open` / `fsw_create` / `fsw_bind_init` / `fsw_execute` lifecycle
  against the same cdylib artifact, just with regions it `attach_mmap`ed itself.
- Wiring, validation, ring sizing, params encoding, the registry, and telemetry are all
  descriptor-driven and never touch the system instance. The descriptor already crosses an
  untrusted boundary as postcard bytes (`SystemDescriptorMsg`), and `into_descriptor()`
  reconstructs it host-side with no static types — so the same bytes can arrive over a process
  boundary instead of the C ABI, and the host validates/wires exactly like a dl system.

So the new machinery is exactly three things: a cross-process wake primitive, a per-worker
control block that steps the system in lockstep with the cycle, and the launch/attach/teardown
plumbing around a worker process.

One isolation rule holds throughout: **the host never dlopens a process system's artifact.**
Everything the host needs from the foreign code — the descriptor and the params schema — arrives
as serialized bytes from a short-lived worker run (§5), so load-time constructors, `describe`,
and every later lifecycle call all execute outside the coordinator's address space. A process
system's fault isolation covers its whole lifecycle, not just the steady state.

## 2. The wake backend — and why not the `atomic-wait` crate

The step protocol needs "wait until this shared `AtomicU32` changes / wake the waiter", i.e. a
futex. The `atomic-wait` crate has exactly the right API, but **it is process-private on every
platform** (verified against `atomic-wait` 1.1.0 sources):

- Linux: `FUTEX_WAIT | FUTEX_PRIVATE_FLAG` — the kernel keys private futexes by
  `(mm, uaddr)`, so a waiter and a waker in different processes never match, even on the same
  `MAP_SHARED` page.
- macOS: libc++'s `__libcpp_atomic_monitor`/`__libcpp_atomic_wait`, whose contention table is a
  per-process static.
- Windows/FreeBSD: `WaitOnAddress` / `UMTX_OP_*_PRIVATE`, both documented process-local.

We therefore keep `atomic-wait`'s API **shape** but implement the shared-memory variants
ourselves, as a small `wake` module in the ring crate (feature `futex`, no new deps beyond
`libc`):

```rust
pub fn wait(a: &AtomicU32, expected: u32);
pub fn wait_timeout(a: &AtomicU32, expected: u32, timeout: Duration) -> WaitOutcome; // Woken | TimedOut
pub fn wake_one(a: &AtomicU32);
pub fn wake_all(a: &AtomicU32);
```

- **Linux**: `SYS_futex` with plain `FUTEX_WAIT`/`FUTEX_WAKE` (no private flag), which is keyed
  by the underlying page and works across processes on a shared mapping.
- **macOS**: `os_sync_wait_on_address{,_with_timeout}` / `os_sync_wake_by_address_{any,all}`
  with the `SHARED` flag — the public futex API since macOS 14.4. We require 14.4+ for process
  systems (the older `__ulock_*` shared ops are a private-API fallback we do not take).
- Other targets: the module is `cfg`'d out and process systems fail to resolve with a clean
  error.

Spurious wakeups are allowed (as in `atomic-wait`); every caller re-checks its predicate in a
loop. The module is excluded from Miri (syscalls); its protocol users are tested with threads,
which shared futexes also support.

The ring's per-ring `wake_word` / `FLAG_WAKE_SHARED` reservation stays reserved. Cyclic-only
process systems need no per-ring wake (all dl wake endpoints are `NoWake`); the doorbell lives in
the control block instead. Per-ring shared wakes become interesting only with cross-process
*async* systems, which are out of scope (§8).

## 3. Data path: which rings become mmap

At `build()`, a ring is allocated with `create_mmap` instead of heap iff it crosses a process
boundary: **every output ring of a process system** (including its implicit health/log), and
**every output ring some process system consumes over an edge**. Everything else stays heap.
In-process consumers of an mmap ring (telemetry views, other systems' inputs) use the
coordinator's own mapping — a ring handle is backing-erased, so nothing downstream can tell.

Ring files live in a per-run session directory — `/dev/shm` when present, else the OS temp dir —
named `metor-fsw-<pid>-<n>/`, one `<instance>.<port>.ring` per shared ring plus one
`<instance>.ctl` control file per worker. The coordinator owns the directory and removes it
best-effort on drop; regions are ephemeral IPC state, never archives.

## 4. The control block and the step protocol

One small mmap file per worker, `#[repr(C)]`, following the ring's header discipline
(magic/version/arch-tag validated on attach; all mutable words atomic):

```text
CtlBlock {
    magic, version, arch_tag,          // immutable, written by the host before spawn
    state:    AtomicU32,  // lifecycle handshake (also its own futex word)
    doorbell: AtomicU32,  // step sequence, host-incremented
    ack:      AtomicU32,  // last step sequence the worker completed
    status:   AtomicU32,  // raw FswStatus of the last execute (untrusted, from_raw'd)
    now:      AtomicU64,  // the step's Timestamp tick, written before doorbell
}
```

Lifecycle over `state` (each transition wakes the word): host spawns the worker at `build()`
with `state = Booting`; the worker maps everything, dlopens, runs `fsw_create`, and reports
`Attached`. `ProcSlot::init` (the coordinator's init barrier) requests `InitReq`; the worker runs
`fsw_bind_init` (binding claims its ring roles and runs `System::init`) and reports `Ready`.
Shutdown is `ShutdownReq` → worker runs `fsw_shutdown`/`fsw_destroy` (releasing its ring roles),
reports `Done`, and exits. Any worker-side failure latches `Failed` plus a status code.

Stepping, in the cycle loop (`ProcSlot::step`, keeping registration-order semantics — downstream
systems in the same cycle see this system's outputs):

- **Host:** store `now` (Release rides the doorbell), increment `doorbell`, `wake_one`; then
  `wait_timeout(ack, old, deadline)` until `ack == doorbell`.
- **Worker loop:** `wait(doorbell, last)`; on wake re-check `state` (shutdown?) and `doorbell`;
  if new, load `now`, run one `fsw_execute` through the existing `DlSlot`, store `status`, store
  `ack = doorbell`, `wake_one(ack)`.

A missed deadline does **not** stop the loop: the coordinator telemeters a
`proc_step_timeout` error on its own health (the worker owns the system's health ring, so the
host cannot write it) and moves on. The sequence protocol makes lateness self-healing — a slow
worker that wakes later sees only the newest doorbell value, runs once for it, and skipped cycles
are simply skipped; latest-wins consumers re-serve their pinned records meanwhile. The deadline
comes from `CoordinatorConfig::proc_step_timeout` (default 100 ms; the wall cycle budget is
usually far tighter, so a healthy worker never sees it).

`FswStatus::Panicked` from the worker maps to `SlotState::Stopped { Panicked }` exactly like a
dl slot; the worker destroys its state on panic (freeing its reader slots and writer claims,
same policy as `DlSlot::step`) and exits.

## 5. The worker process

The worker is not a separate binary: it is **the host executable re-executed** with
`METOR_FSW_WORKER=<manifest>` in its environment. The framework exposes

```rust
metor_fsw_2::proc::worker_entry(); // call first in main(); runs the worker loop and exits if the env var is set
```

The `metor-fsw` CLI calls it, so the shipped runner supports process systems out of the box; an
application embedding the framework in its own binary must call it first thing in `main`, and
this is loudly documented. A missing guard (the child ran the app's main instead) surfaces as a
clean timeout error naming the guard — at resolve for a describe run, at `build()` for a run
worker that never reports `Attached` — and the child is killed either way.
`CoordinatorBuilder::worker_exe` (builder-scoped, so `CoordinatorConfig` stays `Copy`) overrides
the executable for hosts that want a dedicated worker binary; `CoordinatorBuilder::shm_dir`
overrides the session root the same way.

The worker has two modes, selected by the manifest:

- **Describe** (at resolve): the manifest names the artifact and an output file. The worker
  dlopens the artifact, runs `fsw_describe`, writes the postcard `SystemDescriptorMsg` bytes to
  the output file, and exits. The host decodes through the existing
  `SystemDescriptorMsg::into_descriptor()` path — the same untrusted-bytes handling
  `DlSystem::open` applies, just fed from a file instead of a `ByteSink`. This is what keeps the
  host free of foreign code: it never opens the artifact itself. A describe run is bounded by a
  timeout, and its stderr is captured into the resolve diagnostic on failure.
- **Run** (at `build()`): the manifest carries the ABI version, instance name, artifact path,
  canonical params bytes, the control-file path, and the input/output ring file paths **in
  descriptor order** — the same positional contract `bind_dl` uses. The worker maps each ring
  file (`attach_mmap`), turns its regions into `FswRing` handles, and drives an ordinary
  `DlSlot`; the maps outlive the slot so the dl teardown-ordering contract holds unchanged.

The two spawns dlopen the artifact twice, in two different (short-lived, then long-lived)
processes; that is deliberate — resolve can fail for unrelated reasons, and coupling a live
worker's lifetime to the resolve phase buys nothing.

## 6. Liveness and reclamation

A worker can die abruptly (crash, OOM-kill, operator `kill -9`) while holding a writer claim and
reader cursors in shared regions. Today nothing reclaims those, and a dead reader's pinned cursor
would backpressure every upstream producer forever. With real cross-process peers this stops
being hypothetical, so the ring grows the reclamation the layout already reserved room for:

- Each `ReaderSlot` gains an `owner: AtomicU64` (carved from the existing 48-byte pad), stamped
  with the claiming process id in `view()`. The writer claim word stores the claimant's pid
  instead of `1`. Region `VERSION` bumps 2 → 3 (regions are ephemeral; no migration).
- `unsafe fn RingBuffer::reclaim_owner(pid)` frees every reader slot owned by `pid` (bumping the
  slot epoch first, per the reserved reclamation discipline) and releases the writer claim if
  `pid` holds it. The safety contract: the owner process is dead, so none of its stores race.

The host detects death via `Child::try_wait` on every step timeout and at init. On death it
SIGKILLs (belt and braces), reaps, runs `reclaim_owner` over the worker's rings (its own outputs
plus its producers' outputs — the host knows the exact set), and marks the slot
`Stopped { ProcessDied }` (a new `StopReason`). The stop is permanent, matching the in-process
panic policy; restart/quarantine remains future work. The improvement over today: the rest of
the mission keeps flowing at full rate instead of backpressuring into the dead reader.

## 7. Wiring surface

A process system is an artifact-backed `system` node with one new property:

```kdl
system "imu" artifact="imu-driver" process=#true sample_hz=200.0
```

`SystemSpec` gains `process: bool` (serde-default false, so existing documents are unchanged).
`process=#true` without `artifact=` is a resolve error (a static-registry type has no loadable
form the worker can reconstruct; a statically-linked worker mode is future work). Resolve runs a
describe-mode worker (§5) instead of `DlSystem::open`, decodes the descriptor and params schema
from its output, encodes params through the same schema-guided path as `resolve_dl`, and
registers via a new `CoordinatorBuilder::add_proc_cyclic(name, descriptor, artifact_path,
params)` — no `DlSystem` handle exists for a process system, and the `Params` schema stays in
the resolver (its only consumer). A `Reg::Proc`
registration flows through the uniform validate/size/allocate passes untouched and binds to a
`ProcSlot` (host half) at `build()`. Process systems are cyclic-only, like dl systems.
`WiringBuilder` gets the matching `process()` toggle on its dl-system surface.

## 8. Limitations (v1) and future work

- **Cyclic only, stepped serially.** Each `ProcSlot::step` blocks the loop until ack or
  deadline. Overlapping independent workers within a cycle (fan-out doorbells, join before
  dependents) is a natural follow-on once the protocol has soaked.
- **No restart.** A dead or panicked worker is a permanent, telemetered stop.
- **No cross-process async systems**, and therefore no per-ring shared wakes; the ring's
  `wake_word` stays reserved.
- **Platforms:** Linux and macOS 14.4+. Windows has no shared wait-on-address; process systems
  are cleanly unsupported there.
- **`/dev/shm` vs temp files:** regular-file mmap works everywhere; `shm_open`/`memfd` backings
  are a portability/perf refinement later.
