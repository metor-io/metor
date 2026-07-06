# The `System` abstraction

A **system** owns some private state, produces frames into a set of **output ports**
(single-writer ring buffers), and reads a set of **input ports** (read-only views into
other systems' output buffers). The coordinator owns the ring regions, builds each
system's port handles at wiring time, validates that producer outputs satisfy consumer
inputs (using the frames' VTables), then drives the system: a cyclic system is `execute`d
once per cycle; an async system runs its own loop. Systems never return errors — they emit
health as ordinary frames over a framework-provided health output port.

This document describes the system-side contract: what a system *is*, the typed port
wrappers around the ring transport, the self-description the coordinator reads, and the
health/log telemetry. The data path underneath the ports (table bytes == ring payload) is
the `metor-fsw-ring` / `FrameWriter` / `View` machinery; the system layer adds only frame
typing, the binding contract, the lifecycle, and health.

The code lives in:

- `src/system/mod.rs` — the `System` / `CyclicSystem` / `AsyncSystem` / `BuildSystem`
  traits, the `SystemInput` / `SystemOutput` bundle traits, the `Out<O>` health wrapper,
  and the `CyclicRunner` driver. Unit tests in `src/system/tests.rs`.
- `src/port.rs` — the typed `Output<F>` / `Input<F>` port wrappers and `FrameRef<F>` /
  `FrameGrant<F>`.
- `src/binder.rs` — the `RingSource` / `BindPorts` binding contract and the host `Binder`.
- `src/descriptor.rs` — `PortDesc`, `SystemDescriptor`, `SystemKind`, and `compatible`.
- `src/health.rs` — `SystemHealth` / `SystemLog` frames and the `HealthPort` handle.

---

## 1. The `System` family of traits

The system surface is split into a shared base and two leaf traits. The base carries
everything common — the typed port bundles, the wiring name, and the once-each lifecycle
hooks — and the two leaf traits express the one structural difference: who drives the
system. A user implements the base plus exactly one leaf trait. The lifecycle is never
duplicated.

```rust
pub trait System {
    /// The read-only inputs this system consumes.
    type Input: SystemInput + BindPorts;
    /// The owned outputs this system produces (wrapped in `Out` for health).
    type Output: SystemOutput + BindPorts;

    /// Wiring name; the prefix the system's health frame hangs off (§4).
    const NAME: &'static str;

    /// Runs once before the first `execute`/`run`. May emit initial frames / health.
    fn init(&mut self, _output: &mut Self::Output) {}

    /// Runs once at teardown. May flush final frames / health.
    fn shutdown(&mut self, _output: &mut Self::Output) {}
}
```

`init` and `shutdown` default to no-ops, so a system that needs no setup or teardown omits
them. Both receive `&mut Self::Output` — a driver publishes its first frame in `init` (e.g.
a default mode) and may flush a final health record in `shutdown`. They take no input:
there is nothing meaningful to read before or after the run.

### 1.1 Backing-erased rings — one system, two deployment shapes

Rings are backing-erased: `RingBuffer` is one concrete type whatever holds its region —
heap, mmap, or a non-owning attach (the ring's erased `Backing` struct, ring-buffer.md §10)
— so no backing type parameter threads through the traits. `impl System for Foo` is the
only spelling, and the **same impl** serves both deployment shapes: an in-process host
instance over the coordinator's heap rings, and a dlopen'd `cdylib` instance whose ports
hold non-owning attaches into the host's shared-memory regions. Because the bytes on the
wire are described by the VTable rather than by a shared Rust type, the same system source
(indeed the same monomorphization) serves both shapes; only who owns the region differs
(§6).

### 1.2 Cyclic systems

```rust
pub trait CyclicSystem: System {
    /// One unit of work: read the latest inputs, write outputs. Reports trouble
    /// through `output.health()`, never a return value.
    fn execute(&mut self, now: Timestamp, input: &mut Self::Input, output: &mut Self::Output);

    /// This system's self-description for wiring (§5).
    fn descriptor() -> SystemDescriptor { /* SystemKind::Cyclic */ }
}
```

The coordinator calls `execute` once per cycle. `now` is the coordinator's per-cycle
timestamp — the same value handed to every system in one cycle, and the value driving a
simulated clock — so a system stamps its output frames with it rather than calling
`Timestamp::now()` independently.

`execute` takes `&mut Self::Input` (not `&`): reading a ring `View` advances (or pins) its
cursor, so reading is a mutating operation even though the data it exposes is a zero-copy
borrow off the upstream ring. It takes
`&mut Self::Output` because the output ports wrap ring `Writer`s, which need `&mut self` to
write and which must persist across cycles — a `Writer` holds the single-writer role for a
buffer, so recreating it each cycle would be wrong. The coordinator owns the bundles
between cycles.

`descriptor()` is provided: it assembles a [`SystemDescriptor`] from `NAME`, the bundle
descriptors, and `SystemKind::Cyclic`.

### 1.3 Async systems

```rust
pub trait AsyncSystem: System {
    /// The system's own loop. Returns when shutting down.
    async fn run(&mut self, input: &mut Self::Input, output: &mut Self::Output);

    /// This system's self-description for wiring (§5).
    fn descriptor() -> SystemDescriptor { /* SystemKind::Async */ }
}
```

An async system owns its own loop. The coordinator spawns `run` **once** and does not tick
the system. Inside `run`, an event-driven system awaits its input ports with
`Input::recv` (backed by the ring's `Notifier` wake) and does its work on each wake; a
rate-driven system sleeps on its own timer and works on each tick. Either way it uses the
async output path (`Output::write_async`) so a full output can suspend for space. The
input and output references are `&mut` for the same reasons as `CyclicSystem::execute`.

### 1.4 The leaf trait is the structural distinction

The only difference between a cyclic and an async system is who drives it (and, as a
consequence, where its inputs come from — §3). That difference is carried by *which leaf
trait the system implements*, not by a runtime flag on the base trait. `SystemKind` exists
(§5) but only as descriptor metadata for wiring; the trait is the real distinction. This
keeps a single shared lifecycle while letting each driving model carry exactly the entry
point it needs.

### 1.5 Construction: parameters vs streamed inputs

Two deliberately different mechanisms:

- **Parameters** — gains, limits, modes, buffer depths, calibration. Fixed (or reconfigured
  rarely) and *not* a frame stream. They live in the system struct's own fields, set at
  construction before `init`. A system declares how it is built from its typed params with
  `BuildSystem`:

  ```rust
  pub trait BuildSystem: Sized {
      /// The params value the system is constructed from. `()` for a paramless system.
      type Params;
      /// Construct the (pre-init) system from its decoded params.
      fn new(params: Self::Params) -> Self;
  }
  ```

  `BuildSystem` is the **format-independent** half of construction and carries no KDL
  coupling: a `cdylib` exported via `export_system!` only needs `BuildSystem` (its
  `fsw_create` postcard-decodes `Params` and calls `new`), so the dlopen ABI does not ride
  the `kdl` feature. The KDL static-registry path layers `RegisteredSystem` on top, adding
  a `Params: FromKdlNode` bound via a blanket impl, so a statically-linked system is
  registered without extra work.

- **Streamed inputs** — IMU samples, nav estimates, commands. These flow as frames over
  rings and are read through `Self::Input` ports each `execute`/`run`.

The split keeps the hot path about frame streams only and keeps params out of the ring
transport. (A live parameter update, if ever needed, arrives as an ordinary input frame.)

---

## 2. Ports

### 2.1 Output ports — `Output<F>`

A system's `Output` associated type is a struct of one or more **output ports**, one per
output frame type. Each port wraps the single ring `Writer` the system owns for that frame:

```rust
pub struct Output<F, WD = NoWake, WS = NoWake>
where WD: WakeSource, WS: WakeSink
{
    writer: Writer<WD, WS>,
    scratch: Option<LenPacket>,   // reused table-bytes buffer for write_with (no per-call malloc)
    _f: PhantomData<F>,
}
```

Cyclic outputs default to `NoWake`; async outputs select a `Notifier` wake
so a write can suspend for space. The write methods (available when
`F: Frame + IntoBytes + Immutable`):

```rust
/// Publish a *fixed* frame (no dynamic members). The frame's `#[repr(C)]` bytes
/// **are** its table bytes (offset 0 at the fixed region), so this is a single
/// `try_write` with no serialization step.
pub fn write(&mut self, frame: &F) -> Result<(), WriteError>;

/// Publish a frame with dynamic `FrameList`/`FrameMap` members: `build` drives a
/// `FrameWriter<F>` (its `list`/`map` builders) to append the trailer, then the
/// finished table bytes are written as one record.
pub fn write_with(&mut self, fixed: &F, build: impl FnOnce(&mut FrameWriter<F>))
    -> Result<(), WriteError>;

/// **Infallible (E6)** publish of a fixed frame: a failed `try_write` is either
/// `InsufficientCapacity` (a sizing bug) or `WouldBlock` (a slow reader
/// backpressuring the ring), neither a condition the system can act on mid-cycle,
/// so `publish` counts the drop instead of returning it — the port has no
/// `HealthPort` to report through directly.
pub fn publish(&mut self, frame: &F);

/// The `write_with` twin of `publish` — same infallible/counted-drop contract.
pub fn publish_with(&mut self, fixed: &F, build: impl FnOnce(&mut FrameWriter<F>));

/// Async publish of a fixed frame: suspends until a reader frees room.
pub async fn write_async(&mut self, frame: &F) -> Result<(), WriteError>;
```

`write`/`write_with` stay for sizing-aware callers that want the `Result`; `publish`/`publish_with`
are what `#[system]`-authored `execute` bodies use (§7) — the framework folds each port's dropped
count into `health.error("publish_dropped")` once per cycle via `SystemOutput::take_dropped` (a
derive-generated sum-and-clear over every port), rather than making the author check a `Result` for
a condition that (on a correctly sized ring) never actually happens. `MsgOut`/`CommandOut` get the
same `publish`/counter.

How a write lands as bytes: for a fixed frame, the `#[repr(C)]` `F` value already *is* its
table bytes at offset 0, so `write` hands `frame.as_bytes()` straight to `Writer::try_write`.
For a dynamic frame, `FrameWriter` seeds a `LenPacket` with the fixed region, the closure
appends the trailer via `FrameWriter::{list,map}`, and `FrameWriter::table()` yields the
table bytes — exactly what the ring record payload carries, with no separate serialization
step. `write_with` retains the grown `LenPacket` in `scratch` so a per-cycle publish does
not malloc and free a fresh buffer every call.

Multiple outputs at different rates are just multiple ports in the bundle; each buffer is
sized independently and the system writes to each whenever it has new data. No output has to
advance every cycle.

### 2.2 The output bundle — `SystemOutput` and `Out<O>`

A bundle of output ports implements `SystemOutput`, which exposes only the static
descriptors used for sizing and wiring validation before any port exists:

```rust
pub trait SystemOutput {
    /// What every field contributes (§5), in field order: an ordinary wired port's
    /// `PortDesc`, or a bind-time `Capability` (e.g. the downlink's `AllOutputs` →
    /// `ReceiveAll`) that reserves no ring.
    fn decls() -> Vec<PortDecl>;
    /// The wired-port projection of `decls()` (capabilities filtered out) — what
    /// edge validation and ring sizing consume. Has a default impl in terms of
    /// `decls()`.
    fn port_descs() -> Vec<PortDesc> { /* filter_map(PortDecl::into_port) */ }
    /// Sum-and-clear every port's `publish`/`publish_with` drop counter (E6).
    /// Derive-generated; the runner folds a nonzero sum into a `publish_dropped`
    /// health error each cycle. Defaults to `0` so a hand-written bundle (which
    /// tracks no drops) still compiles.
    fn take_dropped(&mut self) -> u64 { 0 }
}
```

`SystemInput` is the input-side twin (`decls`/`port_descs`, with no counter to take — §2.5).
Both traits are `decls()`-first rather than `descriptors()`-first because a
bundle field is not always a port: a `Capability` (currently only `ReceiveAll`) reserves no ring and
wires no edge, so it cannot be represented as a `PortDesc` — `decls()` is the one type-blind walk
both the derive and the binder use, and `port_descs()` is a convenience filter over it for callers
that only care about wired ports.

The framework wraps a user's output bundle `O` in `Out<O>`, which adds the implicit
per-system health/log port pair so `output.health()` is always available:

```rust
pub struct Out<O, WD = NoWake, WS = NoWake>
where WD: WakeSource, WS: WakeSink
{
    ports: O,
    health: HealthPort<WD, WS>,
}
```

`Out` `Deref`/`DerefMut`s to the user bundle `O`, so a system reaches its own ports as
`output.<port>.write(...)` and reaches the framework handle as `output.health()`. The
`Out<O>` `SystemOutput::descriptors()` returns the user ports' descriptors followed by the
two implicit ones (`SystemHealth`, then `SystemLog`) — so a system that declares one user
output reports three output descriptors. A system's `Output` associated type is therefore
`Out<MyOutBundle>`.

### 2.3 Input ports — `Input<F>`

A system's `Input` associated type is a struct of one or more **input ports**, one per
input frame type, each wrapping a read-only ring `View`:

```rust
pub struct Input<F, RD = NoWake, RS = NoWake>
where RD: WakeSink, RS: WakeSource
{
    view: View<RD, RS>,
    _f: PhantomData<F>,
}
```

There is no scratch buffer: reads are zero-copy borrows straight off the ring. This is safe
because the ring is **lossless** — the writer can never overwrite a record a reader has not
consumed, so a borrowed record is valid for as long as the grant holding it lives.

Reads (available when `F: Frame + FromBytes + KnownLayout + Immutable`):

```rust
/// The newest committed record as a typed zero-copy borrow, or `None` if no record
/// has ever arrived. Cyclic systems want the freshest sample, not a backlog. Older
/// unread records are consumed (freed for the writer) on the way; the newest stays
/// **pinned** on the ring, so a later cycle with no new data is served the same
/// record again, and the writer backpressures rather than overwrite it
/// (`DEFAULT_DEPTH` absorbs the one pinned record per latest-wins consumer).
pub fn latest(&mut self) -> Option<FrameGrant<'_, F>>;

/// Process *every* record since the last drain, in order (command / event channels
/// that cannot drop a record). Each record is borrowed in place as a `FrameRef` and
/// freed for the writer as soon as `f` returns.
pub fn drain(&mut self, f: impl FnMut(FrameRef<'_, F>)) -> Result<(), ReadError>;

/// Await the next record (event-driven async systems). Backed by the view's async
/// `read`, which suspends on the `RD` wake until data commits. The record is
/// consumed (freed for the writer) when the grant drops.
pub async fn recv(&mut self) -> Result<FrameGrant<'_, F>, ReadError>;
```

`latest` gives the latest-wins read a cyclic sampler wants; `drain` instead delivers each
record to a closure, for a channel that must not drop; `recv` awaits the next record for an
event-driven async system. The choice between them is a system-author decision, not a trait
constraint. `ReadError` has a single variant, `Corrupt` — a structurally invalid region,
defense-in-depth for a corrupted shared mapping and unreachable from in-crate behavior
(`latest` folds it to `None`; `drain`/`recv` propagate it).

### 2.4 Typed access — `FrameRef<F>` / `FrameGrant<F>`

A callback drain hands out a `FrameRef<'_, F>`: a zero-copy typed view over one record's
table bytes. The reads that *hand back* a record (`latest`, `recv`) return a
`FrameGrant<'_, F>` — a ring `ReadGrant` (the borrow of the record in place, holding the
view's cursor) wrapped with the same accessor surface; dropping it releases the record per
the grant's semantics (consume for `recv`, keep-pinned for `latest`). Three access paths:

```rust
impl<'a, F: Frame + FromBytes + KnownLayout + Immutable> FrameRef<'a, F> {
    /// The fixed `#[repr(C)]` region, zero-copy. The table bytes at offset 0 *are* the
    /// `F` layout, so no per-field decode is needed.
    pub fn get(&self) -> &'a F;
    /// The raw table bytes (fixed region + trailer).
    pub fn table(&self) -> &'a [u8];
    /// A reader over the `FrameList<T, _>` member whose slot sits at `slot_off`.
    pub fn list<T: FromBytes>(&self, slot_off: usize) -> ListReader<'a, T>;
    /// A reader over the `FrameMap<_, V, _>` member whose slot sits at `slot_off`.
    pub fn map<V: FromBytes>(&self, slot_off: usize) -> MapReader<'a, V>;
    /// Drive any `Decomponentize` sink via the frame's vtable — the uniform escape hatch.
    pub fn apply<D: Decomponentize>(&self, sink: &mut D) -> Result<Result<(), D::Error>, ProtoError>;
}
```

1. **Fixed region — zerocopy.** Because the producer wrote `fixed.as_bytes()` at table
   offset 0, `get()` reads `F` directly via `ref_from_prefix` with no per-field decode.
2. **Dynamic members — typed readers.** `FrameList`/`FrameMap` members are read with
   `list(offset_of!(F, member))` / `map(...)` over the same table bytes.
3. **VTable apply — the escape hatch.** `apply` drives any `Decomponentize` sink (the same
   path metor-db uses) where a system wants components rather than a typed struct.

### 2.5 The input bundle — `SystemInput`

```rust
pub trait SystemInput {
    /// What every field contributes (§5), in field order: the required producer
    /// shape of every wired input port, or a bind-time `Capability`.
    fn decls() -> Vec<PortDecl>;
    /// The wired-port projection of `decls()` — what edge validation and ring
    /// sizing consume. Has a default impl in terms of `decls()`.
    fn port_descs() -> Vec<PortDesc> { /* filter_map(PortDecl::into_port) */ }
}
```

The input side carries no runtime state to collect: reads borrow off the ring, and a slow
reader shows up on the *producer's* side as a counted `publish` drop (§2.1), not as an
input-side fault.

### 2.6 Deriving the bundles

Port bundles are plain structs of `Input<F>` / `Output<F>` fields, and `#[derive(SystemInput)]`
/ `#[derive(SystemOutput)]` generate the boilerplate from the field types: `decls()`
delegates to each port's `decl()` in field order, `take_dropped()` sums-and-clears the output
ports' drop counters, and the derive also emits the matching `BindPorts` impl (§5.4). A
complete cyclic system:

```rust
#[derive(SystemInput)]
struct FilterIn { imu: Input<Imu> }

#[derive(SystemOutput)]
struct FilterOut { nav: Output<NavEstimate> }

struct Filter { gain: f64 }

impl System for Filter {
    type Input = FilterIn;
    type Output = Out<FilterOut>;
    const NAME: &'static str = "filter";

    fn init(&mut self, output: &mut Out<FilterOut>) {
        let _ = output.nav.write(&NavEstimate { /* default */ });
    }
}

impl CyclicSystem for Filter {
    fn execute(&mut self, now: Timestamp, input: &mut FilterIn, output: &mut Out<FilterOut>) {
        let Some(imu) = input.imu.latest() else {
            output.health().error("imu_missing");
            return;
        };
        let s = imu.get();
        let _ = output.nav.write_with(
            &NavEstimate { timestamp: s.timestamp, angle: s.omega * self.gain, residuals: FrameList::EMPTY },
            |fw| { fw.list(offset_of!(NavEstimate, residuals), |l| { l.push(Residual { value: s.omega }); }); },
        );
    }
}
```

### 2.7 Fan-in / fan-out

- **Single writer per buffer.** Every ring buffer has exactly one `Writer` — the producing
  system's output port. The ring enforces "at most one live writer per buffer"; the
  coordinator upholds it by constructing exactly one `Output<F>` per output buffer.
- **N producers → N views.** A consumer of N upstream producers holds N input ports, each a
  `View` into a *distinct* single-producer buffer. There is never a shared writer and never
  a fan-in buffer. Combining N streams is the consumer's job, not the transport's.

---

## 3. Cyclic vs async — input source & lifecycle

The two driving models differ in where inputs come from and how execution is triggered;
everything else is shared.

### 3.1 Cyclic systems

- **Input source:** each input port's `View` is registered directly on the upstream
  system's output buffer. No copy — reads are in-place borrows.
- **Triggering:** the coordinator calls `execute` once per cycle.
- **Backpressure, not laps.** The ring is lossless, so a slow consumer can never be lapped;
  instead its unread records hold ring space and the *producer's* `publish` starts returning
  `WouldBlock`, which the producer's runner counts and telemeters as a `publish_dropped`
  health error (§4). A `latest()` consumer pins only the newest record, so a healthy cyclic
  graph never backpressures on the default depth.
- **Wake:** none. Cyclic ports use `NoWake` for both writer and view (the synchronous `try_*`
  paths never touch the wake hooks).

### 3.2 Async systems

- **Input source:** each input port's `View` reads a **private buffer the coordinator owns**.
  The coordinator runs a copy-in `Writer` into that private buffer, mirroring the newest
  upstream record in (at most once per new upstream commit); the system never sees the
  upstream buffer directly.
- **Latest-wins, never suspend.** An async consumer that falls behind fills its private
  ring; the coordinator then skips that cycle's mirror and retries next cycle with whatever
  is newest — intermediate records are superseded, and neither the cycle loop nor the
  upstream producer ever blocks on the async consumer.
- **Triggering:** the system runs its own `run` loop. An event-driven system awaits inputs
  with `Input::recv` (backed by a `Notifier` `WakeSink`); a rate-driven system sleeps on its
  own timer. Either way it uses `Output::write_async` so a full output can suspend for
  space. The coordinator spawns `run` as a task and does not tick the system.
- **Output side is uniform.** An async system's outputs are ordinary ring buffers a
  downstream consumer (cyclic or async) reads like any other output. The async-ness is
  entirely on the input side (private copy-in) and in who calls the loop.

### 3.3 Lifecycle ordering

`init(&mut output)` runs once before any work. For a cyclic system, `execute` runs per cycle
while the slot is running; for an async system, `run` is spawned once. `shutdown(&mut output)`
runs once at teardown. `init`/`shutdown` are exactly-once; `execute` is many-times (cyclic)
or driven from within `run` (async).

---

## 4. Health / error telemetry

Systems do not return errors. A system reports its health as **ordinary frames** flowing out
over a dedicated, framework-provided **health output port** that every system gets implicitly
(named under the system's `NAME` prefix). Because health is just frames, it lands in metor-db
and any UI through the same path as all other data — no special channel.

### 4.1 The standard frames

```rust
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "health")]
pub struct SystemHealth {
    #[metor_fsw(timestamp)] pub timestamp: Timestamp,
    pub cycles: u64,
    pub errors: u64,
    pub last_execute_micros: u64,
    pub error_counts: FrameMap<u64, MAX_ERR_KINDS>,
}
```

The three scalar counters are maintained by the framework around `execute` (so they exist even
for a system that never touches health). Domain-specific kinds are bumped by name and ride
the dynamic `FrameMap`, so they need not be enumerated at compile time and still land as
fully-qualified components (`<system>.health.error_counts.<kind>`) via the dynamic-frame path.

String logs ride a parallel **log frame**, because metor-proto has no string component type —
a log line is a fixed-size byte component:

```rust
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "log")]
pub struct SystemLog {
    #[metor_fsw(timestamp)] pub timestamp: Timestamp,
    pub lines: FrameList<LogLine, MAX_LINES>,
}

#[derive(AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
pub struct LogLine { pub level: u8, pub len: u8, pub _pad: [u8; 6], pub msg: [u8; LOG_MSG_CAP] }
```

Each line's `msg` lands as a `U8`-array component queryable in db like anything else.
Capacities are bounded (`MAX_ERR_KINDS = 16`, `MAX_LINES = 16`, `LOG_MSG_CAP = 64`; longer
lines truncate) so both frames size via `MAX_SIZE` like every output.

### 4.2 The `HealthPort` handle

The health/log port pair and the framework counter state live in `HealthPort`, surfaced to a
system as `output.health()`:

```rust
output.health().error("imu_timeout");           // bump a named domain counter (+ the errors total)
output.health().log(Level::Warn, "skipped frame"); // append a log line, flushed next cycle
```

`Level` is `Info` / `Warn` / `Error`. `error` and `log` are the *only* error-reporting
mechanism a system has. The framework drives the counter maintenance:

```rust
impl HealthPort {
    pub fn end_cycle(&mut self, timestamp: Timestamp, execute_micros: u64); // bump cycles,
        // stamp duration, publish one health record + flush any pending log lines
}
```

### 4.3 The cyclic driver — `CyclicRunner`

`CyclicRunner<S, O>` is the framework wrapper that owns a cyclic system's bundles between
cycles and maintains the standard counters. It owns the system, its input bundle, its
`Out<O>` output, and a lifecycle `state`:

```rust
impl<S, O> CyclicRunner<S, O>
where S: CyclicSystem<Output = Out<O>>, O: SystemOutput
{
    pub fn new(system: S, input: S::Input, output: Out<O>) -> Self;
    pub fn init(&mut self);              // system.init once
    pub fn step(&mut self, now: Timestamp);
    pub fn shutdown(&mut self);          // system.shutdown once
    pub fn state(&self) -> &SlotState;
    pub fn output(&mut self) -> &mut Out<O>;
}
```

`step` is the per-cycle entry. If the slot is already stopped, it does nothing. Otherwise it
times `execute`, folds the output bundle's `take_dropped()` sum (if nonzero) into a
`publish_dropped` health error, and publishes the cycle's health record (`end_cycle`). The
only permanent stop is a `.so`-boundary panic (`StopReason::Panicked`, coordinator.md §3.3);
a slow reader is a backpressure/drop condition, never a stop. `CyclicRunner` also
implements `CyclicSlot`, the object-safe trait the coordinator uses to hold a heterogeneous
`Vec<Box<dyn CyclicSlot>>` and drive every system with one shared per-cycle timestamp.

Fault management beyond this telemetry is out of scope.

---

## 5. Self-description for wiring

Before any port exists, the coordinator sizes buffers, allocates reader slots, and validates
that producers satisfy consumers. A system exposes a static descriptor built entirely from
frame metadata, with no instance constructed.

### 5.1 `PortDesc` and `SystemDescriptor`

A port is one concept along **three orthogonal behavior axes**, plus its edge key, the
`telemetered` flag, and a "who connects the other end" `conn` axis
(`docs/design-port-unification.md`, `docs/design-command-slots.md` §2.1). A component-frame
("Table") port and a message ("Postcard") port are two *configurations* of the same struct, not two
types:

```rust
pub enum PortId {
    Component(ComponentId),  // F::FRAME_ID — a Table port's edge key
    Packet(PacketId),        // M::ID — a Postcard port's edge key
}

pub enum PortSchema {
    Table { vtable: VTable, announce: AnnounceFn },  // component-frame table bytes + vtable
    Postcard,                                        // self-describing (PacketId, postcard) — no vtable
}

pub enum Delivery { Snapshot, Log }   // latest-wins vs every-record
pub enum FanIn    { One, Many }       // exactly one producer vs zero-or-more (inputs only)

pub struct PortDesc {
    pub id: PortId,
    pub name: &'static str,      // F::NAME / M::NAME — display / KDL-token / registry-key name
    pub max_size: usize,         // F::MAX_SIZE / MAX_MSG_BYTES — worst-case record bytes
    pub schema: PortSchema,      // axis 1 — what a record is
    pub delivery: Delivery,      // axis 2 — what a consumer reads
    pub fan_in: FanIn,           // axis 3 — no-op on outputs
    pub telemetered: bool,       // does the downlink / AllOutputs tap this port (output-side)
    pub conn: PortConn,          // Edge (default) | Host | SelfTap(PortId)
}

pub struct SystemDescriptor {
    pub name: &'static str,           // System::NAME
    pub kind: SystemKind,             // Cyclic | Async — wiring metadata only
    pub inputs: Vec<PortDesc>,        // <Self::Input as SystemInput>::port_descs()
    pub outputs: Vec<PortDesc>,       // <Self::Output as SystemOutput>::port_descs()
    pub capabilities: Vec<Capability>, // non-port host resources (e.g. ReceiveAll), §5.4
}
```

`PortDesc::of::<F>()` mints `Table × Snapshot × One`, telemetered, `Edge`-connected — the
frame-port defaults. `PortDesc::msg::<M>()` (`M: NamedMsg`) mints `Postcard × Log × Many`,
telemetered, `Edge`-connected — the message-port defaults; `PortDesc::msg_named::<M>(name)` overrides
the display name (used for the coordinator's own reserved channels, e.g. `"commands"`,
`"sequences"`). Builder methods flip individual axes without touching the rest: `.untelemetered()`
(opt a port out of the downlink/`AllOutputs` — this is what the `CommandOut<M>` token lowers to,
`docs/messages.md`), `.with_delivery(d)`, `.with_fan_in(f)`, `.with_conn(c)`.

The same `PortDesc` describes an output (a produced record stream) and an input (a required
shape) — the two are structurally identical; direction is which `SystemDescriptor` list it sits
in. `PortSchema::Table`'s `announce` is the type-erased instance-prefix factory (telemetry.md §6);
`PortSchema::Postcard` carries none — a `(PacketId, postcard)` record is self-describing, no vtable,
no announce.

The coordinator reads this to:

1. **Size + allocate** each output buffer. `buffer_capacity::<F>(depth)` (equivalently
   `capacity_for(F::MAX_SIZE, depth)`) returns the power-of-two ring capacity for `depth`
   in-flight records: `frame_len(max_size)` adds the ring's 8-byte record header + 8-byte
   payload padding, multiplied by `depth` (at least 2) and rounded up to a power of two.
   `DEFAULT_DEPTH = 8` is used unless the coordinator config overrides `default_depth`.
2. **Validate compatibility** (§5.2).

A bundle field that is not a wired port at all — a bind-time capability like `AllOutputs` — is not a
`PortDesc`: `SystemInput`/`SystemOutput::decls()` returns `Vec<PortDecl>` (`PortDecl::Port(PortDesc)`
or `PortDecl::Capability(Capability)`), and `port_descs()` filters to the wired ones (§2.2, §2.5,
§5.4).

### 5.2 Compatibility — `compatible`

```rust
pub fn compatible(producer: &PortDesc, consumer: &PortDesc) -> bool;
```

A producer output satisfies a consumer input iff their `PortId`s match (Table and Postcard keys live
in disjoint value spaces, so a cross-schema pair can never match) *and* their `Delivery` agrees, and
then:

- **Table/Table**: the consumer's component set is a **subset** of the producer's with matching
  `ty`/`shape`. Both sides are enumerated with `VTable::realize_fields(None)` (registration mode —
  `table = None` surfaces every `(component_id, ty, shape)` triple, including dynamic member
  templates), and the check is a subset comparison over those triples. Subset (not equality) lets a
  producer emit extra fields a consumer ignores — forward-compatible wiring.
- **Postcard/Postcard**: pure `PacketId` equality (already checked via `PortId`) — a message record
  is an opaque postcard blob with no component structure to subset-check.

The check catches "consumer expects a field the producer doesn't emit," "type/shape mismatch," and
"a Log consumer wired to a Snapshot producer (or vice versa)" before a byte flows.

### 5.3 Capabilities — non-port host resources

```rust
pub enum Capability {
    /// A read view over every telemetered output in the graph (`AllOutputs`). Counts
    /// one reader slot on every buffer at sizing time. Host-only.
    ReceiveAll,
}
```

A `Capability` is granted, not wired: it reserves no ring, connects no edge, and is exempt from
`build()`'s edge-validation and ring-allocation passes — only its reader-slot accounting
participates in sizing (telemetry.md §2.5). `AllOutputs` (the downlink's receive-all tap) is the one
shipped capability; a bundle field of that type contributes `PortDecl::Capability(Capability::ReceiveAll)`
to `decls()` instead of a `PortDesc`.

### 5.4 Binding — `RingSource` and `BindPorts`

Descriptors size and allocate the rings; binding hands each typed port the ring reserved for
it. The two are symmetric and positional: binding visits port fields in the *same order* as
`descriptors()`, so a positional cursor lines each port up with its buffer.

```rust
pub trait RingSource {
    fn next_output<WD, WS>(&mut self) -> (RingBuffer, WD, WS) where /* wake bounds */;
    fn next_input<RD, RS>(&mut self) -> (RingBuffer, RD, RS) where /* wake bounds */;
    fn output_registry(&self) -> Arc<OutputRegistry> { /* host-only; default panics */ }
}

pub trait BindPorts: Sized {
    /// Construct every port from the ring source, in `descriptors()` order.
    fn bind<S: RingSource>(src: &mut S) -> Self;
}
```

A `RingSource` is where a bound port's ring comes from. Rings are backing-erased, so one
generated bundle `bind` — a single monomorphic code path — serves both providers:

- The host **`Binder`** pops the coordinator's pre-allocated `BoundPort`s
  in `descriptors()` order, each carrying its optional matched wake endpoints. (The matched
  endpoints matter only for the private copy-in buffer feeding an async input, where the view
  must share the writer's `Notifier`; every other port leaves them empty and the binder
  default-constructs the wake.)
- A dlopen'd system's **`RawBinder`** attaches the host's raw regions by
  offset (`RingBuffer::attach_raw`) over the same positional contract.

`Output<F>::bind` / `Input<F>::bind` each pop one ring with its matched wake endpoints and
wrap the resulting writer/view; the `#[derive(SystemInput)]` / `#[derive(SystemOutput)]`
macros generate the bundle `BindPorts::bind` that calls them in field order. The `Out<O>`
wrapper binds the user ports first, then the two implicit health/log ports — symmetric to its
`descriptors()` pushing the health/log descriptors after the user ports; the very same impl
binds a dlopen'd system's `Out<O>` over the host's raw-attached regions.
`output_registry` is a host-only capability (the broad-access output registry used by a
telemetry downlink/logger/recorder); a non-host source's default panics rather than fabricate
an empty registry.

---

## 6. dlopen / process boundary

A system is either an in-process Rust value, a dlopen'd dynamic library, or a separate
process, interacting over shared-memory ring buffers. The trait surface is designed so the
same system source covers the first two with no rewrite, because **the bytes are described by
the VTable, not by a shared Rust type**.

- **In-process.** A `System` is a Rust value behind the trait. `execute` is a direct method
  call. Ports wrap `Writer`/`View` over the coordinator's heap-backed rings. Cyclic ports use
  `NoWake`; async ports use `Notifier`. No Rust type crosses any boundary.
- **dlopen.** A `cdylib` is exported with the `export_system!` macro, which emits the stable C
  entry points. Rings are backing-erased, so the same `impl System` serves the loaded
  instance: its ports hold non-owning attaches to the host's shared-memory regions via
  `RawBinder` and `RingBuffer::attach_raw`. The only things crossing the `.so` boundary are (a) the ring
  regions, attached by offset, and (b) the system's params (postcard bytes decoded by
  `fsw_create` into `BuildSystem::Params`) and `SystemDescriptor`. The Rust `System` trait is
  the in-process realization of that same byte-described contract.
- **Separate process.** Identical data path; the difference is triggering (a cross-process
  wake word in the ring header rather than an in-process call / `Notifier`).

---

## 7. Authoring with `#[system]`

Everything in §1-§6 is what the framework generates for you. Hand-writing it — the `Input`/`Output`
bundle structs, `impl System`, `impl CyclicSystem`/`AsyncSystem`, `impl BuildSystem`, the `#[cfg]`'d
`export_system!` — is ceremony derivable entirely from one method's signature. The
`#[system]` attribute macro (`metor-fsw-2-macros`, `docs/design-system-macro.md`) reads the ports
straight off `execute`'s (cyclic) or `run`'s (async) parameter list, in signature order, and emits
the bundles, trait impls, and (optionally) the dl ABI exports from that one source of truth —
exactly the model `#[sequence]` already proved for stateless sequence bodies (`docs/sequences-slots.md`
§4).

```rust
use metor_fsw_2::{Input, Output, Timestamp, system};

#[system(name = "nav", export = "export")]
impl NavSystem {
    pub fn new(p: NavParams) -> Self { /* … */ }

    fn execute(
        &mut self,
        now: Timestamp,
        sensors: &mut Input<Sensors>,
        gps: &mut Input<Gps>,
        estimate: &mut Output<AttitudeEstimate>,
    ) {
        let Some(s) = sensors.latest() else { return };   // E3: latest() -> Option, no Result
        // …
        estimate.publish(&AttitudeEstimate { /* … */ });  // E6: infallible publish()
    }
}
```

This is `examples/adcs-fsw2/systems/nav/src/lib.rs` as actually shipped — no
`NavIn`/`NavOut` bundle structs, no `Out<>` wrapper, no `BuildSystem` impl, no `#[cfg]`'d
`export_system!` call; `fn new` (optional; absent ⇒ `Self: Default`) drives `BuildSystem::Params`,
and `#[system(export = "export")]` gates the `fsw_*` dl exports on the crate's own `export` feature
(bare `#[system]` emits none — a static-link-only crate stays warning-free). Recognized parameter
forms, by the last path segment of the type: `now: Timestamp` (the cycle timestamp, required on
`execute`, rejected on `run`), `&mut Input<T>`/`&mut Output<T>` (frame ports), `&mut
MsgIn<M>`/`&mut MsgOut<M>`/`&mut CommandOut<M>` (message ports), `&mut HealthPort` (optional, at
most one). Ports must be `&mut` borrows — the generated bundles own the ports (the runner holds
them between cycles) and lend them to each call; a system that needs something the classifier
doesn't recognize still writes the traits by hand (§1-§6 stay the documented escape hatch, and the
macro emits exactly what this document describes — nothing here changes because a system happens
to be authored with it).

`#[metor_fsw_2::sequence]` (`docs/sequences-slots.md` §4) shares the same signature classifier for
stateless async sequence bodies, plus a `now()`/`Seq::now()` ambient clock so a sequence can stamp
the frames it emits without threading `Timestamp` through by hand.

---

## 8. What is reused vs. defined here

| Concern | Reused (ring / proto) | Defined in the system layer |
|---------|-----------------------|-----------------------------|
| Transport | `RingBuffer`, `Writer`/`try_write`/`write`, `View`/`try_read`/`read`/`try_latest`, `ReadGrant`, `Config` | `Output<F>`/`Input<F>` port wrappers binding a frame type to one ring handle |
| Wake | `WakeSource`/`WakeSink`/`NoWake`/`Notifier` | cyclic = `NoWake`, async = `Notifier`, threaded as `WD`/`WS`/`RD`/`RS` |
| Serialization | `FrameWriter` (`new`/`list`/`map`/`table`/`finish`), `ListReader`/`MapReader`, table bytes == ring payload | `Output::write`/`write_with`/`publish`/`write_async`, `Input::latest`/`drain`/`recv`, `FrameRef`/`FrameGrant` accessors |
| Frame identity / shape | `Frame` (`FRAME_ID`, `NAME`, `timestamp`), `AsVTable::as_vtable`, `Componentize::MAX_SIZE`, `Metadatatize` | `PortDesc`/`SystemDescriptor` self-description |
| Sizing | `round_up8`, `frame_len`, `MAX_SIZE`, `Config.capacity` pow2 | `buffer_capacity`/`capacity_for`, `DEFAULT_DEPTH` |
| Wiring validation | `VTable::realize_fields(None)` registration mode, `RealizedField` | `compatible` subset / ty / shape check |
| Health | `Frame`/`FrameMap`/`FrameList`, dynamic-name path, db ingest | `SystemHealth`/`SystemLog` frames, `HealthPort`, `output.health()` |
| Backing | the erased `Backing` struct (heap/mmap/raw), `attach_raw` | `RingSource`/`BindPorts`, host `Binder`, `RawBinder` — one monomorphic bind path |
| The system itself | `Componentize`/`Decomponentize` | `System`/`CyclicSystem`/`AsyncSystem`/`BuildSystem`, `SystemInput`/`SystemOutput`, `Out`, `CyclicRunner`, `SystemKind` |
