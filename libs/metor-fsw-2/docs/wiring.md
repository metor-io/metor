# Target wiring

Target wiring says what runs and how data moves between systems. It does not
run system code. The wiring layer turns a target into a checked coordinator.

Wiring connects separate systems into one target. It chooses which systems
run, how each one is set up, and which outputs feed which inputs. The same
system code can then serve several targets with different layouts.

The main flow is:

```text
target.py ─┐
            ├─> Wiring IR ─> validate ─> resolve ─> coordinator
Rust builder┘
```

Both front ends produce the same `Wiring` value. The resolver applies the same
checks to both.

## Python targets

The CLI runs a `target.py` with CPython 3.10 or later. The `metor_config`
package records each call and writes IR as JSON. Python does not hold memory
used by the running coordinator.

This example declares two pack systems, one process boundary, a feedback edge,
and a link server:

```python
from metor_config import Downlink, Target, TcpServer
from adcs_pack import Nav, Plant

m = Target(cycle_rate=100.0, sim_dt=0.01, namespace="sat1")

link = m.state("link", TcpServer(addr="127.0.0.1:2240"))

plant = m.add("plant", Plant(seed=4), process=True)
nav = m.add("nav", Nav(gain=0.2))
downlink = m.add("downlink", Downlink(link))

m.connect(plant.sensors, nav.sensors)
m.connect(nav.correction, plant.correction, delayed=True)
```

Pack modules such as `adcs_pack` come from pack manifests. Their classes carry
typed params and ports. A class call records a spec. It does not load the Rust
system.

`m.connect` adds a frame edge. `delayed=True` makes the value arrive one cycle
late and lets the edge close a feedback loop.

`m.route` adds a message edge:

```python
m.route(uplink, mode, msg="SequenceCommand")
```

Message edges can fan in and out. They do not take part in frame cycle checks.
They cannot use a delay.

## Rust targets

`WiringBuilder` builds the same IR without Python:

```rust
use metor_fsw_2::{ClockSpec, WiringBuilder};

let wiring = WiringBuilder::new()
    .coordinator(100.0, ClockSpec::Simulated { dt_secs: 0.01 })
    .artifact("adcs", "adcs-pack", "adcs_pack")
    .system("plant").ty("Plant").from_artifact("adcs").end()
    .system("nav").ty("Nav").from_artifact("adcs").end()
    .connect("plant", "sensors", "nav", "sensors")
    .connect_delayed("nav", "correction", "plant", "correction")
    .serve("127.0.0.1:2240".parse().unwrap())
    .build();
```

The builder checks local mistakes as calls finish. It panics on faults such as
a repeated name or an unknown artifact. Serialized IR gets the same checks at
resolve time and returns a `LoadError`.

## The wiring IR

Both examples produce `Wiring`, a plain data value that serde can read and
write. The current IR version is 6. A host rejects another version during
resolve.

The top-level fields are:

- `coordinator`: cycle rate, clock, ring depth, and an optional name prefix
- `artifacts`: loadable pack libraries
- `states`: state shared by entries in one static pack
- `systems`: fixed system instances
- `slots`: places that can load an allowed entry at run time
- `edges`: frame and message links
- `scopes`: block names and parent links recorded by the Python front end

Artifact paths do not form part of target identity. `path_stripped()` removes
build paths and prebuilt roots before a bundle stores or reports the IR.

## Systems and artifacts

A system with no artifact is static. Its `type` names an entry in the host
`Registry`.

A system with an artifact loads an entry from that pack. The artifact holds an
id, Cargo package name, bare library stem, and build data. The stem stays the
same on all targets. Provisioning turns it into `libfoo.so`, `libfoo.dylib`, or
`foo.dll`.

If a pack has one entry, a loaded system may omit its type. A pack with two or
more entries needs a type.

`process=True` runs a loaded system in a worker. Static systems cannot use this
mode because the worker needs a pack library it can open.

## Params

The IR has three param forms:

- `None` uses declared defaults, or an empty value when no defaults exist.
- `Value` holds JSON-shaped data from Python or the Rust value builder.
- `Postcard` holds bytes made by the typed Rust builder.

Static systems decode a value into their Rust params type with serde.

Loaded systems expose a params schema in the pack manifest. The host checks a
value against that schema and writes canonical postcard bytes. It does not link
the params type.

Postcard params work only for loaded systems and slot occupants.

## Shared states

A static pack can declare a shared state type with `Pack::shared_state`. The
target must declare one state value of that type before attached systems can
run.

`m.state(name, spec)` returns a handle. A system attaches by taking that handle
in its constructor, which records the state's name on the system's `attach`
field. The system's *type* fixes which shared type it can bind; the handle
picks which instance. A shared-state system given no handle is a resolve-time
error, and a plain system given one is rejected too.

The resolver creates all states before it creates systems, so a system's
`attach` resolves to a constructed instance. Each state name and type must be
unique, and each declared state must serve at least one system.

The built-in network link uses this feature. A `TcpServer` state owns the
listener; the `Downlink` and `Uplink` systems attach to it by name.

```python
from metor_config import Downlink, TcpServer, Uplink

link = m.state("link", TcpServer(addr="0.0.0.0:2240", name="sat1"))
downlink = m.add("downlink", Downlink(link))
uplink = m.add("uplink", Uplink(link, msgs=["SequenceCommand"]))
```

The CLI `run --serve ADDR` changes the address of an existing `TcpServer`. If
the target has no server, it adds one and adds an all-output downlink.

## Slots

A slot has fixed input and output names. Every allowed occupant must match that
port contract.

An allowed item names a pack entry and may name its artifact. If it omits the
artifact, resolve searches all artifacts and requires one match.

Slot entries must be reloadable. An entry built with moved-in state, or attached
to pack-shared state, cannot fill a slot.

The initial occupant may start loaded or running. A process slot runs all its
occupants in workers. One load cannot change the slot from in-process to
out-of-process.

## Validation and resolve order

Resolve first checks facts held in the IR:

- IR version
- scope indexes
- unique instance, artifact, and state names
- valid artifact links
- valid system and slot forms
- a nonempty allowed set for each slot

It then checks files, registries, manifests, params, and port contracts.

The build order matters. States come first. Fixed systems come next. Slots then
join the instance table. Receive-all systems run last among user systems. Edges
join the graph after every endpoint exists.

The graph build checks port types, frame subsets, ring sizes, reader counts,
step order, and feedback cycles. A delayed frame edge does not take part in the
cycle check.

## Runtime manifest

Resolve stores the path-free IR in a `WiringManifest` telemetry value. Ground
tools can use it to show the live system graph.

The optional coordinator namespace only changes reported component names and
ids. Wiring still resolves against the bare instance names.
