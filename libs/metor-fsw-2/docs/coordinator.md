# The Coordinator

The **coordinator** is the single piece of software that owns the ring regions, wires the system
graph, drives cyclic systems once per cycle, spawns async systems, folds in the async copy-in step,
and provisions per-system health. It is the runtime that turns a set of independently-written
systems into a running flight-software graph.

The code lives in `src/coordinator/mod.rs`, with the acceptance tests in `src/coordinator/tests.rs`.
The `bind` contract is in `src/binder.rs`; the dlopen slot is in `src/dl.rs`.

Almost everything below the builder is reuse: the data path is the ring (`metor-fsw-ring`), the
typed port wrappers (`src/port.rs`), the system descriptors (`src/descriptor.rs`), the health port
(`src/health.rs`), and the `CyclicRunner` per-system owner (`src/system/mod.rs`). The coordinator
adds the builder and its validation/sizing pass, the `bind`/`Binder` contract, the per-system
slot lifecycle, the copy-in jobs, the telemetry registry, the dlopen integration, and the
lifecycle driver.

---

## 0. Orientation

The coordinator runs in two phases.

1. **Build phase (no cycles running).** `Coordinator::builder(config)` returns a
   `CoordinatorBuilder` that registers systems (`add_cyclic`/`add_async`/`add_dl_cyclic`)
   and edges (`connect`/`connect_delayed`). Registration only records each
   system's `SystemDescriptor` (the derives already produce it) plus a boxed, type-erased
   registration. `build()` runs the validation pass (`compatible()` on every edge; single-connect,
   unconnected-input, and unbroken-feedback-cycle checks), allocates one heap-backed `RingBuffer`
   per output port sized from the descriptors and the fan-out of the edge set, allocates a private
   copy-in buffer per async input, auto-provisions the coordinator's own health/log/status buffers,
   binds every system's typed `Output`/`Input` ports over those rings, and returns a ready
   `Coordinator`.

2. **Run phase.** `Coordinator::run_for(cycles)` executes on `stellarator`. It spawns each async
   system (each of which `init`s itself and signals readiness), waits on an init barrier, runs the
   cyclic `init`s, releases the async tasks, then drives the cyclic cycle loop for `cycles`
   iterations — stepping every cyclic system, running the copy-in step, refreshing the status frame,
   and pacing the rate. On teardown it signals and joins the async tasks and `shutdown`s every
   cyclic system.

---

## 1. Coordinator types & ownership

### 1.1 Configuration

```rust
pub enum ClockMode {
    /// Wall-clock time: each cycle's `now` is `Timestamp::now()`, and the loop
    /// sleeps to hold `cycle_rate` (run-fast-then-wait). The default.
    Wall,
    /// A simulated clock: each cycle's `now` advances by `dt` from a start epoch
    /// and the loop does not sleep — it runs cycles as fast as possible.
    Simulated { dt: Duration },
}

pub struct CoordinatorConfig {
    /// The single global cycle rate the loop holds (run-fast-then-wait) under a
    /// Wall clock. Every cyclic system runs every cycle; there is no per-system
    /// rate division. Ignored under a Simulated clock.
    pub cycle_rate: Hz,            // e.g. 100.0 → a 10 ms budget
    /// In-flight record depth for a buffer whose PortDesc carries no rate hint.
    pub default_depth: usize,      // defaults to DEFAULT_DEPTH (= 8)
    /// The clock driving the per-cycle `now` and loop pacing (default Wall).
    pub clock: ClockMode,
}
```

A single global `cycle_rate` drives all cyclic systems: every cyclic system runs every cycle. There
is no per-system rate division — a cyclic system that wants a slower effective rate divides cycles
itself. The `Simulated` clock makes a mission converge in fixed logical time regardless of host
speed: each cycle's `now` is `epoch + k*dt` and the loop never sleeps, which is what the
deterministic-timestamp test relies on.

### 1.2 The coordinator owns the rings; systems borrow into them

```rust
pub struct Coordinator {
    config:        CoordinatorConfig,
    cyclic:        Vec<Box<dyn CyclicSlot>>,   // type-erased slots, in run (registration) order
    pending_async: Vec<PendingAsync>,          // bound async systems, spawned at run
    copy_ins:      Vec<CopyIn>,                 // private-buffer copy-in jobs for async inputs
    coord_health:  HealthPort,                  // the coordinator's own health/log
    status_out:    Output<CoordinatorStatus>,   // the coordinator status frame writer
    status_view:   Input<CoordinatorStatus>,    // a reader for status read-back
    stopped:       Vec<StoppedSystem>,          // currently hard-stopped systems
    cycle:         u64,
    registry:      Arc<OutputRegistry>,         // broad by-id index over every tappable buffer
    rings:         RingTable,                   // owns every RingBuffer — drops last
}
```

This realizes the ownership rule "systems own their outputs and borrow their inputs" concretely. A
`RingBuffer` is `Arc`-backed and cheaply clonable, so the coordinator holds the
canonical handle in `RingTable` while each system's ports hold `Writer`/`View` clones derived from
it. Keeping the canonical handle alive in `RingTable` — declared **last** in the struct so it drops
after every other field — guarantees a buffer outlives every port over it and every dlopen'd slot
that attached to its region, regardless of teardown order.

### 1.3 `RingTable` — the ring registry

```rust
struct RingTable { rings: Vec<RingEntry> }

struct RingEntry {
    ring:     RingBuffer,
    frame_id: ComponentId,            // the frame the buffer carries (for diagnostics)
    role:     BufferRole,             // Output { system, port } | Private { system, input } | Coordinator
    instance: Option<String>,         // owning system's instance name; None for coordinator buffers
}
```

There is exactly **one writer per `RingEntry`**. The table is where that invariant holds
structurally: a buffer is created for one producing port (or one copy-in job, or the coordinator),
and the builder calls `RingBuffer::writer(...)` exactly once for it. `instance` records the owning
system's instance name so the telemetry sink can prefix that buffer's records (§5).

### 1.4 Per-system slot ownership

A **cyclic** system, its `Input` bundle, and its `Out<Output>` bundle are owned together by the
coordinator across cycles. This is exactly what `CyclicRunner<S, O>` (`src/system/mod.rs`,
`CyclicRunner { system, input, output, state }`) holds. The coordinator stores cyclic systems as a
heterogeneous `Vec<Box<dyn CyclicSlot>>`; `CyclicRunner`, the dlopen `DlSlot`, and the
runtime-loadable `SlotRunner` (`docs/sequences-slots.md`) all implement `CyclicSlot` (§3.4).

An **async** system is different: its `run` future borrows `&mut self`, `&mut Input`, `&mut Output`
for the whole loop, so those three move *into* the spawned task and are owned by it, not by the
coordinator (§4.1). The coordinator keeps only an `AsyncTask` handle to drive lifecycle.

### 1.5 The `bind` contract — `BindPorts`, `Binder`, `BoundPort`

The system derives generate `descriptors()` (for the build pass) and a symmetric `bind` (for
construction). The key invariant is that **`bind()` walks the port fields in the same order as
`descriptors()`**, so a positional cursor lines each port up with the ring the builder
pre-allocated for it.

```rust
pub trait BindPorts: Sized {
    /// Construct every port from the ring source, in descriptors() order.
    fn bind<S: RingSource>(src: &mut S) -> Self;
}
```

A `RingSource` hands out one pre-allocated ring per port. Rings are backing-erased, so a
single generated bundle `bind` serves both providers:

- The host's **`Binder`** walks the coordinator's pre-allocated
  `BoundPort`s, popping one ring per port via `next_output`/`next_input` in `descriptors()` order.
- A dlopen'd system's **`RawBinder`** (on the `.so` side of the ABI)
  walks the host's raw ring regions. The static-system path never sees this — a `.so`
  compiles in its own `CyclicRunner` over the same port types (§3.6).

```rust
pub struct BoundPort {
    ring:  RingBuffer,
    data:  Option<Box<dyn Any>>,   // matched wake endpoints, copy-in path only
    space: Option<Box<dyn Any>>,
}
```

`Out<O>` (the framework wrapper) binds its inner `O`, then constructs its `HealthPort` from the
two auto-provisioned health/log rings (§5), mirroring how `Out::descriptors()` pushes the
health/log descriptors after the user ports.

**Matched wake endpoints.** A `Notifier` is `Arc`-backed: a commit only wakes an awaiting reader
when the writer side and the view side hold the *same* clone. Every cross-system edge is sampled by
a polling consumer — a cyclic system each cycle, or a copy-in job each cycle — so its view can use a
fresh default wake and the match is moot. The one place a matched clone is load-bearing is the
private copy-in buffer feeding an async input: there the coordinator pre-creates the `Notifier`
pair, stores it type-erased on the port's `BoundPort` via `BoundPort::matched`, and hands the
matched clone to the async view. Every other port uses `BoundPort::new` and the `Binder`
default-constructs the wake.

A bundle that wants by-id access to *every* output — the telemetry downlink, a logger, a recorder —
pulls the `OutputRegistry` from `RingSource::output_registry()` in its own `bind`, exactly where it
pulls its typed ports. Only the host `Binder` carries one, so a system that needs it is host-only.

---

## 2. Graph builder, compatibility validation, sizing

### 2.1 The builder API

```rust
impl Coordinator {
    pub fn builder(config: CoordinatorConfig) -> CoordinatorBuilder;
}

impl CoordinatorBuilder {
    pub fn add_cyclic<S, O>(&mut self, system: S) -> SystemHandle;
    pub fn add_cyclic_named<S, O>(&mut self, name: impl Into<String>, system: S) -> SystemHandle;
    pub fn add_async<S>(&mut self, system: S) -> SystemHandle;
    pub fn add_async_named<S>(&mut self, name: impl Into<String>, system: S) -> SystemHandle;
    pub fn add_dl_cyclic(&mut self, name: impl Into<String>,
                         loaded: dl::DlSystem, params: Vec<u8>) -> SystemHandle;

    pub fn connect(&mut self, producer: PortRef, consumer: PortRef) -> Result<(), WireError>;
    pub fn connect_delayed(&mut self, producer: PortRef, consumer: PortRef) -> Result<(), WireError>;

    pub fn build(self) -> Result<Coordinator, WireError>;
}

pub struct PortRef { pub system: SystemHandle, pub frame_id: ComponentId }
impl PortRef { pub fn new<F: Frame>(system: SystemHandle) -> Self; }
```

`add_*` stores the boxed system, `S::descriptor()` (which enumerates `inputs`/`outputs` as
`Vec<PortDesc>`), the system kind, and an instance name. The builder works entirely off
`SystemDescriptor`/`PortDesc` — it never touches the frame type `F` directly. Registration order
becomes the run order.

Each system carries an **instance name** (the `_named` variants set it; the plain variants default
to `System::NAME`). The instance name disambiguates two instances of one system type at the
telemetry sink: their records are prefixed `<instance>.<frame>.<component>`, so they emit distinct
fully-qualified paths despite sharing a `frame_id`. The wiring loader (`wiring.md`) supplies the KDL
instance names.

Ports are addressed by `(SystemHandle, frame_id)` via `PortRef`; both come straight off the derived
`SystemDescriptor`, so a wiring front-end can resolve a named KDL edge to a `connect` call.

### 2.2 Compatibility validation (build-time, before any cycle)

For every edge, `build()` looks up the producer's matching output `PortDesc` and the consumer's
matching input `PortDesc` (by `frame_id` position in the descriptors) and calls
`compatible(producer, consumer)` (`src/descriptor.rs`):

- same `frame_id`, and
- the consumer's realized `(component_id, ty, shape)` set is a **subset** of the producer's
  (forward-compatible: a producer may emit extra fields a consumer ignores).

The builder drives `compatible` per edge and turns a `false` into `WireError::Incompatible` naming
both systems. This catches every wiring mistake before a byte flows.

The same pass enforces the structural rules:

- **Every input port is connected exactly once.** An input with no edge is
  `WireError::UnconnectedInput` (nothing would ever write it). Two edges into one input is
  `WireError::DoubleConnect` (an input is a single `View` into a single producer; combining is the
  consumer's job, via separate ports).
- **Frame-id match on the edge** is rejected early at `connect` time (`WireError::FrameIdMismatch`),
  a friendlier error than the subset failure. `connect` also rejects an unknown system handle
  (`WireError::UnknownSystem`); an unknown port surfaces at `build` (`WireError::UnknownPort`).
- **Single writer per buffer** is structural (§2.5), not a checked rule.
- **Every feedback loop must be broken by a `connect_delayed`** (§3.1), else
  `WireError::FeedbackCycle`.

### 2.3 Buffer sizing & `max_readers` derivation

One buffer per **output** port, plus the coordinator's own health/log/status buffers (§5), plus one
private buffer per **async input** (§4). Each is sized from its `PortDesc` and the edge set:

```
capacity    = capacity_for(port.max_size, depth)   // frame_len(max_size) * depth, pow2
max_readers = fan_out(port) + n_registry_consumers + reader_slack
```

Every ring is **lossless** — there is no per-buffer overrun policy to choose (`alloc_ring` is
the one sizing path).

- `capacity_for` / `buffer_capacity::<F>` (`src/port.rs`) are the sizing helpers. Depth is by
  delivery: a Snapshot port gets `config.default_depth` (a latest-wins sample needs little
  history), a Log port gets `LOG_DEPTH` (an every-record stream must absorb a slow tap). There
  is no rate-derived depth (review finding C1: the earlier advisory `PortDesc::rate_hint` had
  no consumer and was deleted).
- **`max_readers` must cover every `view()` the builder will register**, because the ring has no
  crash-slot reclamation: a reader slot is reserved at build time and never reclaimed. It is the
  sum of:
  - `fan_out(port)` — the number of distinct consumers of that output (each cyclic consumer is a
    direct `View`; each async consumer is **one** copy-in `View`, §4);
  - `n_registry_consumers` — each system that pulls the broad `OutputRegistry` (the telemetry
    downlink, a logger) is an extra reader on **every** output buffer;
  - `config.reader_slack` (default 4) — slack for late taps such as a db/telemetry sink or a
    debugger.

### 2.4 The output registry

Every output buffer (and the coordinator's own health/log/status buffers) is recorded in a broad
`OutputRegistry`, keyed by the instance-qualified id `ComponentId::new("<instance>.<frame>")`. The
registry is frozen into an `Arc<OutputRegistry>` *before* the bind loop, so a system can pull it in
its own `bind` (the telemetry downlink does this), and is also exposed to callers via
`Coordinator::registry()` — an index a logger, recorder, debugger, or test uses to read any output
by id. See `telemetry.md` for the full registry/sink design.

### 2.5 Single-writer-per-buffer enforcement

A buffer is created **for exactly one producer** and its only `Writer` is handed to that producer
during `bind`. The builder calls `RingBuffer::writer(...)` once per buffer; a private async buffer's
single writer is the coordinator's copy-in job. `connect` only ever adds `view()` (reader)
registrations to an existing producer buffer — there is no API by which two systems write one
buffer. "Single writer" is therefore an invariant of the build graph, not a runtime check.

---

## 3. The cycle loop (cyclic systems)

### 3.1 Ordering & the sampling rule (feedback loops)

Cyclic systems run in a **fixed per-cycle order: registration order.** The sampling rule is a direct
consequence of shared rings plus ordered execution, and needs no extra machinery:

- A cyclic system reads each input with `Input::latest()`, which borrows the **newest
  committed record at the instant it runs** (consuming older unread records on the way).
- **Forward edge** (producer registered *before* consumer): the consumer sees **this cycle's fresh**
  output.
- **Back edge / feedback** (producer registered *after* consumer): the consumer sees the producer's
  **previous-cycle** output — a natural **one-cycle delay**. No system ever blocks waiting for a
  not-yet-produced input; it reads the latest available.

Feedback loops are explicit, not implicit. Every directed cycle in the graph must break exactly one
edge with `connect_delayed`, which marks it as the intentional one-cycle-delayed back-edge. At
`build()` the builder constructs the system adjacency over the **non-delayed** edges only and runs
`find_cycle` (§3.1.1); any remaining cycle is an unbroken feedback loop and fails with
`WireError::FeedbackCycle` naming the loop members. This makes the one-cycle-late sampling a
declared property of the wiring rather than an artifact of registration order. The runtime path of
`connect_delayed` is identical to `connect` — a `latest()` read of the committed value, which is
last cycle's because the producer runs after the consumer.

#### 3.1.1 Cycle detection (`find_cycle`)

`find_cycle(adj)` is a plain depth-first search colouring nodes white/grey/black. It pushes each
node onto a DFS stack as it greys it; a back-edge to a grey (on-stack) node closes a cycle, which is
reconstructed as the stack tail from that node onward and returned in loop order. It returns the
first cycle found, or `None` if the graph is acyclic. The builder runs it over the non-delayed
adjacency only, so delayed back-edges never count toward a cycle.

### 3.2 Run-fast-then-wait timing

`run_for(cycles)` runs every cyclic system as fast as possible, then holds the rate by waiting at
the end of each cycle. One cycle:

```rust
let budget = Duration::from_secs_f64(1.0 / config.cycle_rate);
for k in 0..cycles {
    let start = Instant::now();
    self.cycle += 1;
    let now = match config.clock {
        ClockMode::Wall          => Timestamp::now(),
        ClockMode::Simulated{dt} => epoch + dt * k,
    };
    for slot in &mut self.cyclic { slot.step(now); }   // §3.3, registration order
    self.run_copy_ins();                               // async copy-in (§4.3)
    self.update_status(now);                           // refresh status frame (§3.5)
    match config.clock {
        ClockMode::Wall => {
            let elapsed = start.elapsed();
            if elapsed < budget { stellarator::sleep(budget - elapsed).await; }
            else { self.telemeter_overrun(now, elapsed, budget); }
        }
        ClockMode::Simulated{..} => stellarator::yield_now().await,
    }
}
```

- **Overrun** under a `Wall` clock (a cycle exceeds its budget): the loop does **not** sleep; it
  telemeters a coordinator-level health frame (`cycle_overrun`, §5.3) and starts the next cycle
  immediately. The loop never skips work to "catch up" — it runs continuously when saturated.
- Under a `Simulated` clock the loop never paces; it still `yield_now()`s once per cycle so spawned
  async consumers — driven by the copy-in step — get to run on the cooperative runtime.
- `now` is threaded into every `step` in a cycle, so all systems share one timestamp.

`run_for` runs a bounded number of cycles, which is what the tests and bounded missions use; an
unbounded mission is a large `cycles` count.

### 3.3 Backpressure instead of laps

The ring is **lossless**: a writer can never overwrite a record a reader has not consumed, so
a slow consumer can never be lapped and there is no lap-triggered stop. Slowness surfaces on
the write side instead — a producer whose `publish` is rejected (`WouldBlock` from a slow
reader, or `InsufficientCapacity` from a sizing bug) drops that record with a counted drop,
which `CyclicRunner::step` folds into the producer's health as a `publish_dropped` error.
Both systems keep running.

`step` itself is simple: if the slot is stopped it does nothing; otherwise it times
`execute`, folds the output bundle's `take_dropped()` sum into health, and publishes the
cycle's health record (`end_cycle`). The only permanent hard-stop left is a `.so`-boundary
panic (`StopReason::Panicked`, §3.6). A permanently stopped `DlSlot` releases its foreign
state (and thus its reader slots) **immediately** on stop — on a lossless ring a dead
consumer's pinned views would otherwise backpressure every upstream producer forever (§3.6).
A stopped system is never restarted (no recovery hook).

### 3.4 `CyclicSlot` — the per-system slot trait

The coordinator drives cyclic systems through a `CyclicSlot` trait object so it can hold a
heterogeneous set:

```rust
pub(crate) trait CyclicSlot {
    fn init(&mut self);
    fn step(&mut self, now: Timestamp);
    fn shutdown(&mut self);
    fn name(&self) -> &'static str;
    fn state(&self) -> &SlotState;
}
```

`CyclicRunner<S, O>` is the static-system implementation: it owns `system/input/output/state`, times
`execute` with `Instant::now`, folds `take_dropped()` into a `publish_dropped` health error, and
calls `output.health().end_cycle(now, micros)` (§3.3). `DlSlot` is the dlopen implementation (§3.6);
`SlotRunner` (`docs/sequences-slots.md`) is the third, a runtime-swappable slot whose *occupant* can
be loaded/started/stopped/reset at runtime rather than being fixed at `build()`. `SlotState` is
shared by all three, since a runtime slot's lifecycle is a strict superset of the static two-state
one:

```rust
pub enum StopReason { Panicked }

pub enum SlotState {
    Empty,              // no occupant yet (SlotRunner only)
    Loaded,              // occupant built, not yet polling (SlotRunner only)
    Running,             // polled every cycle — the only state CyclicRunner/DlSlot ever reach
    Done { outcome: u8 }, // the occupant's future returned Ready — terminal success (SlotRunner only)
    Stopped { reason: StopReason },
}
```

`Panicked` — the only stop reason — is reachable only by a `.so`-backed slot (`DlSlot`, or a
`SlotRunner` whose occupant panics inside its boundary): it returns `FswStatus::Panicked` and
the slot hard-stops. A static `CyclicRunner` cannot produce it (a panic there unwinds the host
directly), so a static `CyclicRunner` never actually stops; it and a `DlSlot` only ever occupy
`Running` or `Stopped`. `Empty`/`Loaded`/`Done` are reachable only through a `SlotRunner`'s
runtime lifecycle commands (`docs/sequences-slots.md` §3, `docs/messages.md` §4). Once
`Stopped`, a slot is never cleared by any kind (a `SlotRunner`'s `Stopped` still needs an
explicit `Reset` to become `Loaded` again, not an automatic clear).

### 3.5 Surfacing stopped systems

Each cycle, `update_status(now)` scans the slots and collects the currently-stopped set plus the
process systems' worker facts. When either changes it:

- refreshes the `CoordinatorStatus` frame (`publish_status`) — a coordinator-owned output frame
  (NAME `"coordinator"`, frame name `"coordinator_status"`) carrying a `FrameList<StoppedEntry>` of
  up to `MAX_STOPPED` (= 32) entries, each a reason code plus a fixed-capacity name buffer
  (`STATUS_NAME_CAP` = 48), and a `FrameList<WorkerEntry>` of up to `MAX_WORKERS` (= 32) entries —
  one per process system, carrying the worker's pid (`0` between workers), restart count, and a
  `WorkerRunState` code (Stopped=0/Restarting=1/Running=2), which is how telemetry says a system
  runs out-of-process at all (`Coordinator::workers()` is the host-side accessor); and
- logs a stopped-set change to coordinator health (a `system_stopped` error counter plus a
  `Level::Warn` log line per stopped system, then `end_cycle`). Worker step timeouts and restarts
  land there too, as `proc_step_timeout` / `proc_restart` counters.

`Coordinator::stopped()` returns the live `&[StoppedSystem]` and `read_status()` reads the published
status frame back (name + reason code), for telemetry and test inspection.

### 3.6 dlopen'd systems

A system compiled as a `cdylib` is loaded by `DlSystem` (`src/dl.rs`), which opens the `.so`,
resolves its `fsw_*` symbols, checks the ABI version word, and `fsw_describe`s it into a
`SystemDescriptor`. From the builder's view this is the twin of `add_cyclic_named`: `add_dl_cyclic`
pushes that reconstructed descriptor, so the **same** `compatible()`/`WireError` validation, ring
sizing, allocation, and registry entry run over it unchanged. dlopen'd systems are cyclic-only.

The difference is at `bind`. Instead of typed `BoundPort`s walked by a `Binder`, the coordinator
gathers the raw ring regions the dlopen path needs — each port's `(base, len)` and role, as
`FswRing` handles in `descriptors()` order. Outputs are this system's own buffers; inputs are
`region()`s of the upstream producers' output buffers (the cyclic-consumer path, read directly). It
passes them, plus the postcard `params` blob, to `DlSystem::into_slot`, which `fsw_create`s the
state and returns a `DlSlot`. The `.so` reconstructs each ring over the host's heap region
via `attach_raw` (same process, no copy, no IPC — it sees the identical atomics the host's other
systems do), and its `CyclicSlot::step` forwards over the ABI, mapping `FswStatus::Panicked` to
`SlotState::Stopped { reason: Panicked }`.

A permanent stop destroys the foreign state **immediately**, not at teardown: on the stop
transition the slot calls `fsw_destroy` at once, dropping the `.so`'s raw-attached ports and
freeing its reader slots — on a lossless ring a dead consumer's pinned views would otherwise
backpressure every upstream producer forever.

Teardown ordering is load-bearing: a `DlSlot`'s `Drop` calls `fsw_destroy` (idempotent — a
no-op if the stop already destroyed the state) before its `Arc<Library>` field unloads and
before the host `RingTable` frees the regions. The coordinator drops its `cyclic` slot vec
before its `rings` field (struct field order), so no raw attach outlives its region and no
`.so` code runs after its `Library` is gone. See `dl-open.md` for the full ABI.

### 3.7 Single-threaded execution

The cyclic loop runs **single-threaded** on one stellarator task. This keeps the sampling rule
(§3.1) exact — there are no intra-cycle races on read order — and is the simplest model. Async
systems and the copy-in step share the same executor cooperatively.

---

## 4. Async systems

### 4.1 Spawn once

For each async system the coordinator spawns its `run` loop exactly once. Because
`AsyncSystem::run(&mut self, &mut Input, &mut Output)` borrows all three for the loop's lifetime, the
system, its `Input` bundle, and its `Out<Output>` move **into** the spawned task (`AsyncSlot`):

```rust
stellarator::spawn(async move {
    me.system.init(&mut me.output);             // init inside the task, before run
    ctx.ready_count.fetch_add(1, Release);      // signal the init barrier (§6)
    ctx.ready.wake_all();
    ctx.go.wait_for(|| ctx.go_flag.load(Acquire)).await;   // hold at the go-gate
    loop {
        if ctx.stop.load(Acquire) { break; }
        me.system.run(&mut me.input, &mut me.output).await;  // one pass; system paces itself
    }
    me.system.shutdown(&mut me.output);         // shutdown inside the task, after the loop
});
```

The system's own `run` decides pacing — await an input via `Input::recv` (backed by a `Notifier`
sink) or `stellarator::sleep` on a timer. The coordinator only spawns the task and later signals
teardown. The `JoinHandleDropGuard` cancels the task if a `Coordinator` is dropped mid-run.

`init`/`shutdown` for an async system run **inside its task** (the only owner of the bundle). The
init barrier (§6) ensures every system's `init` has completed before any first `run` pass or cycle.

### 4.2 Private copy-in buffers (the input side)

An async **Snapshot** input does **not** view the upstream output directly; it views a **private
buffer the coordinator owns**. For each such input port the builder allocates a private
heap-backed `RingBuffer` (lossless, like every ring), sized like any buffer with `max_readers =
1 + reader_slack` (the async system's view plus slack). Two ports straddle it:

- **Writer side — the copy-in job** (`CopyIn`), owned by the coordinator (the buffer's single
  writer). It views the upstream producer's output (`NoWake`) and mirrors records into the private
  buffer.
- **Reader side — the async system's `Input` port**, bound with a `Notifier` data sink (the matched
  wake from §1.5) so `Input::recv` wakes when copy-in commits.

The copy-in mirrors only the **newest** upstream record, and its `try_write` never blocks: a full
private ring (the async consumer is behind) means that cycle's mirror is simply skipped and the
next cycle retries with whatever is newest then. Latest-wins, no intermediate buffer, and the
cycle loop never suspends on an async consumer.

### 4.3 Where copy-in runs

The coordinator **folds copy-in into the cycle loop** (`run_copy_ins()` after the cyclic systems
each cycle):

```rust
fn run_copy_ins(&mut self) {
    for c in &mut self.copy_ins {
        // Skip untouched upstreams: `committed` moves iff a record landed on this
        // ring — this also keeps the pinned newest record from being re-mirrored
        // (and the consumer re-woken) every cycle.
        let committed = c.upstream.committed();
        if committed == c.last_committed { continue; }
        c.last_committed = committed;
        if let Ok(Some(grant)) = c.upstream.try_latest() {
            let _ = c.writer.try_write(&grant);   // full private ring = skip this mirror
        }
    }
}
```

This keeps the loop single-threaded and avoids requiring a `Notifier` on every *cyclic* producer's
output (a cyclic producer writes with `NoWake`; a separate awaiting copy-in task would need the
producer to notify). The cost is that async input latency is bounded by the cycle rate. The mirror
runs **at most once per new upstream commit** — the `committed`-word cache dedups unchanged
upstreams — and the record is borrowed in place off the upstream ring (`try_latest`) and written
straight through, with no scratch copy. The `try_write` notifies the private buffer's data
`Notifier`, which wakes the async `run`'s `recv`.

### 4.4 Async outputs feeding cyclic consumers

There is nothing async-specific on the **output** side: an async system writes its output ring at
its own pace, and a cyclic consumer holds an ordinary `View` it reads with `Input::latest()` each
cycle — latest-wins, same as any edge. A fast async producer that fills the ring is backpressured
like any writer: its `write_async` suspends until the consumer frees room (§3.3). No special
handling.

---

## 5. Health & status provisioning

### 5.1 Per-system health/log buffers

Every system implicitly produces a `SystemHealth` and a `SystemLog` frame: `Out::descriptors`
pushes `PortDesc::of::<SystemHealth>()` and `PortDesc::of::<SystemLog>()` after the user ports, and
`Out::bind` constructs the system's `HealthPort` from the two corresponding rings. These buffers are
sized and allocated like any output (§2.3); the user does not wire them.

### 5.2 Driving the standard counters

The framework drives the standard counters around `execute`: `CyclicRunner::step` times `execute`,
folds any counted `publish` drops into a `publish_dropped` health error (§3.3), and calls
`HealthPort::end_cycle(now, micros)`, which bumps `cycles`, stamps `last_execute_micros`,
and publishes one `SystemHealth` record plus any pending log lines. For async systems the
same `HealthPort` rides in their `Out<Output>` inside the task, and the counters around their work
are driven from within `run` — the coordinator does not tick them.

### 5.3 Coordinator-level health & status

The coordinator owns its own `HealthPort` (NAME `"coordinator"`) and a `CoordinatorStatus` frame for
cycle-level events that belong to no single system: **cycle overruns** (§3.2) and **hard-stopped
systems** (§3.5). The status frame names which systems stopped and why; the health/log frames carry
the per-event counters and log lines. These reuse the same frame types and ports as any system.

### 5.4 Where frames go & the instance prefix

Health is just frames: the health/log/status buffers are ordinary output rings, so any consumer —
the telemetry downlink, metor-db, a UI, a test — reads them with a `View` like any other output, via
the `OutputRegistry` (§2.4).

Each buffer's records must land namespaced so two systems' health do not collide in db
(`filter.health.cycles` vs `nav.health.cycles`). The frames are derived with fixed names
(`"health"`/`"log"`), so the prefix is applied **at the telemetry sink, per buffer**, not baked into
the on-ring frame bytes: the registry entry for each buffer is keyed by `<instance>.<frame>` and
carries an `announce` vtable already prefixed with the owning system's instance name. Coordinator
buffers use the synthetic instance `"coordinator"`, so they downlink under `coordinator.health` /
`coordinator.log` / `coordinator.coordinator_status`. `Coordinator::output_instances()` and
`registry()` expose the mapping. See `telemetry.md` / `wiring.md` for the sink and naming.

---

## 6. Lifecycle

Order: **init all (behind a barrier) → run → shutdown all**, honoring the system contract
(`init`/`shutdown` exactly-once, `execute`/`run` in between).

1. **Init (barrier).** `start()` spawns every async task first; each async system `init`s itself
   inside its task, then signals readiness (`ready_count`) and parks at a go-gate. The coordinator
   waits for **all** async inits to complete, then runs the cyclic `init`s on the loop's task, then
   sets the go-flag and wakes every async task into its run loop. This guarantees every system's
   `init` finishes before any `execute` or `run` pass — the property the init-barrier test asserts.
2. **Run.** The cyclic cycle loop (§3) plus the spawned async tasks, all on the stellarator
   executor.
3. **Shutdown.** `shutdown(tasks)`:
   - **Signals each async task** by setting its `stop` flag and notifying its input data
     `Notifier`s so a task can re-poll. (A task parked in `Input::recv` with no pending datum only
     re-checks on the next commit; the bounded window below covers timer- and data-paced tasks.)
   - **Waits a bounded `JOIN_TIMEOUT`** (20 ms) for tasks to observe `stop`, finish their current
     pass, and run their own `System::shutdown` (which must run on the task that owns the bundle),
     then **drops the tasks** — whose `drop_guard` cancels any still parked (the non-cooperative
     path).
   - **Shuts down the cyclic systems** (`CyclicSlot::shutdown`) in **reverse** registration order.
   - The `RingTable` drops last (struct field order), so every buffer outlives every port and every
     dlopen'd slot.

Shutdown is cooperative (flag + wake + bounded join), so a well-behaved async system finishes its
current `run` pass and flushes final frames in its own `shutdown` rather than being cut mid-write.
The hard timeout is the only non-cooperative path.

---

## 7. Reused vs. coordinator-specific

| Concern | Reused | Coordinator-specific |
|---|---|---|
| Per-system owner | `CyclicRunner<S,O>` (`system/input/output`, timed `step`, `end_cycle`) | the `CyclicSlot` trait object + `SlotState`/`StopReason`; `DlSlot` |
| Ports / data path | `Output::new`, `Input::new`, `latest`/`drain`/`recv` | the `BindPorts`/`Binder`/`BoundPort`/`RingSource` contract |
| Transport | `RingBuffer::{create_in_memory,writer,view,region}`, `Config` | `RingTable` ownership, one-writer-per-buffer build invariant |
| Sizing | `capacity_for`, `buffer_capacity::<F>`, `DEFAULT_DEPTH` | `max_readers` from fan-out + registry consumers + slack |
| Self-description | `SystemDescriptor`, `PortDesc`, `descriptor()` | the builder; `connect`/`connect_delayed` addressing by `(system, frame_id)` |
| Validation | `compatible(producer, consumer)` | per-edge driving; single-connect / unconnected-input / unbroken-cycle (`find_cycle`) |
| Wake | `NoWake` (cyclic), `Notifier` (async), `WaitQueue` | matched wake endpoints on the copy-in private buffer |
| Health | `SystemHealth`/`SystemLog`, `HealthPort` | auto-provisioned buffers; instance prefix at the sink; coordinator health + `CoordinatorStatus` |
| Async | `AsyncSystem::run`, `Input::recv`, `Output::write_async` | spawn-once task + in-task `init`/`shutdown`; private copy-in buffers + newest-record mirror |
| Telemetry | — | the `OutputRegistry`, registry-consumer sizing (the downlink is an ordinary `add_cyclic` system) |
| dlopen | `DlSystem`/`DlSlot`/`FswRing` ABI (`dl.rs`/`abi.rs`) | `add_dl_cyclic`, raw-region bind, teardown ordering |
| Runtime | `stellarator::{run,spawn,sleep,yield_now,JoinHandle::drop_guard}` | the run-fast-then-wait `run_for` loop; cooperative teardown |

---

## 8. Not yet implemented

- **Rate-derived buffer depth.** Depth is fixed per delivery kind (`config.default_depth` /
  `LOG_DEPTH`). Deriving depth from the producer/consumer rate ratio (so a slow reader never
  backpressures the producer within one of its periods) is a refinement (review finding C1:
  the earlier advisory `PortDesc::rate_hint` had no consumer and was deleted).
- **Per-system rate division.** One global `cycle_rate` drives every cyclic system; a system that
  wants a slower effective rate divides cycles itself.
- **Intra-cycle parallelism.** The cyclic loop is single-threaded. Running an acyclic layer in
  parallel would interact with the sampling rule and is not done.
- **Stopped-system recovery.** A hard-stopped slot is never restarted; there is no operator restart
  hook.

---

## 9. Tests

`src/coordinator/tests.rs` registers and wires systems through the builder (no hand-built ports) and
covers:

- **`two_system_end_to_end`** — a cyclic producer → cyclic consumer graph; the consumer (registered
  after the producer) samples this cycle's fresh value `1.0..=5.0`, confirming the registration-order
  forward-edge rule.
- **`idle_consumer_backpressures_producer`** — a consumer that never drains fills the ring;
  neither system stops (`stopped()` stays empty — backpressure never stops a system), and the
  producer's rejected writes are counted drops.
- **`async_through_copy_in`** — an async consumer fed through a private copy-in buffer; a bursting
  producer outruns the per-cycle mirror and the run completes without blocking (latest-wins), and
  the consumer still receives real samples via `recv`.
- **`validation_*`** — `FrameIdMismatch` (at `connect`), `UnknownPort`, `UnconnectedInput`,
  `DoubleConnect` (at `build`).
- **`init_barrier_holds`** — both systems' `init` complete before the first `execute` observes the
  init counter.
- **`feedback_cycle_unbroken_is_rejected`** / **`delayed_edge_allows_feedback_loop`** — an unbroken
  2-system cycle fails with `FeedbackCycle`; breaking the back-edge with `connect_delayed` builds and
  runs without hard-stopping.
- **`simulated_clock_is_deterministic_and_monotonic`** — under a `Simulated` clock each cycle's `now`
  is `start + k*dt`, strictly rising, with no wall-clock pacing.
</content>
</invoke>
