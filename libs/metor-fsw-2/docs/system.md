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
- `src/port.rs` — the typed `Output<F>` / `Input<F>` port wrappers and `FrameRef<F>`.
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
pub trait System<B: Backing = BoxBacking> {
    /// The read-only inputs this system consumes.
    type Input: SystemInput + BindPorts<B>;
    /// The owned outputs this system produces (wrapped in `Out` for health).
    type Output: SystemOutput + BindPorts<B>;

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

### 1.1 `B: Backing` — one system, two deployment shapes

The `B` type parameter is the ring [`Backing`] the bundles are bound over. It defaults to
`BoxBacking` — the in-process host case — so an ordinary system writes a plain
`impl System for Foo` with no mention of `B`. A system that may run as a dlopen'd `cdylib`
instead writes its impls `Backing`-generic (`impl<B: Backing> System<B> for Foo`), so a
loaded instance can hold `RawBacking` views into the host's shared-memory regions. Because
the bytes on the wire are described by the VTable rather than by a shared Rust type, the
same system source serves both shapes; only the backing differs (§6).

### 1.2 Cyclic systems

```rust
pub trait CyclicSystem<B: Backing = BoxBacking>: System<B> {
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

`execute` takes `&mut Self::Input` (not `&`): draining a ring `View` advances its cursor
and fills the port's reused scratch buffer, so reading is a mutating operation. It takes
`&mut Self::Output` because the output ports wrap ring `Writer`s, which need `&mut self` to
write and which must persist across cycles — a `Writer` holds the single-writer role for a
buffer, so recreating it each cycle would be wrong. The coordinator owns the bundles
between cycles.

`descriptor()` is provided: it assembles a [`SystemDescriptor`] from `NAME`, the bundle
descriptors, and `SystemKind::Cyclic`.

### 1.3 Async systems

```rust
pub trait AsyncSystem<B: Backing = BoxBacking>: System<B> {
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
async output path (`Output::write_async`) so a lossless output can suspend for space. The
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
pub struct Output<F, B = BoxBacking, WD = NoWake, WS = NoWake>
where B: Backing, WD: WakeSource, WS: WakeSink
{
    writer: Writer<B, WD, WS>,
    scratch: Option<LenPacket>,   // reused table-bytes buffer for write_with (no per-call malloc)
    _f: PhantomData<F>,
}
```

Cyclic outputs default to `BoxBacking` + `NoWake`; async outputs select a `Notifier` wake
so a lossless write can suspend for space. The write methods (available when
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

/// Async publish of a fixed frame: suspends (lossless mode only) until there is room.
pub async fn write_async(&mut self, frame: &F) -> Result<(), WriteError>;
```

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
    /// The produced frame of every output port (§5), in field order.
    fn descriptors() -> Vec<PortDesc>;
}
```

The framework wraps a user's output bundle `O` in `Out<O>`, which adds the implicit
per-system health/log port pair so `output.health()` is always available:

```rust
pub struct Out<O, B = BoxBacking, WD = NoWake, WS = NoWake>
where B: Backing, WD: WakeSource, WS: WakeSink
{
    ports: O,
    health: HealthPort<B, WD, WS>,
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
pub struct Input<F, B = BoxBacking, RD = NoWake, RS = NoWake>
where B: Backing, RD: WakeSink, RS: WakeSource
{
    view: View<B, RD, RS>,
    scratch: Vec<u8>,   // reused copy-out target
    have: bool,         // whether scratch holds a valid record
    _f: PhantomData<F>,
}
```

Lap checking and resync (always available):

```rust
/// True iff the writer lapped this view (overwrite buffers only). The coordinator
/// checks this on cyclic systems *before* `execute` (§3.1).
pub fn is_lapped(&self) -> bool;

/// Skip to the live edge, abandoning unread (possibly lapped) data. Async input
/// ports call this on lap to "drop on full and continue" (§3.2).
pub fn resync(&self);
```

Reads (available when `F: Frame + FromBytes + KnownLayout + Immutable`):

```rust
/// Drain to the newest committed record and hand back a typed view of it, or `None`
/// if no record has ever arrived. Cyclic systems want the freshest sample, not a
/// backlog. A `Lapped` view is surfaced as `Err`.
pub fn latest(&mut self) -> Result<Option<FrameRef<'_, F>>, ReadError>;

/// Process *every* record since the last drain, in order (command / event channels
/// that cannot drop a record). Stops and returns `Err` on lap.
pub fn drain(&mut self, f: impl FnMut(FrameRef<'_, F>)) -> Result<(), ReadError>;

/// Await the next record (event-driven async systems). Backed by the view's async
/// `read_into`, which suspends on the `RD` wake until data commits. Propagates `Lapped`.
pub async fn recv(&mut self) -> Result<FrameRef<'_, F>, ReadError>;
```

`latest` loops `View::try_read_into` until caught up, keeping the last record — the
latest-wins drain a cyclic sampler wants. `drain` instead delivers each record to a closure,
for a channel that must not drop. `recv` awaits the next record for an event-driven async
system. The choice between them is a system-author decision, not a trait constraint.

### 2.4 Typed access — `FrameRef<F>`

A read hands out a `FrameRef<'_, F>`: a zero-copy typed view over one record's table bytes,
with three access paths:

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
    /// The required producer shape of every input port (§5), in field order.
    fn descriptors() -> Vec<PortDesc>;
    /// Whether any input port has been lapped (overwrite buffers). The coordinator
    /// checks this on cyclic systems before `execute` (§3.1).
    fn any_lapped(&self) -> bool;
}
```

### 2.6 Deriving the bundles

Port bundles are plain structs of `Input<F>` / `Output<F>` fields, and `#[derive(SystemInput)]`
/ `#[derive(SystemOutput)]` generate the boilerplate from the field types: `descriptors()`
delegates to each port's `descriptor()` in field order, `any_lapped()` ORs each input port's
`is_lapped()`, and the derive also emits the matching `BindPorts` impl (§5.3). A complete
cyclic system:

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
        let Ok(Some(imu)) = input.imu.latest() else {
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
  system's output buffer (overwrite mode). No copy.
- **Triggering:** the coordinator calls `execute` once per cycle.
- **Lap = hard stop.** Before invoking, the coordinator checks `any_lapped()` on the input
  bundle. A lapped view means the reader is hopelessly behind; the framework driver
  telemeters it and permanently stops the system rather than silently resyncing (§4, the
  `CyclicRunner`). The `Input` port only *exposes* `is_lapped()`; the stop policy lives in
  the driver/coordinator.
- **Wake:** none. Cyclic ports use `NoWake` for both writer and view (the synchronous `try_*`
  paths never touch the wake hooks).

### 3.2 Async systems

- **Input source:** each input port's `View` reads a **private buffer the coordinator owns**.
  The coordinator runs a copy-in `Writer` into that private buffer, copying the relevant
  upstream output records in; the system never sees the upstream buffer directly.
- **Drop on full, not stop.** An async system cannot be gated by skipping invocation, so on
  lap its input port `resync()`s to the live edge and continues — the dropped records are
  simply lost. This is the read-side behavioral difference from cyclic ports.
- **Triggering:** the system runs its own `run` loop. An event-driven system awaits inputs
  with `Input::recv` (backed by a `Notifier` `WakeSink`); a rate-driven system sleeps on its
  own timer. Either way it uses `Output::write_async` so a lossless output can suspend for
  space. The coordinator spawns `run` as a task and does not tick the system.
- **Output side is uniform.** An async system's outputs are ordinary ring buffers a
  downstream consumer (cyclic or async) reads like any other output. The async-ness is
  entirely on the input side (private copy-in) and in who calls the loop.

### 3.3 Lifecycle ordering

`init(&mut output)` runs once before any work. For a cyclic system, `execute` runs per cycle
while no input is lapped; for an async system, `run` is spawned once. `shutdown(&mut output)`
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
    pub lapped_inputs: u64,
    pub last_execute_micros: u64,
    pub error_counts: FrameMap<Name<'static>, u64, MAX_ERR_KINDS>,
}
```

The four scalar counters are maintained by the framework around `execute` (so they exist even
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
    pub fn record_lapped(&mut self);                 // bump lapped_inputs
    pub fn end_cycle(&mut self, timestamp: Timestamp, execute_micros: u64); // bump cycles,
        // stamp duration, publish one health record + flush any pending log lines
}
```

### 4.3 The cyclic driver — `CyclicRunner`

`CyclicRunner<S, O, B>` is the framework wrapper that owns a cyclic system's bundles between
cycles and maintains the standard counters. It owns the system, its input bundle, its
`Out<O, B>` output, and a lifecycle `state`:

```rust
impl<S, O, B> CyclicRunner<S, O, B>
where B: Backing, S: CyclicSystem<B, Output = Out<O, B>>, O: SystemOutput
{
    pub fn new(system: S, input: S::Input, output: Out<O, B>) -> Self;
    pub fn init(&mut self);              // system.init once
    pub fn step(&mut self, now: Timestamp);
    pub fn shutdown(&mut self);          // system.shutdown once
    pub fn state(&self) -> &SlotState;
    pub fn output(&mut self) -> &mut Out<O, B>;
}
```

`step` is the per-cycle entry. If the slot is already stopped, it does nothing. If any input
lapped, it charges the lap (`record_lapped`), logs an error, publishes a final health record
(`end_cycle`), and flips the slot to `Stopped { reason: LappedInput }` — a permanent stop.
Otherwise it times `execute` and publishes the cycle's health record. `CyclicRunner` also
implements `CyclicSlot`, the object-safe trait the coordinator uses to hold a heterogeneous
`Vec<Box<dyn CyclicSlot>>` and drive every system with one shared per-cycle timestamp.

Fault management beyond this telemetry is out of scope.

---

## 5. Self-description for wiring

Before any port exists, the coordinator sizes buffers, allocates reader slots, and validates
that producers satisfy consumers. A system exposes a static descriptor built entirely from
frame metadata, with no instance constructed.

### 5.1 `PortDesc` and `SystemDescriptor`

```rust
pub struct PortDesc {
    pub frame_id: ComponentId,       // F::FRAME_ID
    pub frame_name: &'static str,    // F::NAME (the unprefixed frame name)
    pub vtable: VTable,              // F::as_vtable() — the frame-relative component layout
    pub max_size: usize,             // F::MAX_SIZE (worst-case table bytes)
    pub announce: AnnounceFn,        // instance-prefix factory (telemetry schema)
}

pub struct SystemDescriptor {
    pub name: &'static str,          // System::NAME
    pub kind: SystemKind,            // Cyclic | Async — wiring metadata only
    pub inputs: Vec<PortDesc>,       // <Self::Input as SystemInput>::descriptors()
    pub outputs: Vec<PortDesc>,      // <Self::Output as SystemOutput>::descriptors()
}
```

`PortDesc::of::<F>()` derives a descriptor from a frame type.
The same `PortDesc` describes an output (a produced frame) and an input (a required frame
shape) — the two are structurally identical; direction is which `SystemDescriptor` list it
sits in. `announce` is a type-erased prefix factory: given an instance name it re-derives the
**prefixed** announce vtable + component metadata (`<instance>.<frame>.<field>` ids/names) the
coordinator records as the buffer's external schema. It is an `Arc<dyn Fn>` rather than a
`fn` because a dlopen'd port has no static `F` and instead carries a closure capturing its
metadata-derived prefix rewrite.

The coordinator reads this to:

1. **Size + allocate** each output buffer. `buffer_capacity::<F>(depth)` (equivalently
   `capacity_for(F::MAX_SIZE, depth)`) returns the power-of-two ring capacity for `depth`
   in-flight records: `frame_len(max_size)` adds the ring's 8-byte record header + 8-byte
   payload padding, multiplied by `depth` (at least 2) and rounded up to a power of two.
   `DEFAULT_DEPTH = 8` is used unless the coordinator config overrides `default_depth`.
2. **Validate compatibility** (§5.2).

### 5.2 Compatibility — `compatible`

```rust
pub fn compatible(producer: &PortDesc, consumer: &PortDesc) -> bool;
```

A producer output satisfies a consumer input iff they share a `frame_id` *and* the consumer's
component set is a **subset** of the producer's with matching `ty`/`shape`. Both sides are
enumerated with `VTable::realize_fields(None)` (registration mode — `table = None` surfaces
every `(component_id, ty, shape)` triple, including dynamic member templates), and the check
is a subset comparison over those triples. Subset (not equality) lets a producer emit extra
fields a consumer ignores — forward-compatible wiring. The check catches "consumer expects a
field the producer doesn't emit" and "type/shape mismatch" before a byte flows.

### 5.3 Binding — `RingSource` and `BindPorts`

Descriptors size and allocate the rings; binding hands each typed port the ring reserved for
it. The two are symmetric and positional: binding visits port fields in the *same order* as
`descriptors()`, so a positional cursor lines each port up with its buffer.

```rust
pub trait RingSource {
    type B: Backing;
    fn next_output<WD, WS>(&mut self) -> (RingBuffer<Self::B>, WD, WS) where /* wake bounds */;
    fn next_input<RD, RS>(&mut self) -> (RingBuffer<Self::B>, RD, RS) where /* wake bounds */;
    fn output_registry(&self) -> Arc<OutputRegistry> { /* host-only; default panics */ }
}

pub trait BindPorts<B: Backing>: Sized {
    /// Construct every port from the ring source, in `descriptors()` order.
    fn bind<S: RingSource<B = B>>(src: &mut S) -> Self;
}
```

A `RingSource` is where a bound port's ring comes from, abstracted over the backing `B` so one
generated bundle `bind` serves both providers:

- The host **`Binder`** (`B = BoxBacking`) pops the coordinator's pre-allocated `BoundPort`s
  in `descriptors()` order, each carrying its optional matched wake endpoints. (The matched
  endpoints matter only for the private copy-in buffer feeding an async input, where the view
  must share the writer's `Notifier`; every other port leaves them empty and the binder
  default-constructs the wake.)
- A dlopen'd system's **`RawBinder`** (`B = RawBacking`) attaches the host's raw regions by
  offset (`RingBuffer::attach_raw`) over the same positional contract.

`Output<F>::bind` / `Input<F>::bind` each pop one ring with its matched wake endpoints and
wrap the resulting writer/view; the `#[derive(SystemInput)]` / `#[derive(SystemOutput)]`
macros generate the bundle `BindPorts::bind` that calls them in field order. The `Out<O>`
wrapper binds the user ports first, then the two implicit health/log ports — symmetric to its
`descriptors()` pushing the health/log descriptors after the user ports — threading the ring
source's backing `B` so a dlopen'd system's `Out<O, RawBacking>` binds over the host regions.
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
  call. Ports wrap `Writer`/`View` over `BoxBacking`. Cyclic ports use `NoWake`; async ports
  use `Notifier`. No Rust type crosses any boundary.
- **dlopen.** A `cdylib` is exported with the `export_system!` macro, which emits the stable C
  entry points. The system writes its impls `Backing`-generic, so a loaded instance holds
  `RawBacking` ports attached to the host's shared-memory regions via `RawBinder` and
  `RingBuffer::attach_raw`. The only things crossing the `.so` boundary are (a) the ring
  regions, attached by offset, and (b) the system's params (postcard bytes decoded by
  `fsw_create` into `BuildSystem::Params`) and `SystemDescriptor`. The Rust `System` trait is
  the in-process realization of that same byte-described contract.
- **Separate process.** Identical data path; the difference is triggering (a cross-process
  wake word in the ring header rather than an in-process call / `Notifier`).

---

## 7. What is reused vs. defined here

| Concern | Reused (ring / proto) | Defined in the system layer |
|---------|-----------------------|-----------------------------|
| Transport | `RingBuffer`, `Writer`/`try_write`/`write`, `View`/`try_read_into`/`read_into`/`is_lapped`/`resync`, `Overrun`, `Config` | `Output<F>`/`Input<F>` port wrappers binding a frame type to one ring handle |
| Wake | `WakeSource`/`WakeSink`/`NoWake`/`Notifier` | cyclic = `NoWake`, async = `Notifier`, threaded as `WD`/`WS`/`RD`/`RS` |
| Serialization | `FrameWriter` (`new`/`list`/`map`/`table`/`finish`), `ListReader`/`MapReader`, table bytes == ring payload | `Output::write`/`write_with`/`write_async`, `Input::latest`/`drain`/`recv`, `FrameRef` accessors |
| Frame identity / shape | `Frame` (`FRAME_ID`, `NAME`, `timestamp`), `AsVTable::as_vtable`, `Componentize::MAX_SIZE`, `Metadatatize` | `PortDesc`/`SystemDescriptor` self-description |
| Sizing | `round_up8`, `frame_len`, `MAX_SIZE`, `Config.capacity` pow2 | `buffer_capacity`/`capacity_for`, `DEFAULT_DEPTH` |
| Wiring validation | `VTable::realize_fields(None)` registration mode, `RealizedField` | `compatible` subset / ty / shape check |
| Health | `Frame`/`FrameMap`/`FrameList`, dynamic-name path, db ingest | `SystemHealth`/`SystemLog` frames, `HealthPort`, `output.health()` |
| Backing | `BoxBacking`/`MmapBacking`/`RawBacking`, `attach_raw` | `RingSource`/`BindPorts`, host `Binder`, `RawBinder`, `B`-generic threading |
| The system itself | `Componentize`/`Decomponentize` | `System`/`CyclicSystem`/`AsyncSystem`/`BuildSystem`, `SystemInput`/`SystemOutput`, `Out`, `CyclicRunner`, `SystemKind` |
