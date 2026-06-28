# Work-Package 5 — The Coordinator

Status: **design only, pre-implementation**. Reviewer sign-off required before any code lands.
No Rust in this WP. This document specifies the **coordinator**: the single piece of software
that owns the ring regions, wires the system graph, drives cyclic systems once per cycle, spawns
async systems, and provisions health. It builds directly on the **landed** WP1 ring
(`ring/src/lib.rs`) and WP4 system contract (`src/system.rs`, `src/port.rs`, `src/health.rs`,
`src/descriptor.rs`). WP6 (the KDL config language) layers on top of the builder defined here.

Relevant landed code (read before implementing):
- `src/system.rs` — `System`/`CyclicSystem`/`AsyncSystem`, `SystemInput`/`SystemOutput`,
  `Out<O,B,WD,WS>` (the framework wrapper that adds `health()`), `CyclicRunner<S,O>` (the WP4
  stand-in coordinator — **the seam this WP grows**), `descriptor()`.
- `src/port.rs` — `Output<F,B,WD,WS>::new`, `Input<F,B,RD,RS>::new`, `capacity_for`,
  `buffer_capacity::<F>`, `DEFAULT_DEPTH`, `Input::{is_lapped,resync,latest,drain,recv}`.
- `src/health.rs` — `SystemHealth`/`SystemLog`, `HealthPort::{new,record_lapped,end_cycle,error,log}`.
- `src/descriptor.rs` — `PortDesc`, `SystemDescriptor`, `SystemKind`, `compatible(producer, consumer)`.
- `ring/src/lib.rs` — `RingBuffer::{create_in_memory,writer,view}`, `Config { capacity, max_readers,
  overrun }`, `Overrun::{Overwrite,Lossless}`, `frame_len` (now `pub`), `NoWake`, `Notifier`,
  `BoxBacking`, `View::{is_lapped,resync,try_read_into,read_into}`, `FullReaderTable`.
- `stellarator` — `run(|| async { … })` (executor entry), `spawn(fut) -> JoinHandle`,
  `JoinHandle::drop_guard`, `sleep(Duration)`, `sync::WaitQueue` (behind `Notifier`).

---

## 0. Design summary (orientation)

The coordinator is built in two phases:

1. **Build phase (no cycles running).** A `Coordinator::builder()` registers systems
   (`add_cyclic`/`add_async`) and edges (`connect`). Registration only records each system's
   `SystemDescriptor` (WP4 already derives it). `build()` then runs the **validation pass**
   (`compatible()` on every edge, single-writer + fan-out checks), allocates one
   `RingBuffer<BoxBacking>` per output port sized from the descriptors and fanned-out from the
   edge set, binds each system's typed `Output<F>`/`Input<F>` ports over those rings (resolving
   WP4's deferred `SystemOutput::bind`), auto-provisions a health/log buffer pair per system, and
   returns a ready `Coordinator`.

2. **Run phase.** `Coordinator::run()` executes on `stellarator`: it `init`s every system, spawns
   each async system's `run` loop once, then enters the cyclic cycle loop (run-fast-then-wait at
   the configured rate), folding the async copy-in step into the loop. On teardown it signals and
   joins the async tasks and `shutdown`s every system.

Everything below the builder is reuse: the data path is the landed ring, the port wrappers, the
descriptors, the health port, and `CyclicRunner`. The genuinely new surface is the builder, the
graph/validation/sizing logic, the `Binder`/`bind` contract, the grown per-system slot (lapped →
permanent stop), and the async copy-in plumbing.

---

## 1. Coordinator types & ownership

### 1.1 Top-level types

```rust
pub struct CoordinatorConfig {
    /// Cycle rate the loop holds (run-fast-then-wait). DESIGN.md "Each cycle".
    pub cycle_rate: Hz,            // e.g. 100.0 → a 10 ms budget
    /// Default in-flight record depth for a buffer whose PortDesc carries no rate_hint.
    pub default_depth: usize,      // defaults to ring::DEFAULT_DEPTH (= 8)
}

pub struct Coordinator {
    config:    CoordinatorConfig,
    rings:     RingTable,          // owns every RingBuffer<BoxBacking> (§1.2)
    cyclic:    Vec<Box<dyn CyclicSlot>>,   // type-erased grown runners, in run order (§3)
    asyncs:    Vec<AsyncTask>,     // spawned-once async loops + shutdown signal (§4)
    copy_ins:  Vec<CopyIn>,        // private-buffer copy-in jobs for async inputs (§4)
}
```

**The coordinator owns the rings; systems borrow into them.** This is the ownership rule
DESIGN.md states ("Systems own their outputs and borrow their inputs") realized concretely: a
`RingBuffer<BoxBacking>` is `Arc`-backed and cheaply clonable (`ring/src/lib.rs` `impl Clone for
RingBuffer`), so the coordinator holds the canonical handle in `RingTable` and the systems' ports
hold `Writer`/`View` clones derived from it. Keeping the canonical handle alive in `RingTable`
guarantees a buffer outlives every port over it, regardless of system teardown order.

### 1.2 `RingTable` — the ring registry

```rust
struct RingTable {
    rings: Vec<RingEntry>,         // one per allocated buffer
}
struct RingEntry {
    id:        BufferId,           // dense index, assigned at build time
    ring:      RingBuffer<BoxBacking>,
    frame_id:  ComponentId,        // the frame the buffer carries (for diagnostics)
    role:      BufferRole,         // Output { system, port } | Private { async_system, port } | Health | Log
    // Pre-created, matched wake endpoints shared by writer- and reader-side ports (§2.3).
    wake:      WakeEndpoints,
}
```

There is exactly **one writer per `RingEntry`** (§2.4); the table is the place that invariant is
enforced structurally, because a buffer is created with a single owning producer and the builder
never hands out a second `Writer`.

### 1.3 Per-system slot ownership

A **cyclic** system, its `Input` bundle, and its `Out<Output>` bundle are owned together by the
coordinator across cycles — exactly what `CyclicRunner<S,O>` already does (`src/system.rs`:
`CyclicRunner { system, input, output }`). WP5 keeps `CyclicRunner` as the per-system owner and
**grows it** (§3.4) into a `CyclicSlot` trait object so the coordinator can hold a heterogeneous
`Vec<Box<dyn CyclicSlot>>`.

An **async** system is different: its `run` future borrows `&mut self`, `&mut Input`, `&mut
Output` for the whole loop, so those three move *into* the spawned task and are owned by it, not by
the coordinator (§4.1). The coordinator keeps only a handle (`AsyncTask`) to drive lifecycle.

### 1.4 Resolving `SystemOutput::bind` — the `Binder` / `bind` contract

WP4 deferred binding: the landed `SystemOutput`/`SystemInput` derives generate only
`descriptors()`/`any_lapped()` (see `libs/metor-fsw/macros/src/system.rs`, which emits
`<#ty>::descriptor()` per field). WP5 adds the **parallel** construction path. The key invariant
is that **`bind()` walks the port fields in the same order as `descriptors()`**, so a positional
cursor lines each port up with the ring the builder pre-allocated for it.

Per-port `bind` (added to the landed `Output<F>`/`Input<F>`):

```rust
impl<F: Frame, WD: WakeSource, WS: WakeSink> Output<F, BoxBacking, WD, WS> {
    /// Bind this output over the next ring in the binder, taking the matched
    /// writer-side wake endpoints the builder pre-created for that buffer.
    fn bind(b: &mut Binder) -> Self {
        let (ring, data, space) = b.next_output::<WD, WS>();   // typed pop, descriptors() order
        Output::new(ring.writer(data, space))                  // landed Output::new + RingBuffer::writer
    }
}
impl<F: Frame, RD: WakeSink, RS: WakeSource> Input<F, BoxBacking, RD, RS> {
    fn bind(b: &mut Binder) -> Self {
        let (ring, data, space) = b.next_input::<RD, RS>();
        Input::new(ring.view(data, space).expect("reader slot reserved at sizing time"))
    }
}
```

`Binder` is a **concrete** cursor (not `dyn` — it needs generic methods), built by the coordinator
after sizing, holding the ordered rings and the matched wake endpoints for one system's bundle:

```rust
struct Binder<'a> {
    out_cursor: slice::Iter<'a, BoundOutput>,   // one per output PortDesc, in order
    in_cursor:  slice::Iter<'a, BoundInput>,    // one per input PortDesc, in order
}
impl Binder<'_> {
    fn next_output<WD: WakeSource, WS: WakeSink>(&mut self) -> (RingBuffer<BoxBacking>, WD, WS);
    fn next_input<RD: WakeSink, RS: WakeSource>(&mut self) -> (RingBuffer<BoxBacking>, RD, RS);
}
```

The derives gain a generated `bind(&mut Binder) -> Self` that calls `<FieldType>::bind(binder)`
per field — symmetric to the existing `descriptors()` generation and equally free of `F`-parsing.
`Out<O>` (the framework wrapper) binds its inner `O` then constructs its `HealthPort` from the two
auto-provisioned health/log rings (§5), mirroring `Out::descriptors()` pushing the health/log
descriptors after the user ports.

**Matched wake endpoints (the load-bearing subtlety).** A `Notifier` is `Arc`-backed
(`ring/src/lib.rs`): the writer side and the view side must hold the **same clone** for a commit to
wake an awaiting reader (the WP4 test creates `imu_data` once and `.clone()`s it into both
`writer(...)` and `view(...)`). So `bind` must **not** default-construct wakes independently. The
`Binder` carries the pre-created, per-buffer `WakeEndpoints` from `RingTable` and hands the matched
clone to whichever side it is binding. Because a buffer's wake type is fixed by the graph (cyclic
buffers → `NoWake`; async-fed buffers → `Notifier`), the endpoints are stored type-erased
(`Box<dyn Any>`) and downcast in `next_output`/`next_input` against the concrete `WD/WS` the port
type supplies. This works but is fiddly — see **Q3**.

---

## 2. Graph builder, compatibility validation, sizing

### 2.1 The builder API (WP6's target)

```rust
impl Coordinator {
    pub fn builder(config: CoordinatorConfig) -> CoordinatorBuilder;
}

impl CoordinatorBuilder {
    /// Register a cyclic system. Returns a handle whose ports can be named in `connect`.
    pub fn add_cyclic<S>(&mut self, system: S) -> SystemHandle
        where S: CyclicSystem<Output = Out<…>> + 'static;

    /// Register an async system.
    pub fn add_async<S>(&mut self, system: S) -> SystemHandle
        where S: AsyncSystem + 'static;

    /// Connect "producer system's output frame X" → "consumer system's input frame X".
    /// Ports are addressed by (SystemHandle, frame_id) — both come straight off the
    /// already-derived SystemDescriptor, so WP6 can resolve a KDL edge to this call.
    pub fn connect(&mut self, producer: PortRef, consumer: PortRef) -> Result<(), WireError>;

    /// Validate, size, allocate, bind, provision health, return a ready coordinator.
    pub fn build(self) -> Result<Coordinator, WireError>;
}

pub struct PortRef { pub system: SystemHandle, pub frame_id: ComponentId }
```

`add_*` stores the boxed system plus `S::descriptor()` (the landed `CyclicSystem::descriptor()` /
`AsyncSystem::descriptor()`, which already enumerate `inputs`/`outputs` as `Vec<PortDesc>`). The
builder never touches `F` directly — it works entirely off `SystemDescriptor`/`PortDesc`, exactly
the WP4 "read surface."

Type erasure: `add_cyclic` immediately wraps the system in its `CyclicRunner`/`CyclicSlot`
(deferred until ports exist) and stores a thunk; `add_async` stores a boxed launcher. The builder
keeps the registration order, which becomes the v1 run order (§3.1).

WP6 is a pure front-end: a KDL document names systems and edges, the loader instantiates each
system (params from its config block, per WP4 §1.2 "constructible before init"), calls `add_*`,
resolves edges to `PortRef`s by frame name, and calls `connect`. No coordinator logic lives in WP6.

### 2.2 Compatibility validation (build-time, before any cycle)

For every `connect(producer, consumer)` edge, `build()` looks up the producer's matching
`OutputDesc` and the consumer's matching `InputDesc` (both already on the descriptors) and calls
the landed `compatible(producer_desc, consumer_desc)` (`src/descriptor.rs`):

- same `frame_id`, and
- the consumer's realized `(component_id, ty, shape)` set is a **subset** of the producer's
  (forward-compatible: a producer may emit extra fields a consumer ignores).

`compatible` already does the `VTable::realize_fields(None)` registration-mode enumeration and the
subset/ty/shape comparison. WP5 only *drives* it per edge and turns a `false` into a `WireError`
naming both ports. This catches every wiring mistake before a byte flows, as DESIGN.md requires
("the coordinator validating compatibility against the systems' VTables").

Additional structural checks in the same pass:

- **Every input port is connected exactly once.** A cyclic/async input with no `connect` is a
  build error (nothing would ever write it). Two `connect`s into one input is a build error
  (an input port is a single `View` into a single producer — WP4 §2.4 "N producers → N views",
  combining is the consumer's job via *separate* ports).
- **Single writer per buffer (§2.4).**
- **Frame-id match on the edge** — `connect` rejects an edge whose producer port and consumer port
  do not even share a `frame_id` early (a friendlier error than the subset failure).

### 2.3 Buffer sizing & `max_readers` derivation

One buffer per **output** port (plus the auto health/log buffers, §5; plus one private buffer per
**async input**, §4). Each is sized from its `PortDesc` and the edge set:

```
depth      = ceil(rate_hint ? f(rate_hint, cycle_rate) : config.default_depth)   // ≥ 2
capacity   = capacity_for(port.max_size, depth)        // landed: frame_len(max_size)*depth, pow2
max_readers = fan_out(port)                             // count of registered consumers
overrun    = Overrun::Overwrite                         // v1 default (DESIGN.md "writer-chosen")
```

- `capacity_for` / `buffer_capacity::<F>` are the landed sizing helpers (`src/port.rs`); they wrap
  the now-`pub` `frame_len` (the WP4 Q1 gap is closed — `ring/src/lib.rs` exposes `frame_len`).
- **`max_readers` = fan-out**, computed from the graph: for an output port, the number of distinct
  consumers = (cyclic consumers, each a direct `View`) + (async consumers, each **one** copy-in
  `View`, §4) + (health/db sink readers for health/log buffers, §5). `Config.max_readers` must
  cover every `view()` the builder will register, because the ring has no crash-slot reclamation
  in v1 (`ring/src/lib.rs` `Config` doc). Over-provision by a small constant for late attach
  (e.g. a debugger/db tap) — a tunable.
- **`depth` from `rate_hint`.** When producer and consumer carry `rate_hint`s (advisory, on
  `PortDesc`), depth covers the rate ratio so a slower reader does not lap within one of its
  periods; absent a hint, `default_depth`. v1 may simply use `default_depth` everywhere and treat
  rate-derived depth as a refinement (**Q10 carried from WP4**).

### 2.4 Single-writer-per-buffer enforcement

The ring documents "at most one live writer per buffer" (`ring/src/lib.rs` `Writer` doc) but does
not enforce it — the builder does, structurally:

- A buffer is created **for exactly one output port** and the only `Writer` is handed to that
  port's `Output<F>` during `bind`. The builder calls `RingBuffer::writer(...)` **once** per
  buffer. A private async buffer's single writer is the coordinator's copy-in job (§4).
- `connect` only ever adds **`view()`** (reader) registrations to an existing producer buffer; it
  never creates a second writer. There is no API by which two systems can write one buffer.

This makes "single writer" an invariant of the build graph, not a runtime check.

---

## 3. The cycle loop (cyclic systems)

### 3.1 Ordering & the sampling rule (feedback loops)

v1 runs cyclic systems in a **fixed per-cycle order** (DESIGN.md defers deterministic ordering and
replay; the loop still needs a defined order). The order is:

> **Topological order over the acyclic part of the graph, falling back to registration order.**
> The builder topologically sorts cyclic systems by their `connect` edges; cycles (feedback loops)
> are broken at a designated **back-edge**, and within an SCC the registration order is used.

The **sampling rule** is a direct consequence of shared overwrite rings + ordered execution, and
needs no extra machinery:

- A cyclic system reads each input with `Input::latest()` (`src/port.rs`), which drains its `View`
  to the **newest committed record at the instant it runs**.
- **Forward edge** (producer runs *before* consumer this cycle): the consumer sees **this cycle's
  fresh** output. Topological order maximizes these.
- **Back edge / feedback** (producer runs *after* consumer): the consumer sees the producer's
  **previous-cycle** output — a natural **one-cycle delay** on feedback edges. No system ever
  blocks waiting for a not-yet-produced input; it just reads the latest available.

So the stated rule is: *"a cyclic system sees the latest committed value of each input at the
moment it executes; topological ordering makes acyclic edges same-cycle, feedback edges incur a
one-cycle delay."* This is deterministic given the fixed order and is the v1 contract (full
determinism/replay deferred per DESIGN.md). **Q1** asks the reviewer to confirm topological+back-
edge-delay versus plain registration-order-with-delay.

### 3.2 Run-fast-then-wait timing

Per DESIGN.md "Each cycle": run every system as fast as possible, then hold the rate by waiting at
the end. One cycle:

```rust
let budget = Duration::from_secs_f64(1.0 / config.cycle_rate);
loop {
    let start = Instant::now();
    let now = Timestamp::now();
    for slot in &mut self.cyclic { slot.step(now); }   // §3.3, in fixed order
    self.run_copy_ins();                               // fold async copy-in here (§4.2)
    let elapsed = start.elapsed();
    if elapsed < budget {
        stellarator::sleep(budget - elapsed).await;    // hold the rate
    } else {
        self.telemeter_overrun(elapsed, budget);       // §3.2 overrun: no sleep, next cycle now
    }
    if self.stopping() { break; }                      // §6 teardown
}
```

- **Overrun** (a cycle exceeds its budget): do **not** sleep; telemeter the overrun (a coordinator-
  level health frame, §5.3) and start the next cycle immediately. The loop never tries to "catch
  up" by skipping work — it just runs continuously when saturated.
- Timing uses `stellarator::sleep` (the loop is a stellarator task), so async system tasks and
  copy-in share the executor during the wait.

### 3.3 Lapped input → permanent hard-stop

Before invoking each cyclic system, check `any_lapped()` (`SystemInput::any_lapped`, landed; ORs
every input port's `View::is_lapped`). The landed `CyclicRunner::step` already *charges* a lapped
input (`record_lapped`) but **still executes** — WP5 must change this to the DESIGN.md hard-stop:
**telemeter and permanently stop invoking that system.**

A "stopped" system is represented by a **per-slot `stopped: bool`** in the grown slot (§3.4). Once
set it is never cleared in v1 (recovery is future work). Surfacing:

- The slot publishes a final health record via `HealthPort::end_cycle` (so `lapped_inputs` is
  nonzero and `cycles` stops advancing — observable in metor-db), plus a one-shot `Error` **log
  line** via `HealthPort::log(Level::Error, …)` naming the lapped input.
- Subsequent cycles skip the slot entirely (no `execute`, no health publish), so its outputs go
  stale; **downstream cyclic consumers of a stopped system will themselves eventually lap** and
  stop in turn — the failure propagates by the same mechanism, which is the intended fail-stop
  behavior, not a special case.
- The coordinator exposes `stopped` slots in a status query / coordinator health frame so an
  operator sees which modules fell out (**Q4**: is a bool enough, or do we want an explicit
  `SlotState { Running, Stopped { reason } }` and a dedicated coordinator status frame?).

### 3.4 Growing `CyclicRunner` into `CyclicSlot`

`CyclicRunner<S,O>` (`src/system.rs`) is the WP4 stand-in and the explicit seam. WP5 generalizes
it minimally and erases it behind a trait so the coordinator holds a heterogeneous set:

```rust
trait CyclicSlot {
    fn init(&mut self);
    fn step(&mut self, now: Timestamp);   // grown: any_lapped → stop; else time execute + end_cycle
    fn shutdown(&mut self);
    fn name(&self) -> &'static str;       // System::NAME
    fn stopped(&self) -> bool;
}
```

`CyclicRunner` already owns `system/input/output`, times `execute` with `Instant::now`, and calls
`output.health().end_cycle(Timestamp::now(), micros)` — WP5 keeps all of that and only changes
`step` to: if `stopped` return; if `input.any_lapped()` → `record_lapped()`, publish a final
health record, log the error, set `stopped`, return; else time `execute` and `end_cycle` as today.
`now` is threaded from the loop so every system in a cycle shares one timestamp. `init`/`shutdown`
delegate to the landed methods unchanged.

### 3.5 Single-threaded vs parallel within a cycle

v1 runs the cyclic loop **single-threaded** on one stellarator task: simplest, and it makes the
sampling rule (§3.1) exact (no intra-cycle races on read order). Systems are independent enough
that a future WP could run an acyclic *layer* in parallel, but that interacts with the sampling
rule and is out of scope (**Q5**).

---

## 4. Async systems

### 4.1 Spawn once

For each async system the coordinator spawns its `run` loop exactly once (DESIGN.md: "the
coordinator does not invoke them"). Because `AsyncSystem::run(&mut self, &mut Input, &mut Output)`
borrows all three for the loop's lifetime, the system, its `Input` bundle, and its `Out<Output>`
move **into** the spawned task:

```rust
let stop = StopSignal::new();             // shared flag the loop polls / awaits
let handle = stellarator::spawn(async move {
    system.init(&mut output);             // §6: init inside the task, before run
    loop {
        if stop.is_set() { break; }
        system.run(&mut input, &mut output).await;   // one pass; system paces itself
    }
    system.shutdown(&mut output);         // §6: shutdown inside the task, after the loop
});
self.asyncs.push(AsyncTask { handle: handle.drop_guard(), stop, name });
```

The system's own `run` decides pacing — await inputs via `Input::recv` (backed by a `Notifier`
sink) or `stellarator::sleep` on a timer — exactly as the WP4 `AsyncFilter` test does. The
coordinator only spawns and later signals teardown (§6). `JoinHandle::drop_guard` ensures a
dropped coordinator cancels the task.

### 4.2 Private copy-in buffers (the input side)

An async input does **not** view the upstream output directly; it views a **private buffer the
coordinator owns** (DESIGN.md, WP4 §3.2). For each async input port the builder allocates a private
`RingBuffer<BoxBacking>` in `Overrun::Overwrite` mode, sized like any buffer (§2.3) with
`max_readers = 1` (only the async system's `View`). Two ports straddle it:

- **Writer side — the copy-in job**, owned by the coordinator (single writer, §2.4). It reads the
  upstream producer's output `View` and writes records into the private buffer.
- **Reader side — the async system's `Input` port**, bound with a `Notifier` data sink (`RD =
  Notifier`) so `Input::recv` wakes when copy-in commits.

**Drop on full.** The private buffer is `Overwrite`, so the copy-in `Writer::try_write` **never
blocks and silently overwrites** unconsumed records when the async consumer is behind — this *is*
DESIGN.md's "if there is no room, the data is dropped," with no extra logic. (Equivalently, the
copy-in could resync on lap; overwrite already gives drop-on-full for free.)

### 4.3 Where copy-in runs

v1 **folds copy-in into the cycle loop** (`run_copy_ins()` after the cyclic systems each cycle,
§3.2):

```rust
fn run_copy_ins(&mut self) {
    for c in &mut self.copy_ins {
        // Drain fresh upstream records, mirror into the private buffer (drop-on-full),
        // notify the async system's view.
        while let Ok(true) = c.upstream.try_read_into(&mut c.scratch) {
            let _ = c.private_writer.try_write(&c.scratch);   // overwrite = drop on full
        }
    }
}
```

Rationale: keeps the system **single-threaded** and avoids requiring a `Notifier` on every *cyclic*
producer's output (a cyclic producer writes with `NoWake`; a separate awaiting copy-in task would
need the producer to notify). The cost is that async input latency is bounded by the cycle rate —
acceptable for v1. The copy-in `try_write` notifies the private buffer's `Notifier` (its data
side), which wakes the async `run`'s `recv`.

**Alternative (noted, not chosen): a copy-in task per async input** that awaits the upstream `View`
directly. More responsive, but requires the upstream producer's output ring to carry a `Notifier`
data wake (so a commit wakes the copy-in), which complicates buffer wake-typing when the producer
is cyclic. Deferred (**Q2**).

### 4.4 Async outputs feeding cyclic consumers

There is nothing async-specific on the **output** side (WP4 §3.2): an async system writes its
output ring at its own pace; a cyclic consumer holds an ordinary `View` and reads it with
`Input::latest()` each cycle. Coherence: the cyclic consumer sees whatever the async system had
committed by the moment the consumer runs — latest-wins, same as any edge. If the async producer is
much *faster* than the cycle and the cyclic consumer is slow, the overwrite buffer can lap the
consumer → the §3.3 hard-stop applies, sized against by `depth` (§2.3). No special handling.

---

## 5. Health provisioning & counter driving

### 5.1 Auto-provisioned health/log buffers

Every system implicitly produces a `SystemHealth` and a `SystemLog` frame (WP4: `Out::descriptors`
pushes `PortDesc::of::<SystemHealth>()` and `PortDesc::of::<SystemLog>()` after the user ports).
The coordinator **auto-provisions** the two buffers per system at build time — they are not wired by
the user. Sizing is identical to any output (§2.3); `max_readers` = number of telemetry consumers
(at least one db/sink reader, §5.4). During `bind`, `Out::bind` constructs the system's
`HealthPort` from these two rings (`HealthPort::new(Output::new(health_writer),
Output::new(log_writer))`).

### 5.2 Driving the standard counters

The framework already drives the four standard counters around `execute`: `CyclicRunner::step`
times `execute` with `Instant` and calls `HealthPort::end_cycle(Timestamp::now(), micros)`, which
bumps `cycles`, stamps `last_execute_micros`, and publishes one `SystemHealth` record plus any
pending log lines (`src/health.rs`). A lapped input routes through `HealthPort::record_lapped`
(bumps `lapped_inputs`). WP5 keeps this verbatim in the grown slot (§3.4) and adds only the
hard-stop transition (one extra `record_lapped` + final publish + error log before stopping).

For async systems the same `HealthPort` is in their `Out<Output>` (moved into the task); the
counters around their work are driven from within `run`/the task wrapper (the async loop calls
`end_cycle` per pass), since the coordinator does not tick them.

### 5.3 Coordinator-level health

The coordinator emits its own `SystemHealth`/`SystemLog` (NAME = `"coordinator"`) for cycle-level
events that belong to no single system: **cycle overruns** (§3.2), **systems that hard-stopped**
(§3.3), and async task exits. This reuses the exact health frames and port — no new telemetry type.

### 5.4 Where health frames go & the `<NAME>` prefix

Health is "just frames" (DESIGN.md): the health/log buffers are ordinary output rings, so any
consumer — metor-db, a UI, a downstream system — reads them with a `View` like any other output.
v1 provisions at least one reader slot for a **db/telemetry sink** that drains every system's
health/log buffer into metor-db (the same ingest path as all component data).

WP4 deferred the `<NAME>.health` / `<NAME>.log` **prefixing**: the frames are derived with fixed
names `"health"`/`"log"` (`src/health.rs` `#[metor_fsw(name = "health")]`), but each system's
records must land namespaced under its `System::NAME` so two systems' health do not collide in db
(`filter.health.cycles` vs `nav.health.cycles`). The coordinator owns the prefix because `NAME` is
per-system. The prefix is **not** a compile-time frame attribute (it would have to be on every
system's frame); instead the coordinator records, per health/log buffer, the owning system's
`NAME`, and the telemetry sink applies the `<NAME>.` path prefix when ingesting that buffer's
records into db (the frame's `VTable` paths get the system prefix at ingest, mirroring how the
cube-sat example namespaces a table with `#[metor_fsw(parent = "cube_sat")]`, but applied at
runtime per instance). **Q6** asks the reviewer to confirm the prefix is applied at the sink rather
than baked into the on-ring frame.

---

## 6. Lifecycle

Order: **init all → run → shutdown all**, honoring the WP4 contract (`init`/`shutdown`
exactly-once, `execute`/`run` in between).

1. **Init.** For cyclic systems, `CyclicSlot::init` (→ `CyclicRunner::init` → `System::init(&mut
   output)`) in registration order, on the cyclic loop's thread before the first cycle. For async
   systems, `init` runs **inside the spawned task** before its first `run` pass (§4.1), so the
   borrow rules hold and an async system can publish an initial frame. The coordinator spawns the
   async tasks, then enters the cycle loop.
2. **Run.** The cyclic cycle loop (§3) plus the spawned async tasks, all on the stellarator
   executor (`stellarator::run(|| async { coordinator.run().await })`).
3. **Shutdown.** On teardown the coordinator:
   - **Stops the cyclic loop** (sets `stopping`, breaks after the current cycle — never mid-system,
     so no `execute` is interrupted).
   - **Signals each async task** via its `StopSignal` and **notifies** any input `Notifier` so a
     task parked in `Input::recv` wakes, observes the flag, exits its loop, and runs
     `System::shutdown(&mut output)` *itself* (shutdown must run on the task that owns the bundle).
     The coordinator then **awaits each `JoinHandle`** to confirm the task drained. A bounded join
     timeout guards a misbehaving task (it is then dropped via the `drop_guard`, cancelling it
     without `shutdown` — telemetered).
   - **Shuts down cyclic systems** (`CyclicSlot::shutdown`) in reverse registration order.
   - Drops `RingTable` last, so every buffer outlives every port.

In-flight async work: because shutdown is cooperative (flag + wake + join), an async system
finishes its current `run` pass and flushes final frames in its own `shutdown`, rather than being
cut mid-write. The hard timeout is the only non-cooperative path.

---

## 7. Reused vs. new

| Concern | Reused (landed) | New in WP5 |
|---|---|---|
| Per-system owner | `CyclicRunner<S,O>` (`system/input/output`, timed `step`, `end_cycle`) | grow into `CyclicSlot` trait object + permanent `stopped` flag |
| Ports / data path | `Output<F>::new`, `Input<F>::new`, `latest`/`drain`/`recv`/`resync`/`is_lapped` | `bind()` per port + the `Binder` cursor (resolves `SystemOutput::bind`) |
| Transport | `RingBuffer::{create_in_memory,writer,view}`, `Config`, `Overrun`, `frame_len` (now `pub`) | `RingTable` ownership, one-writer-per-buffer build invariant |
| Sizing | `capacity_for`, `buffer_capacity::<F>`, `DEFAULT_DEPTH` | `depth`/`max_readers` derivation from the edge/fan-out graph |
| Self-description | `SystemDescriptor`, `PortDesc`, `descriptor()`, `SystemInput::any_lapped` | builder consuming descriptors; `connect` addressing by `(system, frame_id)` |
| Validation | `compatible(producer, consumer)` (frame_id + subset ty/shape) | per-edge driving; single-writer / single-connect / unconnected-input checks |
| Wake | `NoWake` (cyclic), `Notifier` (async), `WaitQueue` | matched wake-endpoint provisioning per buffer; copy-in `Notifier` plumbing |
| Health | `SystemHealth`/`SystemLog`, `HealthPort::{new,record_lapped,end_cycle,error,log}` | auto-provisioned buffers; `<NAME>` prefix at the sink; coordinator-level health |
| Async | `AsyncSystem::run`, `Input::recv`, `Output::write_async` | spawn-once task + `init`/`shutdown` in-task; private copy-in buffers + drop-on-full |
| Runtime | `stellarator::{run,spawn,sleep,JoinHandle::drop_guard}` | the run-fast-then-wait cycle loop; cooperative teardown |

Genuinely new code: the builder + validation/sizing pass, the `Binder`/`bind` contract, the grown
`CyclicSlot` (lapped → stop), the copy-in jobs, and the lifecycle driver. The transport, ports,
descriptors, health, and `CyclicRunner` are reuse.

---

## 8. Open questions / risks for the reviewer

1. **Q1 — cyclic ordering & sampling rule for feedback loops.** Proposed: topological order over
   the acyclic part, back-edges broken with a one-cycle delay; an input always reads the latest
   committed value at the moment its system runs (forward edges same-cycle, feedback edges
   one-cycle-delayed). Is topological-with-back-edge-delay the right v1 rule, or plain
   registration-order with the same latest-wins sampling (simpler, but acyclic edges may straddle
   cycles)? DESIGN.md defers full determinism, but the loop needs *a* defined order.
2. **Q2 — where copy-in runs.** Proposed: fold copy-in into the cycle loop (single-threaded, no
   `Notifier` needed on cyclic producers, async input latency bounded by the cycle rate). The
   alternative is a per-async-input task awaiting the upstream `View` (more responsive, but forces
   a `Notifier` data wake onto every producer feeding an async consumer). Confirm the folded
   approach for v1.
3. **Q3 — `Binder`/`bind` ergonomics & matched wakes.** The matched-`Notifier` requirement
   (writer and view must share the same `Arc`-backed clone) forces the `Binder` to carry
   per-buffer, type-erased wake endpoints and downcast them against each port's concrete `WD/WS`.
   Workable but fiddly. Acceptable, or do we want a different bind shape (e.g. the coordinator
   constructs `Writer`/`View` fully and hands them in, with a small typed registry per system)?
4. **Q4 — representing a stopped system.** Proposed: a per-slot `stopped: bool`, surfaced via the
   system's health (`lapped_inputs` > 0, `cycles` frozen) + a one-shot error log + a coordinator
   status frame. Is a bool enough, or do we want `SlotState { Running, Stopped { reason } }` with a
   dedicated coordinator status frame, and any operator-driven restart hook (recovery is otherwise
   future work)?
5. **Q5 — single-threaded vs intra-cycle parallelism.** v1 runs the cyclic loop single-threaded
   (exact sampling, simplest). Is that acceptable for v1, with parallel acyclic layers deferred, or
   is parallelism needed now (and thus the sampling rule must account for it)?
6. **Q6 — health `<NAME>` prefixing.** Proposed: apply the `<NAME>.health` / `<NAME>.log` path
   prefix at the **telemetry sink/db ingest**, per buffer, rather than baking it into the on-ring
   frame (whose name is the fixed `"health"`/`"log"`). Confirm the prefix lives at ingest, not in
   the frame bytes.
7. **Q7 — `max_readers` over-provisioning.** With no crash-slot reclamation in v1
   (`ring/src/lib.rs`), `max_readers` must be set to fan-out at build time, plus a slack constant
   for late taps (db/debugger). What slack, and is a late attach (a reader added after `build`)
   in-scope for v1 at all?
8. **Q8 — async lifecycle in-task.** `init`/`shutdown` for async systems run inside the spawned
   task (the only owner of the bundle), so an async `init` runs after cyclic `init`s, concurrently
   with the first cycles. Is that ordering acceptable, or must all `init`s complete before any
   `run`/cycle starts (requiring a barrier)?
9. **Q9 — cycle-rate config granularity.** One global `cycle_rate` for all cyclic systems in v1
   (per-system rate-division is future work). Is a single coordinator rate enough, or do some
   cyclic systems need to run every Nth cycle now?
