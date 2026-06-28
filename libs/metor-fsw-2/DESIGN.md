# Metor FSW

Metor FSW is a framework designed to make it easy to build highly complex, modular, flight
software. It does this through a few core sets of primitives.

## Components

Components are values identified with a unique ComponentId and name. The ComponentId is a fnv1a
hash of the name. Components are fixed size N dimensional arrays, that can be any of the common
Rust scalar values.

You can read more about components in the metor-proto library.

## Component Frame

Components can be grouped into "frames": collections of components that share a logical timestamp.
For example:
```rust
struct IMU {
    timestamp: Timestamp,
    omega: Vec3<f64>,
    accel: Vec3<f64>,
}
```
A frame is itself identified by a ComponentId (the fnv1a hash of its name), and each field is a
component. The shared timestamp is marked once and propagated to every field (see the
`#[metor_fsw(timestamp)]` attribute that metor-fsw already provides).

The goal with frames is that they act as contracts between subsystems. The IMU driver produces an
IMU frame; the navigation filter consumes it. Because a frame is just a timestamped group of
components, it serializes to a metor-proto table described by a VTable — the same representation
metor-db ingests, with no extra serialization step.

## Systems

A system in Metor FSW is a modular piece of software that encapsulates a particular piece of
functionality — an IMU driver, a navigation filter, etc. A single piece of software, the
coordinator, links individual systems together into a cohesive whole. A system implements this
trait (rough pseudo code — not the final signature):
```rust
trait System {
    type Input: SystemInput;
    type Output: SystemOutput;
    fn init(&mut self);
    fn execute(&mut self, input: Input, output: Output);
    fn shutdown(&mut self);
}
```
`init` runs once before the system is first executed; `shutdown` runs once when it is torn down.
Systems do not return errors. Instead each system reports its own health as telemetry — error
counters and string logs — which flows out like any other component data.

## Cross System Communication

metor_coordinator moves data between systems and executes them. It runs at a fixed, configurable
cycle rate.

There are two kinds of systems:
1. **Cyclic** systems are invoked once per cycle by the coordinator directly. Their inputs are
   read-only **views** into other systems' output buffers.
2. **Async** systems run at their own rate — event-driven, or some other cycle rate. The
   coordinator does not invoke them; instead it owns **dedicated input buffers** for them, copying
   the relevant outputs in and reading their outputs back out.

Aside from how they are invoked, the only structural difference is that cyclic systems read views
directly into upstream outputs, while async systems read from their own private input buffers.

Data moves over ring buffers. Each system writes its output into one or more ring buffers and
reads its inputs from views into other buffers. Systems own their outputs and borrow their inputs.
The buffers are shared-memory backed so the same mechanism works whether a system is loaded
in-process or runs as a separate process. The detailed design of the ring buffer lives in its own
document.

### Overrun semantics (writer-chosen)

By default writers do not block on readers: a system writing to its output buffer makes progress
even if that means overwriting data a slow reader has not yet consumed. A writer may, however,
choose to **not write** (error) or **wait** rather than overwrite data an active reader has not
yet consumed, when it needs guaranteed delivery. In the default overwrite mode, slowness is
detected on the read side:

- **Cyclic systems:** before invoking a cyclic system, the coordinator checks the read head of
  each of its input views. If a read head has already been overwritten (the writer lapped it), the
  reader is hopelessly behind. This is treated as a **hard error**: it is telemetered and the
  coordinator stops invoking that module.
- **Async systems:** they cannot be gated by simply not invoking them, so the coordinator copies
  output data into the async system's private input buffer. If there is no room, the data is
  **dropped**.

The trade-off is explicit: we favor bounded, non-blocking writers and live detection of
unrecoverable lag over guaranteed delivery. Command/event-grade lossless channels, if needed, are
a future extension.

### Each cycle

For now the coordinator runs each system as fast as possible within a cycle, then waits at the end
of the cycle to hold the configured rate. Deterministic ordering and replay are future work.

### What are systems anyway?

A system is either a dynamic library that is dl-opened, or a separate process. In both cases we
interact with them through shared-memory ring buffers. The difference is only how execution is
triggered: the dl-opened version exposes an execute entry point we call directly; the process
version needs a notification mechanism (TBD). dl-open across a stable ABI is genuinely hard, which
is part of why the data path is built on shared memory rather than passing Rust types across the
boundary.

### VTables / Limited Reflection

If two systems have different inputs and outputs, how do we wire them up? Systems output sets of
components as repr(C) tables, and the coordinator needs to know what is in each bundle. VTables
provide that description: they enumerate the components and frames that make up a system's output.

VTables are a core part of the metor-proto protocol — they are how consumers like metor-db parse
and understand messages. We reuse that exact mechanism here, which is the key symmetry in the
system. Supporting frames and dynamic components requires extending the VTable op set (a frame's
ComponentId, and list/map ops for dynamic data — see below).

### Wiring up systems

Users need a clean way to declare that the outputs of one system feed the inputs of another, with
the coordinator validating compatibility against the systems' VTables. The concrete mechanism is
TBD.

### Dynamic Components

Frames support a limited form of runtime dynamism. Imagine a module that produces telemetry about
the processes on the system: at compile time you cannot know how many processes there will be, and
they come and go at runtime.
```rust
struct Process {
  pid: u64,
  cpu_usage: f64
}

struct ProcessesField {
  processes: FrameMap<ComponentId, Process> // or
  processes: FrameList<Process>
}
```
In both cases the consumer knows two things: (1) the type of the dynamic components inside the
frame, and (2) how to access them — by index (list) or by a key (map).

Each dynamic element is addressed by a **fully-qualified component name** formed from the field's
path prefix plus its key or index — e.g. `processes.htop.pid` for a map keyed by name, or
`processes.0.pid` for a list. The ComponentId is the hash of that full name, so even deeply nested
dynamic data still yields ordinary, fully-qualified components that metor-db and UIs can store and
query without special cases.

The data lives in a padded trailer after the fixed portion of the frame (a concept cribbed from
rkyv / flatbuffers). The dynamic field holds an offset and length into that trailer. With purely
fixed data the trailer is zero length and unused; with dynamic data the reader jumps into it.
Describing this in a VTable requires new list/map ops so that other systems — and metor-db — can
interpret the trailer rather than treat it as an opaque blob.

## Telemetry Downlink

Flight software is only useful if its state can be observed off-board. metor_fsw provides a
dedicated **telemetry system** that taps the outputs of other systems and streams them out of the
process in metor-proto's wire format — the same format metor-db ingests. The downlink and the
database are therefore two ends of one protocol: a frame produced by a system is streamed out as a
table, and metor-db stores it with no extra translation. This is the same VTable symmetry that runs
through the rest of the design, extended to the network edge.

Like any system the telemetry streamer is invoked by the coordinator, and it can in principle run
at any point in the cycle. In practice it is registered last so it captures an end-of-cycle
snapshot of every output: each cycle it reads the latest frame from each tapped output buffer and
emits it.

It has two modes:
1. **All telemetry** — it taps every output buffer in the running graph (every system's output
   frames, plus the per-system health and log frames). No wiring is required; the coordinator
   binds it to the full set of outputs.
2. **Subset** — it taps a named subset of frames or instances, for a constrained downlink budget.

Because the streamer needs to enumerate and read every output rather than a fixed, compile-time set
of typed inputs, the coordinator gives it a **tap** over the output ring buffers it owns, together
with each frame's VTable and the producing system's instance name. Each distinct VTable is
announced once; thereafter each cycle pushes that frame's latest bytes as a table packet
referencing the announced VTable. The component paths are prefixed with the producing instance's
name (so `processes.htop.pid` from the `procmon` instance is downlinked as
`procmon.processes.htop.pid`) — this is where the per-instance namespacing the wiring records is
finally applied, so two instances of one system type never collide on the wire.

The transport is pluggable. A TCP stream — to a ground link or a co-located metor-db — is the first
target; a shared-memory queue is a natural local alternative. Either way the bytes on the wire are
identical and a consumer needs only the announced VTables to parse them.
