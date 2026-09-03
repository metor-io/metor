# Python systems

A target file can define a cyclic system in Python. The function is compiled
at build time into a WebAssembly pack artifact, and the vehicle runs it like
any other pack entry: it has typed ports, it steps at its position in the
cycle, its output is telemetry, and a fault in it stops only that system.

Python systems suit small derived quantities and glue between native systems.
They let a target author add a norm, a threshold, or a unit change without a
Rust crate, and they run under the same fuel and memory limits as every wasm
occupant (see [WebAssembly packs](wasm.md)).

## Declare a system

`@system` marks a function as a system. Its parameters are inputs and its
return value is the output. A parameter annotated with a `Frame` subclass reads
that frame; a bare name is bound by path in the decorator.

```python
from metor_config import Frame, Target, f64, node, system

class Est(Frame):
    norm: f64

@system("plant.sensors.gyro_b")
@node(x=980, y=40)
def gyro_norm(gyro_b) -> f64:
    return (gyro_b @ gyro_b) ** 0.5

@system
def scaled(est: Est) -> f64:
    return est.norm * 2.0

m = Target(cycle_rate=120.0, sim_dt=1 / 120)
m.add("gyro_norm", gyro_norm)
m.add("scaled", scaled)
```

The decorator only captures the declaration, with its source and line. The
system flies when `Target.add` registers it, and it steps at that position in
the system list, interleaved with native systems in the order the adds were
written. Adding the same handle twice is an error. A declared function that is
never added is a warning at record time naming the file and line.

`@node(x=, y=)` records a canvas position in the IR. It changes nothing on the
vehicle.

A `Frame` subclass declares a frame with typed fields. Scalars are `f64`; a
fixed-size vector is `Tensor[f64, 3]`. A returned frame is the system's output
frame; a returned scalar is a one-field frame named after the function. A
`State` subclass declares fields that persist between steps.

## Run rule

A system with inputs fires when its driving input has a new record since its
last step. The driving input is the first parameter unless `on=` names
another. Other inputs read their newest record. A step with no new driving
record publishes nothing. A system whose input has never published skips
until it does.

`bind=` maps a parameter to a path when the parameter name should differ from
the bound component, for example `@system(bind={"w": "plant.sensors.gyro_b"})`.

A system with no inputs takes `rate=`. It fires every `cycle_rate / rate`
steps, so the rate must divide the target's cycle rate; any other value fails
the build.

```python
@system(rate=30.0)
def beat() -> Beat:
    return Beat(v=2.5)
```

A Python system that consumes another Python system's frame is wired by the
frame type. A Python system that reads a native output is wired by the path in
its decorator. Both become ordinary edges when the target resolves. The usual
ordering rule applies: a consumer must step after its producer, or the edge
must be marked `delayed=True`.

## Build and load

`metor-fsw build` compiles the target's captured program into one artifact,
`<id>.wasm`, next to the built native libraries, with a `.manifest` sidecar
like any pack. The compiler resolves every path binding against the other
artifacts' pack manifests, so a path that names no component, a type that does
not match, or a source system whose rate does not divide the cycle rate is a
build error that points at the `target.py` line. Outputs of systems the host
registers statically are not visible to the compiler, so a Python system
cannot bind to them.

The compiled module speaks the pack ABI. It bakes its own pack manifest, one
entry per added system, each with a full descriptor: the input ports its
bindings need, the output frame, and the log tail every entry carries. Nothing
on the vehicle can tell a compiled Python pack from a Rust one. Compilation is
deterministic: the same source and the same manifests produce the same bytes.

`metor-fsw run` and bundles treat the module like a located library. A bundle
carries the module unchanged for every target triple.

## On the vehicle

Resolve opens the module under the interpreter, reads its manifest, and binds
each entry as a wired cyclic system with its own interpreter instance. The
run rule and the rate counter live inside the module, so the host driver stays
generic.

A trap, an exhausted fuel grant, or a corrupt ring record stops that system.
The coordinator reports it as stopped in its own log and status, keeps
draining its inputs so producers never back up, and keeps cycling every other
system. A stopped Python system is not restarted.

A Python entry can also be the occupant of a runtime slot. Name it in the
slot's `allow` set as you would a native entry.

Random numbers come from a per-boot seed the host injects at resolve, since a
module imports nothing and has no entropy of its own.

## Limits

- Only decorated functions run on the vehicle. Bare expressions and top-level
  bindings are panel features and evaluate on the ground.
- A Python system cannot be a process system.
- Stages such as resampling are not available on the vehicle.
- Replacing a system's source at run time is not supported. Rebuild and
  restart the target.
- State does not carry across a slot occupant swap.
