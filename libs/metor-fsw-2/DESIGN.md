# Metor FSW

Metor FSW (`metor-fsw-2`, imported as `metor_fsw_2`) is a framework for building highly
modular flight software out of small, independently-developed pieces. A mission is a graph
of **systems** — an IMU driver, a navigation filter, an attitude controller — that a single
**coordinator** wires together, schedules, and observes. Systems exchange **frames** of
typed **components** over shared-memory **ring buffers**, and the whole graph streams its
state off-board through one **telemetry downlink**.

The framework is built on a handful of primitives that compose, plus one structural
symmetry that ties them together: a frame's in-memory bytes *are* its on-the-wire bytes,
described by the same metor-proto VTable whether the consumer is a peer system, the
downlink, or the metor-db time-series database. There is no separate serialization step
anywhere in the data path.

This document is the top-level overview. Each subsystem has a detailed companion document
in `docs/` (referenced inline and listed at the end).

## Components

Components are the leaf values that flow through the system. Each is identified by a
`ComponentId` — the fnv1a-64 hash of its dotted name, with the top bit masked — and is a
fixed-size N-dimensional array of a common Rust scalar (`f64`, `u64`, `i32`, …). Components,
their `ComponentId`s, and the VTable mechanism that describes them all come from the
metor-proto library; `metor-fsw-2` builds on those primitives rather than re-inventing them.

## Frames

Components are grouped into **frames**: `#[repr(C)]` structs whose fields are components
sharing one logical timestamp. A single `#[derive(Frame)]` turns a plain struct into a
timestamped, `ComponentId`-named group of components:

```rust
use metor_fsw_2::*;
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: [f64; 3],
    accel: [f64; 3],
}
```

A frame is itself named by a `ComponentId` (`Frame::FRAME_ID`, the hash of `Frame::NAME`),
and each field is a component hanging off that dotted prefix (`imu.omega`, `imu.accel`).
The timestamp is marked once with `#[metor_fsw(timestamp)]` and propagated to every field.
The `Frame` trait (`src/frame.rs`) is a thin bundle over the four metor-proto component
traits — `AsVTable`, `Metadatatize`, `Componentize`, `Decomponentize` — plus the frame
name/id and the shared-timestamp accessor; the derive expands to all four sub-derives and
the `Frame` impl.

Frames are the contracts between subsystems. The IMU driver produces an `Imu` frame; the
navigation filter consumes it. Because a frame is a timestamped, `#[repr(C)]` group of
components, its bytes serialize directly to a metor-proto table described by a VTable — the
same representation metor-db ingests, with no extra step. See `docs/frames.md`.

## VTables: the reflection layer

Different systems have different inputs and outputs, so the coordinator needs a description
of what each system's frames contain in order to wire them together. **VTables** provide
that description: a frame's VTable enumerates the components (and dynamic members) that make
up the frame, with each leaf's type, shape, and `ComponentId`.

VTables are a core part of the metor-proto protocol — they are how consumers like metor-db
parse and understand messages. `metor-fsw-2` reuses that exact mechanism, which is the key
symmetry of the design: the description a peer system uses to consume a frame, the
description the downlink announces, and the description metor-db stores are one and the same.
Supporting frames and runtime-dynamic components extends the metor-proto VTable op set with a
frame tag op and `List`/`Map`/`PathComponent` ops; see `docs/vtable-dynamic.md`.

## Dynamic components

Frames support a bounded form of runtime dynamism through two member types,
`FrameList<T, MAX>` and `FrameMap<K, V, MAX, MAX_KEY>` (`src/dynamic.rs`). Consider a system
reporting per-process telemetry, where the number of processes is unknown at compile time
and changes at runtime:

```rust
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "procmon")]
struct ProcMon {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    // name-keyed: processes.htop.cpu_usage
    processes: FrameMap<Name<'static>, Process, 64>,
    // or positionally indexed: processes.0.cpu_usage
    // processes: FrameList<Process, 64>,
}
```

A consumer of such a frame knows two things statically: the element type (`Process`), and
how to address an element — by a key (map) or by index (list). The const generics carry the
maximum cardinality (and, for maps, the maximum key length) so that a frame's worst-case
size, `Componentize::MAX_SIZE`, remains a compile-time constant the coordinator can use to
size a ring buffer.

Each dynamic element is addressed by a **fully-qualified component name** formed from the
field's path prefix plus its key or index — `processes.htop.pid` for a name-keyed map,
`processes.0.pid` for a list. The `ComponentId` is the hash of that full name, so even
deeply nested dynamic data yields ordinary, fully-qualified components that metor-db and UIs
store and query without special cases. Map keys are validated `Name`s: they may not contain
`.` (it would alias the path separator) or be empty (an empty segment vanishes under the
path hasher).

In the `#[repr(C)]` layout, a list/map field **is** an 8-byte `Slot { trailer_off, byte_len }`
— nothing more. The element data lives in a padded **trailer** after the fixed portion of
the frame (a layout cribbed from rkyv / flatbuffers); the slot points into it. With purely
fixed data the trailer is empty and unused. The producer builds the trailer with a
`FrameWriter` (`src/writer.rs`, with its `ListWriter`/`MapWriter` builders); a consumer reads
it with a `ListReader`/`MapReader` (`src/reader.rs`) or through the VTable `apply` escape
hatch. See `docs/frames.md`.

## Systems

A system encapsulates one piece of functionality. Every system implements the shared
`System` base trait (`src/system/mod.rs`), which carries the typed input/output bundle types,
a wiring `NAME`, and the once-each `init`/`shutdown` lifecycle hooks:

```rust
pub trait System<B: Backing = BoxBacking> {
    type Input: SystemInput + BindPorts<B>;
    type Output: SystemOutput + BindPorts<B>;
    const NAME: &'static str;
    fn init(&mut self, output: &mut Self::Output) {}
    fn shutdown(&mut self, output: &mut Self::Output) {}
}
```

The input and output bundles are plain structs of typed ports, derived with
`#[derive(SystemInput)]` / `#[derive(SystemOutput)]`. On top of `System`, a user implements
exactly one of two leaf traits expressing how the system is driven:

- **`CyclicSystem`** is coordinator-driven. The coordinator calls
  `execute(now, input, output)` once per cycle. Its inputs are read-only views straight into
  the upstream producers' output buffers.
- **`AsyncSystem`** owns its own loop. The coordinator spawns `run(input, output)` once and
  never ticks it; the system paces itself on a timer or by awaiting its inputs. Its inputs
  read from private buffers the coordinator copies into (see below).

```rust
pub trait CyclicSystem<B: Backing = BoxBacking>: System<B> {
    fn execute(&mut self, now: Timestamp, input: &mut Self::Input, output: &mut Self::Output);
}
```

`now` is the coordinator's per-cycle timestamp — the same value for every system in one
cycle, and the value a simulated clock advances. A system stamps its output frames with it
rather than reading the wall clock independently, which is what makes a mission replayable
under a simulated clock.

### Health instead of errors

Systems never return errors. The framework wraps a system's user output bundle `O` in
`Out<O>`, which adds an implicit per-system **health/log** port pair every system gets for
free. A system reports trouble through it:

```rust
output.health().error("i2c_timeout");          // bump a named error counter
output.health().log(Level::Warn, "retrying");  // append a log line
```

The framework maintains four standard counters around each `execute` — cycles, total errors,
lapped inputs, and last-execute microseconds — and publishes a `SystemHealth` frame plus a
`SystemLog` frame every cycle (`src/health.rs`). Named error counts ride a dynamic `FrameMap`
inside the health frame, so they land as ordinary components (`health.error_counts.<kind>`).
Health and logs flow out like any other frame data, so a system's troubles are observable
off-board through the same downlink as its telemetry. See `docs/system.md`.

## Ports and ring buffers

Data moves over ring buffers (the `metor_fsw_ring` crate, re-exported as
`metor_fsw_2::ring`). Each system **owns** its output buffers and **borrows** views into its
inputs. A typed `Output<F>` port (`src/port.rs`) wraps the single ring `Writer` a system
holds for frame `F`; a typed `Input<F>` wraps a read-only `View`. The ports are thin: because
a frame's `#[repr(C)]` bytes *are* its table bytes, publishing a fixed frame is a single ring
write with no serialization, and reading hands back a zero-copy typed `FrameRef<'_, F>`.

A consumer reads with `latest()` (drain to the newest committed record — what a cyclic system
wants), `drain()` (process every record in order — for command/event channels that cannot
drop), or `recv()` (await the next record — for event-driven async systems).

The ring buffer is **shared-memory backed**: its entire state lives in one contiguous,
offset-addressed region with no process-local pointers inside it, so the same mechanism works
whether two systems share an address space or (with the ring's `mmap` feature) span
processes. See `docs/ring-buffer.md`.

### Overrun semantics

The overrun policy is the writer's choice, fixed when the buffer is created and recorded in
its header (`Overrun` in the ring crate):

- **`Overwrite`** (the default) — the writer never blocks on readers. When it would reuse
  bytes a slow reader has not consumed, it overwrites them; the slow reader detects this on
  the read side as a **lap**.
- **`Lossless`** — the writer honors the in-use check: a synchronous `try_write` returns
  `WouldBlock`, and the async `write` suspends until a reader frees space. It never laps a
  reader, so guaranteed delivery is available where a channel needs it.

The framework favors bounded, non-blocking writers with live lap detection over guaranteed
delivery by default. In `Overwrite` mode, slowness is handled differently per system kind:

- **Cyclic systems:** before invoking a cyclic system, the framework checks whether any of
  its input views were lapped. A lapped input means the reader is hopelessly behind, which is
  treated as a **hard error**: it is telemetered, and the coordinator permanently stops
  invoking that system (recovery is future work).
- **Async systems:** they cannot be gated by simply not invoking them, so the coordinator
  mirrors upstream output into each async system's **private input buffer**, which is itself
  an `Overwrite` ring. If the async consumer is behind, the oldest unconsumed record is
  dropped (drop-on-full).

## The coordinator

`Coordinator` (`src/coordinator/mod.rs`) owns the ring regions, validates and wires the
system graph, drives the run loop, and emits coordinator-level health and a status frame. It
is built in two phases.

A `CoordinatorBuilder` registers systems and edges:

```rust
let mut b = Coordinator::builder(CoordinatorConfig::default());
let imu = b.add_cyclic(ImuDriver::new());
let nav = b.add_cyclic(MekfFilter::new());
b.connect(PortRef::new::<Imu>(imu), PortRef::new::<Imu>(nav))?;
b.add_telemetry(TelemetryConfig { transport: TcpTransport::new(addr), mode: TelemetryMode::All });
let mut coordinator = b.build()?;
```

`build()` is where the graph is checked and laid out before a single byte flows: it
validates every edge (frame-id match, structural compatibility, no unconnected or
double-connected inputs, no unbroken feedback cycle), sizes and allocates one ring per output
port, binds every typed port over its ring, provisions the implicit health/log buffers, and
returns a ready `Coordinator`. Wiring mistakes surface as a `WireError` here, not at runtime.

Then `Coordinator::run_for(cycles)` (or the underlying lifecycle) drives the run phase on the
`stellarator` async runtime: it spawns the async systems, runs every system's `init` behind a
barrier so all setup completes before the first cycle, then loops.

### Port compatibility and self-description

A system describes itself statically through a `SystemDescriptor` (`src/descriptor.rs`): its
name, kind, and the `PortDesc` of every input and output — each a frame id, VTable, worst-case
size, and rate hint, all derived from the frame type with no instance needed. `connect`
addresses a port by `(system, frame_id)`, and `build()` checks each edge with `compatible`:
the producer and consumer must share a frame id, and the consumer's component set must be a
**subset** of the producer's with matching types and shapes. Subset (not equality) lets a
producer emit extra fields a consumer ignores, so wiring is forward-compatible.

### Each cycle

Under the default **wall clock**, the loop runs every cyclic system once in registration
order, performs the async copy-in step, refreshes the status frame, then sleeps to hold the
configured `cycle_rate` (run-fast-then-wait); an overrun is telemetered rather than allowed
to slip the rate silently. Under a **simulated clock** (`ClockMode::Simulated { dt }`), the
per-cycle `now` advances by a fixed logical step from an epoch and the loop does not sleep —
it runs as fast as the host allows — so a mission converges in fixed sim time regardless of
host speed. This is what makes hardware-in-the-loop and offline simulation runs deterministic.

Every cyclic system runs every cycle; there is no per-system rate division yet. Async systems
run at their own pace, decoupled by the copy-in buffers.

### Feedback loops

A control loop is a cycle in the graph (controller → plant → sensor → controller). Exactly
one edge of each loop must be declared with `connect_delayed` instead of `connect`. The
runtime path is identical — a read of the latest committed value, which is last cycle's
because the producer runs after the consumer in registration order — but a delayed edge is
excluded from cycle detection, so the one-cycle-late sampling is explicit rather than an
accident of registration order. An unbroken cycle is rejected at `build()` as a
`FeedbackCycle`. See `docs/coordinator.md`.

## Wiring a mission

A mission's graph is described by the `Wiring` data model (`src/wiring/model.rs`): a plain,
serde-serializable Rust description of the coordinator config, the loadable artifacts, the
system instances, the edges, and the optional telemetry downlink. `Wiring` is the single
source of truth, and it has two equivalent front-ends:

- A **KDL document**, deserialized by `parse` — a text format for declaring systems, their
  params, and their connections.
- The **`WiringBuilder`** (`src/wiring/builder.rs`), a fluent Rust constructor.

```kdl
coordinator cycle_rate=200.0 default_depth=8
system "imu" type="ImuDriver" i2c_bus=1 sample_hz=200.0
system "nav" type="MekfFilter" gain=0.8
connect "imu" -> "nav" frame="imu"
telemetry {
    transport "tcp" addr="127.0.0.1:2240"
    mode "all"
}
```

Both front-ends feed one shared `resolve`, which instantiates each system, connects the
edges, adds telemetry, and `build()`s the `Coordinator`. Static (compiled-in) system types
are looked up in an app-built `Registry` that maps a `type=` string to a factory; a system
crate exposes a `register` function that adds its own types. Every load failure is a
span-carrying `LoadError` diagnostic. Each system is given a distinct **instance name** (the
first KDL argument), which becomes the per-instance prefix the downlink applies, so two
instances of one system type never collide on the wire. See `docs/wiring.md`.

## Running systems as cdylibs

A system is either **statically linked** into the host binary or compiled to a **`cdylib`
that the host `dlopen`s** at runtime; both run in the same process and exchange data over the
same shared-memory rings, so a dynamically-loaded system sees the identical atomics a
statically-linked one does — no copy, no IPC. The only difference is how a system is
constructed and how its lifecycle is invoked. (Running a system as a *separate process* is a
natural extension the shared-memory data path is designed for, but is not yet implemented.)

dlopen across a stable Rust ABI is genuinely hard, which is exactly why the data path is built
on shared-memory rings rather than passing Rust types across the boundary. Only **serialized
bytes** (a postcard-encoded descriptor and params blob) and **`#[repr(C)]` handles** ever
cross the C ABI — never a `Vec`, `Arc`, or `VTable` by value. A system `cdylib` exports a
small, versioned `extern "C"` surface (`src/abi/mod.rs`): an ABI-version word the host checks
for equality before anything else, a `fsw_describe` that hands back the system's serialized
`SystemDescriptor` plus its `Params` schema, and the `fsw_create` / `fsw_bind_init` /
`fsw_execute` / `fsw_shutdown` / `fsw_destroy` lifecycle. The `export_system!` macro emits all
of it as a one-liner per export. Every `extern "C"` body wraps its work in `catch_unwind`, so
no unwind ever crosses the boundary (which would be undefined behavior); a caught panic is
mapped to a clean status and hard-stops just that slot.

On the host side, `DlSystem::open` (`src/dl.rs`) loads the `.so`, checks the ABI word, and
reconstructs the `SystemDescriptor` — after which a dlopen'd system is wiring-validated, sized,
and allocated exactly like a static one. At `build()` it becomes a `DlSlot` that the
coordinator drives as just another cyclic slot, forwarding `init`/`step`/`shutdown` across the
ABI and handing the `.so` raw ring-region handles to attach its ports to. dlopen'd systems are
cyclic-only. Params declared in KDL are encoded against the `.so`'s exported `Params` schema
(the host never links the `Params` type), producing the exact same postcard bytes the typed
`WiringBuilder` produces — so the KDL and Rust front-ends are byte-equivalent. See
`docs/dl-open.md`.

## Telemetry downlink

Flight software is only useful if its state can be observed off-board. The framework provides
a dedicated **telemetry system** (`src/telemetry/mod.rs`) that taps the outputs of other
systems and streams them out in metor-proto's wire format — the same format metor-db ingests.
The downlink and the database are two ends of one protocol: a frame produced by a system is
streamed out as a table, and metor-db stores it with no translation. Because a frame's ring
payload *is* its table bytes, there is no serialization step; the bytes a system committed are
the bytes on the wire.

`TelemetrySystem` is an ordinary `CyclicSystem`, registered last so its end-of-cycle snapshot
captures every other system's freshest output. It has two modes:

1. **`All`** — tap every output in the graph: every system's frames, their implicit
   health/log, and the coordinator-owned health/log/status. No wiring required.
2. **`Subset`** — tap a named set of instances or frames, for a constrained downlink budget.

Rather than a fixed set of typed inputs, the streamer reaches outputs through the **output
registry** (below), which gives it a read view plus each tapped buffer's VTable and producing
instance name. Each distinct VTable is announced once; thereafter each cycle pushes that
frame's latest bytes as a `Table` packet referencing the announced VTable, with component paths
prefixed by the producing instance's name (so `processes.htop.pid` from the `procmon` instance
downlinks as `procmon.processes.htop.pid`). This is where the per-instance namespacing the
wiring records becomes load-bearing on the wire.

The control cycle is synchronous but the socket is async, so the in-cycle stage only
*snapshots* each tapped buffer (a memcpy) into a bounded, per-tap-coalescing hand-off, and a
spawned sender task drains it and does the awaiting I/O. The cycle never blocks on the link: a
backed-up transport just causes newer snapshots to overwrite un-sent ones (latest-wins),
bumping a `telemetry.dropped` health counter — loss, never delay, the same philosophy as the
`Overwrite` rings one level down. The transport is pluggable behind the `Transport` trait;
`TcpTransport` is the shipped implementation (a stream to a ground link or co-located metor-db),
with a shared-memory queue a natural future alternative. The bytes on the wire are identical
either way, and a consumer needs only the announced VTables to parse them. See
`docs/telemetry.md`.

## The output registry

Underneath the downlink is a general capability: the `OutputRegistry` (`src/registry.rs`), a
queryable index over every tappable output buffer in the graph, keyed by each buffer's
instance-qualified id `ComponentId::new("<instance>.<frame>")`. The telemetry downlink is its
first consumer, but it is general — any broad or dynamic reader (a logger, a recorder, a
debugger, a test) reaches outputs the same way. The registry never exposes a raw buffer, only
a `view()` factory, so every reader is slot-accounted against the buffer's build-time
`max_readers` budget. The coordinator sizes that budget to include each known registry
consumer, so an `All`-mode downlink always has a reader slot on every buffer.

## Messages, the uplink, and the command plane

Beside the fixed-frame data path is a second payload kind: **messages** — self-describing
`(PacketId, postcard)` records carried on byte rings, indexed by a parallel `MessageRegistry`,
and tapped by telemetry just like outputs. Messages carry variable-length `serde` types (the
panel's sequence registry and per-channel events) that do not fit the `#[repr(C)]` frame mold.

The framework closes the operator loop in **both** directions over messages. The **downlink**
taps every message channel and streams each record off-board (the message twin of the output
tap). The **uplink** is its read twin: an ordinary `AsyncSystem` (`UplinkSystem`) that owns its
own connection, receives panel-published `SequenceCommand` messages, and re-emits them onto a
reserved **`"commands"`** message channel. The coordinator runs a generic **command bus**: at the
head of each cycle it drains every `"commands"` channel from the `MessageRegistry` (plus one
coordinator-owned channel backing an in-process `control_handle()`), decodes each `SequenceCommand`,
and dispatches it to the addressed runtime slot by `channel_id` — before stepping the slots, so a
command lands the same cycle it arrives. The coordinator is command-generic: it knows nothing of
sequences, only how to drain a command bus and dispatch. Uplink and downlink use **separate
connections**: a connection is an owned resource the system/ring model cannot distribute the way it
hands out ring views, so sharing one socket across two systems is deferred. See `docs/messages.md`.

## Limitations and future work

The design intentionally leaves several capabilities for later; they are noted here in one
place rather than scattered through the prose:

- **Cross-process systems.** Systems run statically linked or dlopen'd in one process today.
  The shared-memory ring (with its `mmap` feature) is designed to carry the data path across
  process boundaries, but the cross-process notification/launch mechanism is not built.
- **Recovery from a hard stop.** A cyclic system that laps an input or panics (in a `.so`) is
  permanently stopped; there is no restart or quarantine-and-resume path.
- **Per-system rates.** Every cyclic system runs every cycle; there is no rate division beyond
  the single global cycle rate (a system can still self-pace by running async).
- **Lossless command/event channels end-to-end.** The ring supports a `Lossless` overrun mode,
  but the default cross-system edges are `Overwrite`; first-class guaranteed-delivery command
  channels are a future extension.
- **Automatic transport reconnect.** The TCP downlink connects once and stops downlinking on
  disconnect; reconnect/backoff is future work.
- **Shared uplink+downlink connection.** The uplink and downlink each open their own connection.
  A connection is an owned OS resource the system/ring model cannot split into independent handles
  the way it distributes ring views, so sharing one socket across two systems needs a "shared owned
  resource" abstraction that does not exist yet (`docs/messages.md` §4.5).

## Document map

This overview stands on its own; the companion documents in `docs/` carry the detailed design
of each subsystem:

- `docs/frames.md` — frames, components, dynamic `FrameList`/`FrameMap`, the trailer layout.
- `docs/vtable-dynamic.md` — the VTable op-set extensions for frames and dynamic data.
- `docs/system.md` — the system trait family, typed ports, and per-system health.
- `docs/ring-buffer.md` — the shared-memory ring buffer and its overrun semantics.
- `docs/coordinator.md` — the builder, graph validation, scheduling, clocks, and lifecycle.
- `docs/wiring.md` — the `Wiring` data model, the KDL front-end, and the Rust builder.
- `docs/dl-open.md` — the `cdylib` C-ABI, the loader, and schema-guided params.
- `docs/telemetry.md` — the output registry and the telemetry downlink.
- `docs/cli-runner.md` — the `metor-fsw` CLI: loading a wiring, packaging a bundle, and running a mission.
- `docs/sequences-slots.md` — runtime-loadable slots and the `#[sequence]` author surface.
- `docs/messages.md` — the message channel (a second payload kind), the telemetry uplink/downlink of messages, and the `SequenceCommand` command plane.
</content>
</invoke>
