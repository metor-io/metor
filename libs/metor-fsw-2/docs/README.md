# Metor FSW design

Metor FSW is a framework for building modular flight software. A mission is a
checked graph of systems. Each system has typed input and output ports. The
coordinator owns the graph, its ring buffers, and the cycle clock.

It lets a team build a mission from small parts that can be developed, tested,
and reused on their own. A sensor driver, estimator, and controller each have
one clear job. The mission then connects those parts and checks that they agree
about the data they exchange.

Mission code can link systems into the host, load them from a pack library, or
run them in a worker process. All three forms use the same port descriptions
and wiring checks.

## One mission cycle

The resolver checks the mission before it runs. It loads each system
description, checks each edge, sizes the rings, and binds the ports.

The coordinator then runs cyclic systems in mission order. One timestamp is
used for the full cycle.

For example, a mission may run these systems in this order:

```text
imu -> navigation -> control -> downlink
```

The `imu` system writes a frame. `navigation` reads it in the same cycle and
writes a new frame. `control` reads that result. The downlink runs in the
receive-all tail and drains all telemetered outputs.

A frame edge that points back in this order must be marked as delayed. It then
reads data from an earlier cycle. Message edges do not take part in this check
because they carry ordered events, not same-cycle state.

## Data types

Frames hold sampled state. A frame is a `#[repr(C)]` Rust struct with one
timestamp. Its memory bytes are also its ring and wire bytes.

Messages hold commands and events. A message record starts with its packet id,
then stores postcard bytes. Message inputs may read from more than one producer.

Each output has one ring writer. Inputs hold read views. A writer never
replaces bytes that a reader still holds. A full ring makes a write fail. A
`publish` helper may drop the new record and report the loss through health.

See [Frames](frames.md) and [Ports, messages, and rings](messages.md).

## System forms

Use a function system for most cyclic work. Its function args declare its
ports.

Use `Pack::task` for an async function that must advance on the mission clock.
The driver polls it once per cycle. Sequences use this form.

Use a `CyclicSystem` struct when state and port bundles need named types. Use an
`AsyncSystem` struct for a free-running task that waits on I/O or host time.
The coordinator does not poll that task once per cycle.

See [Systems](system.md) and [Coordinator](coordinator.md).

## Mission input

A `mission.py` file and the Rust `WiringBuilder` both produce wiring IR v4.
The IR holds shared states, systems, runtime slots, edges, artifacts, and
coordinator settings.

The resolver applies the same checks to both inputs. It also writes a stripped
copy of the IR to the wiring manifest output for tools that inspect a live
mission.

See [Mission wiring](wiring.md).

## Code loading

A pack lists the systems that one crate exports. The host may register that
pack at build time or load its library through ABI v10.

A process system uses the same pack library in a worker. The host and worker
share mmap ring files. A small control block sends one cycle timestamp to the
worker and waits for its result within a set time.

See [Packs and loading](packs.md), [Process systems](process-systems.md), and
[Packaging](packaging.md). See the [command-line guide](cli.md) for build, run,
check, and publish commands.

## Built-in services

The link service accepts TCP clients, sends schema and identity data, then
sends live telemetry. Its uplink side turns valid message packets into normal
message outputs. Local mDNS can announce the server address.

The alarm system reads telemetered frame values and sends alarm messages. A
runtime slot can load, start, stop, or replace one allowed pack entry.

See [Telemetry](telemetry.md), [Alarms](alarms.md), and
[Sequences and slots](sequences-slots.md).

## Where contracts live

These docs explain the design that the current code implements. Public Rust
items state exact API rules. Module docs state rules that span several items.

Plans and review notes do not define current behavior. Check the code and its
tests when a design detail is not covered here.
