# Implementation plan — cross-process systems

> **Landed 2026-07-08**, one commit per wave, as planned. Notable in-flight
> adjustments: only `ctl`/`worker`/`host` are target-gated (the session dir,
> `Reg::Proc`, and the alloc passes compile everywhere, with a stub `bind_proc`
> and a no-op `worker_entry` elsewhere); `describe_via_worker` ended up `pub`
> (the embedder path to a descriptor without a dlopen, and what the e2e test
> uses); and the W7 harness runs each test on its own thread because
> `stellarator::run` consumes the thread-local executor.

Design: `docs/process-systems.md`. Summary of what we build: a **shared futex** `wake` module in
the ring crate (the `atomic-wait` crate is process-private on every platform, so we mirror its
API over `FUTEX_WAIT` sans private flag / `os_sync_wait_on_address` SHARED), **owner-pid
reclamation** in the ring region (version 3), a per-worker **control block** with a
doorbell/ack step protocol, a **worker process** that re-executes the host binary and drives an
ordinary `DlSlot` over `attach_mmap`ed rings (plus a short-lived **describe mode**, so the host
never dlopens a process system's artifact), the host-side **`ProcSlot`** (spawn, lockstep step,
death detection, reclaim), and the `process=#true` **wiring surface**. Zero dl ABI change
(`FSW_ABI_VERSION` stays 4 — the worker consumes the existing ABI in its own process).

Built in **8 waves**. Dependency graph (strict edges →):

```
W1 (ring wake) ──▶ W3 (control block) ──┬─▶ W4 (worker side) ──┐
W2 (ring reclaim) ──────────────────────┴─▶ W5 (host ProcSlot) ─┴─▶ W6 (wiring) ─▶ W7 (e2e) ─▶ W8 (docs)
```

- **W1 and W2 are independent** (different parts of `ring/src/lib.rs` + a new `ring/src/wake.rs`)
  but touch one crate; do them sequentially in that order, one commit each.
- **W4 and W5 are independent** after W3 (worker half vs host half of the same protocol; the
  `CtlBlock` type in W3 is the shared contract). W6's resolve additionally needs W4's describe
  mode — the graph's W4→W6 edge is real, not just transitive through W7.
- Build + test after each wave. Gates that must stay green throughout:
  `cargo build -p metor-fsw-ring --no-default-features`,
  `cargo build -p metor-fsw-2 --no-default-features`, and the existing dl/slot integration
  tests. The proc surface is `#[cfg(any(target_os = "linux", target_os = "macos"))]`; nothing
  else may grow a cfg.

---

## W1 — ring crate: shared futex `wake` module

**Files:** `ring/src/wake.rs` (new), `ring/src/lib.rs` (module decl + re-export), `ring/Cargo.toml`.

- New feature `futex` adding `dep:libc`. Module is additionally
  `cfg(any(target_os = "linux", target_os = "macos"))`.
- API (mirrors `atomic-wait`, plus the timeout the step deadline needs):
  ```rust
  pub enum WaitOutcome { Woken, TimedOut }
  pub fn wait(a: &AtomicU32, expected: u32);
  pub fn wait_timeout(a: &AtomicU32, expected: u32, timeout: Duration) -> WaitOutcome;
  pub fn wake_one(a: &AtomicU32);
  pub fn wake_all(a: &AtomicU32);
  ```
  Contract in the module docs: returns early if `a != expected` at wait time; spurious wakeups
  allowed; callers loop on their predicate. Works across processes when `a` lives in a
  `MAP_SHARED` region (that is the whole point — spell out the `atomic-wait` private-futex
  finding here so nobody "simplifies" back to it).
- Linux backend: raw `libc::syscall(SYS_futex, ptr, FUTEX_WAIT, expected, timespec_or_null)` /
  `FUTEX_WAKE`. `EAGAIN` (value changed) and `EINTR` fold into a normal return; `ETIMEDOUT` maps
  to `TimedOut`.
- macOS backend: `extern "C"` declarations for `os_sync_wait_on_address`,
  `os_sync_wait_on_address_with_timeout` (`OS_CLOCK_MACH_ABSOLUTE_TIME`),
  `os_sync_wake_by_address_any`, `os_sync_wake_by_address_all`, all with the `SHARED` flag
  (macOS 14.4+; note the availability floor in the docs). Wake on zero waiters returns `ENOENT`
  — fold to ok.
- **Tests** (`ring/src/tests.rs` or `wake.rs` unit tests, threads suffice — shared futexes also
  work intra-process): wake-one releases a waiter; wait on a changed value returns immediately;
  `wait_timeout` times out on silence and reports `Woken` on a real wake; a store+wake / check+
  wait pair never loses an update (loop a few thousand handoffs).
- `MIRI.md`: note the module is syscall-based and excluded from Miri runs.

## W2 — ring crate: owner stamping + `reclaim_owner` (region v3)

**Files:** `ring/src/lib.rs`, `ring/src/tests.rs`, `docs/ring-buffer.md` deferred to W8.

- `ReaderSlot` gains `owner: AtomicU64` after `epoch`; `_pad` shrinks 48 → 40. `VERSION` 2 → 3.
- `RingBuffer::view` stamps `owner = std::process::id() as u64` (Release) right where it bumps
  the epoch. `View::drop` leaves the stamp (slot is `FREE_SLOT`-keyed; the stale owner is inert).
- `RingBuffer::writer` CAS becomes `0 → pid` (pid ≥ 1 for user processes, so `0` stays "free");
  `Writer::drop` and `force_release_writer` unchanged (store 0).
- New `pub unsafe fn reclaim_owner(&self, pid: u64)`: for each slot whose `cursor != FREE_SLOT`
  and `owner == pid`, bump `epoch` then Release-store `FREE_SLOT`; if `writer == pid`,
  Release-store 0. Safety contract: the owning process is dead (no racing stores). Document that
  this is exactly the reclamation discipline the epoch field reserved.
- **Tests:** claim from this process, reclaim by our own pid frees the slot and the writer claim
  and a blocked `try_write` then succeeds; reclaim by a different pid is a no-op; a new `view()`
  after reclaim gets a fresh registration (registration handshake still converges). Miri still
  covers all of this (no syscalls).

## W3 — fsw-2: control block + step protocol (`src/proc/ctl.rs`)

**Files:** `src/proc/mod.rs` (new, cfg'd; `pub mod` in `lib.rs`), `src/proc/ctl.rs`,
`src/proc/tests.rs`, `Cargo.toml` (enable ring `mmap` + `futex` features; add `memmap2`).

- `#[repr(C)] CtlBlock` per the design: immutable header (magic `b"MFC1"`, version, arch tag —
  reuse the ring's `arch_tag()` pattern) + `state`/`doorbell`/`ack`/`status: AtomicU32` +
  `now: AtomicU64`. `state` values: `Booting=0, Attached=1, InitReq=2, Ready=3, ShutdownReq=4,
  Done=5, Failed=6`.
- Two typed halves over one mapping, so misuse is a compile error, not a protocol bug:
  - `CtlHost::create(path)` (host writes header, `state = Booting`) with
    `wait_state(expected_next, timeout)`, `request(state)`, `step(now, deadline) ->
    StepOutcome { Acked(FswStatus), TimedOut }` — store `now` (Relaxed), `doorbell.fetch_add(1,
    Release)`, `wake_one(doorbell)`, then loop `wait_timeout(ack, old, remaining)` until
    `ack == doorbell` (Acquire) or the deadline lapses; on ack, `FswStatus::from_raw(status)`.
  - `CtlWorker::attach(path)` (validates header) with `report(state)`,
    `next(last_seq) -> WorkerCmd { Step { seq, now }, Shutdown }` — loop: Acquire-load `state`
    (shutdown?), Acquire-load `doorbell`; if unchanged, `wait(doorbell, last_seq)` and re-check;
    `done(seq, status)` — store `status`, Release-store `ack = seq`, `wake_one(ack)`.
- The `now` ordering rides the doorbell Release/Acquire pair; state transitions wake both words
  (`wake_all(state)` + `wake_all(doorbell)`) so a worker parked on the doorbell observes
  shutdown.
- **Tests** (threads over one `CtlHost`/`CtlWorker` pair — the protocol is address-space
  agnostic): full lifecycle handshake; N lockstep steps deliver monotonically increasing seq +
  the exact `now` written; a worker that skips (sleeps through two doorbells) runs once for the
  newest seq and the host's next step still converges; host `step` deadline on a silent worker
  returns `TimedOut` and a late ack does not poison the following step; shutdown while parked.

## W4 — fsw-2: the worker side (`src/proc/worker.rs`)

**Files:** `src/proc/worker.rs`, `src/proc/mod.rs`, `src/lib.rs` (re-export `proc::worker_entry`),
`src/bin/metor-fsw.rs` or `src/cli.rs` (guard call).

- `WorkerManifest` (serde/postcard), an enum of the two modes:
  - `Describe { artifact: PathBuf, out: PathBuf }`
  - `Run { abi_version, instance, artifact: PathBuf, params: Vec<u8>, ctl: PathBuf,
    inputs: Vec<PathBuf>, outputs: Vec<PathBuf> }` (ring files in descriptor order — the
    `bind_dl` positional contract).
- `pub fn worker_entry()`: if `METOR_FSW_WORKER` is unset, return immediately (one env read);
  otherwise run the worker and `std::process::exit` with its code. Documented loudly on the
  function and in `docs/process-systems.md` §5: embedders call it first in `main`.
- Describe mode: `DlSystem::open(artifact)` (ABI word checked there), re-encode the
  `SystemDescriptorMsg` to postcard, write to `out`, exit 0. Errors print to stderr and exit
  nonzero — the host folds captured stderr into its resolve diagnostic. (Refactor note:
  `DlSystem::open` decodes the msg into a `SystemDescriptor`; either keep the raw describe bytes
  alongside, or split a `describe_raw(path) -> Vec<u8>` helper out of `open` so describe mode
  writes the .so's bytes verbatim. Prefer the split — verbatim bytes, no re-encode.)
- Worker run mode: read+decode manifest → `CtlWorker::attach` → `attach_mmap` every ring file
  (hold the `RingBuffer`s for the process lifetime; they must outlive the slot) → check
  `abi_version == FSW_ABI_VERSION` → `DlSystem::open(artifact)` → `make_slot(params, inputs,
  outputs, name)` (leak the instance name for the `&'static str`) → `report(Attached)` → wait
  `InitReq` → `slot.init()` (bind_init: claims ring roles, runs `System::init`) →
  `report(Ready)` → loop on `CtlWorker::next`: `Step` → `slot.execute_raw(now)` (via `step` +
  `state()` fold, matching `DlSlot::step`'s panic-destroy policy) → `done(seq, status)`; a
  `Panicked` status also breaks the loop after `done` (state → `Failed`, exit 1). `Shutdown` →
  `slot.shutdown()`, drop slot (`fsw_destroy`), `report(Done)`, exit 0. Any setup error →
  `report(Failed)`, nonzero exit.
- `make_slot`/`execute_raw` are `pub(crate)` already; the worker lives in-crate, no
  visibility changes beyond what compiles.
- **Tests:** manifest round-trip; `worker_entry` no-ops without the env var. (Real worker runs
  are W7's integration tests.)

## W5 — fsw-2: host side — session dir, mmap alloc, `Reg::Proc`, `ProcSlot`

**Files:** `src/proc/host.rs` (new), `src/coordinator/mod.rs`, `src/lib.rs`.

- `CoordinatorConfig` gains `proc_step_timeout: Duration` (default 100 ms),
  `worker_exe: Option<PathBuf>` (default `std::env::current_exe()`), `shm_dir: Option<PathBuf>`
  (default `/dev/shm` if it is a directory, else `std::env::temp_dir()`).
- `CoordinatorBuilder::add_proc_cyclic(name, descriptor: SystemDescriptor, artifact: PathBuf,
  params: Vec<u8>) -> SystemHandle`, `Reg::Proc(ProcReg { artifact, params })` — **no
  `DlSystem`**: the host never dlopens a process artifact; the descriptor arrives as decoded
  describe-worker output (the params schema stays in the wiring resolver, which is the only
  consumer). On non-linux/macos targets the method is absent (cfg) and wiring resolve errors
  instead (W6). All uniform passes (validation, edge resolve, fan-out, sizing) need **no
  change** — they only read descriptors.
- `alloc_rings` backing selection: precompute `shared: HashSet<(sid, out_idx)>` = outputs of
  `Reg::Proc` systems ∪ producer endpoints of edges consumed by `Reg::Proc` systems (from
  `cons_edges`). `alloc_ring` grows a `backing` argument: heap, or mmap at
  `<session>/<instance>.<port>.ring`. The session dir is created lazily on the first shared ring
  and carried in `RingAlloc` → `Coordinator` (removed best-effort in `Coordinator::drop`).
  Record each proc system's ring **paths** (own outputs + producer rings, descriptor order)
  alongside, for the manifest.
- `bind_proc` arm in `bind_systems` (mirrors `bind_dl`): write the manifest into the session
  dir, `CtlHost::create`, spawn `Command::new(worker_exe).env("METOR_FSW_WORKER", manifest)`
  (stdio inherited), wait for `Attached` with a spawn timeout (a few seconds; on timeout or
  early exit: kill, reap, fail `build()` with a `WireError::ProcSpawn`-style error naming the
  likely missing `worker_entry` guard).
- `ProcSlot` (implements `CyclicSlot`):
  - `init`: `request(InitReq)`, wait `Ready` (timeout → treat as died).
  - `step(now)`: skip if stopped; `ctl.step(now, proc_step_timeout)`. `Acked(Running)` → keep
    going; `Acked(Panicked)` → `Stopped { Panicked }` + reclaim; `TimedOut` → `try_wait` the
    child: dead → kill/reap/`reclaim` + `Stopped { ProcessDied }`; alive → count a
    `proc_step_timeout` coordinator-health error (surface: `update_status` already owns
    coordinator health; simplest is a `timeouts: u64` counter on the slot the coordinator folds
    in, or route through a shared `CoordHealth` handle — pick at implementation, keep it
    allocation-free per cycle).
  - `shutdown`: `request(ShutdownReq)`, wait `Done` briefly, then kill+reap regardless.
  - `Drop`: kill+reap if still alive; reclaim.
  - Reclaim = `unsafe { ring.reclaim_owner(child_pid) }` over the recorded ring set (safety: the
    child was reaped first).
- New `StopReason::ProcessDied` (+ `code()`, display, status-frame plumbing next to
  `Panicked`).
- **Tests** (in-crate, no real worker): backing selection marks exactly the crossing rings and
  the session dir appears/cleans up; a `Reg::Proc` graph passes the uniform validation passes;
  `ProcSlot` state transitions driven by a fake `CtlWorker` on a thread (host half is
  process-agnostic, so the whole step/timeout/died matrix is unit-testable — fake the child
  with a `Child`-less test seam or spawn `/bin/sleep`-style stand-ins where a real pid is
  needed).

## W6 — wiring surface + CLI

**Files:** `src/wiring/model.rs`, `src/wiring/parse.rs`, `src/wiring/de.rs`,
`src/wiring/mod.rs` (resolve), `src/wiring/builder.rs`, `src/wiring/error.rs`, `src/cli.rs`.

- `SystemSpec.process: bool` (`#[serde(default)]`). KDL: `process=#true` property on `system`
  nodes, parsed beside `artifact=`.
- Resolve: `process && artifact.is_none()` → new span-carrying `LoadError::ProcessNeedsArtifact`;
  on unsupported targets → `LoadError::ProcessUnsupported`. Otherwise a new `resolve_proc`:
  spawn a describe-mode worker (W4) against the built artifact path with a timeout, decode the
  output file's `SystemDescriptorMsg` (descriptor + params schema — the same decode/reject
  checks `DlSystem::open` applies, shared as a helper), encode params through the existing
  schema-guided path, and call `add_proc_cyclic`. Failure diagnostics carry the worker's
  captured stderr. The host does **not** `DlSystem::open` process artifacts.
- `WiringBuilder`: `process()` toggle on the dl-system spec method (same `SystemSpec`, byte-
  equivalent params, matching the KDL front-end — extend the front-end-equivalence test).
- CLI: `worker_entry()` as the first line of `cli::run` (and note in `docs/cli-runner.md`, W8).
- **Tests:** KDL parse round-trip of `process=#true`; both error paths; front-end equivalence
  (KDL vs builder produce identical `Wiring`).

## W7 — end-to-end integration tests

**Files:** `tests/proc_integration.rs` (new), reusing `tests/fixtures/dl-fixture` (its system is
already the dl e2e vehicle; add ports/params there only if the existing shape is insufficient).

Mirror `tests/dl_integration.rs`'s fixture-build harness. The test binary itself is the worker
executable: call `worker_entry()` at the top of `main` via a `#[cfg(test)]`-friendly harness —
integration tests have their own `main`, so use a custom test harness (`harness = false`) for
this one file, calling `worker_entry()` then `libtest_mimic`-style or plain sequential test fns
(match whatever `dl_integration.rs` does for fixture builds; keep it minimal).

- **Lockstep e2e:** static producer → process-system (dl-fixture) → static consumer, N sim
  cycles; assert the consumer saw the fixture's transform, health frames flowed from the worker,
  and shutdown reaps the child + removes the session dir.
- **Death + reclaim:** run a few cycles, SIGKILL the child, run more cycles; assert the slot
  reports `Stopped { ProcessDied }`, upstream producers keep publishing without
  `publish_dropped` accumulation (reader slots were reclaimed), and the loop never stalls longer
  than the step deadline.
- **Timeout without death:** (if cheap) a fixture param that sleeps once past the deadline;
  assert one `proc_step_timeout`, no stop, and recovery on the next cycle.
- **Mixed modes:** one static + one dlopen'd + one process system in a single wiring, KDL
  front-end, `--sim-dt`-style config; asserts all three exchange frames.

## W8 — documentation pass

- `DESIGN.md`: rewrite the "Running systems as cdylibs" cross-process caveat and the
  Limitations bullet; add `docs/process-systems.md` to the document map.
- `docs/ring-buffer.md`: region v3 (owner word, pad 40), `reclaim_owner`, the `wake` module
  (§8 async-wake and §"future" cross-process bullets), MIRI exclusions.
- `docs/dl-open.md`: a short "the worker process consumes this same ABI" cross-reference.
- `docs/cli-runner.md`: `worker_entry` note; `docs/wiring.md`: `process=#true`.

---

## Risks / decisions to watch

- **R1 — macOS availability floor:** `os_sync_wait_on_address` needs macOS 14.4+. If an older
  floor turns out to matter, the fallback is the private `__ulock_(wait|wake)` with
  `UL_COMPARE_AND_WAIT_SHARED`; keep the backend behind one function pair so swapping is local.
- **R2 — re-exec guard:** a host app that forgets `worker_entry()` re-runs its own main in the
  child. The spawn-timeout error at `build()` names the guard explicitly; consider also setting
  a sentinel env var the coordinator checks at construction to catch recursive missions.
- **R3 — blocking waits on the stellarator loop:** `ProcSlot::step` blocks the executor thread
  up to the deadline, starving async tasks (telemetry sender) in that window. Same order of
  magnitude as an in-process system computing; acceptable v1, revisit with overlapped stepping.
- **R4 — health surface for timeouts:** the worker owns the system health ring, so host-side
  timeout counts must land on coordinator health. Decide the exact plumbing in W5 (slot-side
  counter folded by `update_status` is the least invasive).
- **R5 — ring version bump:** v3 regions reject v2 attaches (and vice versa). Regions are
  ephemeral, but any long-running panel/db tooling that attaches rings directly must be rebuilt
  in the same train; check for out-of-tree `attach_raw` users before landing W2.

---

## Follow-on: W9 restart + W10 worker telemetry (landed 2026-07-08)

- **W9 — restart.** `ProcSlot` grew a non-blocking phase machine (`Running` →
  `Backoff` → `Attaching` → `Initing` → back to `Running`, or `Terminal` past
  the budget), polled one phase per cycle so a respawn never stalls the loop.
  Restart applies to `ProcessDied` *and* worker-side `Panicked` (quarantined,
  so restartable — unlike the in-process dl path). Knobs:
  `CoordinatorConfig::proc_max_restarts` (default 3, `0` = permanent-stop) and
  `proc_restart_backoff` (default 500 ms). Respawn reuses the persisted
  manifest over a recreated control block; every attempt costs one unit of
  budget; each begun restart drains into coordinator health as `proc_restart`.
- **W10 — telemetry.** `CoordinatorStatus` gained `worker_count` plus a
  `FrameList<WorkerEntry, MAX_WORKERS>`: per process system the worker pid
  (`0` between workers), restart count, and a `WorkerRunState` code
  (Stopped=0/Restarting=1/Running=2), republished on any change (a restart's
  new pid included). `CyclicSlot::proc_info` is the collection hook (only
  `ProcSlot` overrides) and `Coordinator::workers()` the host-side accessor.
- **e2e:** the death test pins `proc_max_restarts: 0` (the opt-out), and
  `worker_restarts_then_exhausts_budget` kills the worker, proves a
  replacement spawns and produces, kills it too past a budget of 1, and
  asserts the terminal stop, `restarts == 1`, pid `0`, and a never-blocked
  producer.
- **Slots:** process mode still applies to `system` nodes only; a
  worker-per-occupant mode for runtime slots is future work (the occupant
  Load/Unload lifecycle would map onto spawn/kill, which the restart
  machinery now provides the primitive for).
