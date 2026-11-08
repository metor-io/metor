# Muxes: splitting and combining frames

A mux builds new frames from fields of existing frames, declared in a
target's Python file.

- Split: one input frame, several output frames, each a subset of its fields.
- Combine: several input frames, one output frame.

Both are the built-in system `fsw.mux`. It reads fixed frames only; messages
and frames holding a `FrameMap` are refused.

## Python API

```python
from metor_config import Mux

s = fsw.add("split", Mux(
    imu=Imu(gyro_b=plant.sensors.gyro_b, accel=plant.sensors.accel),
    css=Css(css=plant.sensors.css),
))

c = fsw.add("est_body", Mux(
    body=Body(q_b_eci=nav.attitude_estimate.q_hat_b_eci, pos_eci=gps.pos_eci),
    on=nav.attitude_estimate,
    max_age=0.5,
))
```

Each keyword names an output port and gives an instance of a frame record
some registered port declares. Stubgen emits, per frame, a record class
whose `__init__` takes one typed keyword per top-level field, and a port
class with one property per field:

```python
class Sensors(Record):
    _fields = {"gyro_b": "f64[3]", "css": "f64[6]", ..}
    def __init__(self, *, gyro_b: Field[Literal["f64[3]"]] | None = None, ..): ..

class SensorsPort(OutPort[Sensors]):
    @property
    def gyro_b(self) -> Field[Literal["f64[3]"]]: ..
```

System outputs are typed as their port class, so pyright checks field names
and signatures. `Record.__init__` and `OutPort.field` raise `ConfigError` on
the same checks at runtime.

## Semantics

- A destination path takes a source `(input, path)`. Type and shape must
  match exactly. A path naming a nested struct maps each leaf under it by
  relative name. Unmapped fields stay zero.
- A path may name one element of a shaped leaf, such as `v.5` or `p.1.2`.
  An element of `ty[d0, d1, ..]` is `ty[d1, ..]`; each index drops one
  dimension, so `p.1.2` of `f64[6,6]` is `f64`. Elements use the separator
  that struct arrays use, so a path names the same element whether a plain
  `[T; N]` is split into leaves or a `FrameArray` or tensor is one leaf.
- Inputs that are not the trigger are latest-only: each record overwrites
  its fields in the output's staging.
- An output never emits before every input it maps has delivered once.
- `max_age`: an output does not emit while an input it maps is older than
  `max_age`. Age is the cycle time minus the input's last stamp, or minus the
  cycle it arrived in when the frame has no stamp.

| `on=` | Emits | Stamp default |
|---|---|---|
| a port | once per record drained from that input | that record's stamp |
| `"any"` | once per cycle in which a mapped input has a new record | cycle time |
| `"all"` | once every mapped input has a record newer than the last emit | cycle time |
| `"cycle"` | every cycle | cycle time |

A combine requires `on=`; a split defaults to its one input.
`timestamp=<port>` takes that input's latest stamp, `timestamp="now"` the
cycle time.

Each cycle drains the inputs that trigger no output first, then the trigger
inputs in order. An output triggered by one port that maps another trigger
port sees that port's records up to the last drain.

## Lowering

```rust
#[system]
fn mux(#[state] s: &mut Mux, now: Timestamp, inputs: &mut DynInputs,
       outputs: &mut DynOutputs) { .. }
```

The params list the input ports and, per output, its port, trigger, stamp
source, `max_age`, and field map by name. The ctor takes a trailing
`&Bound`, the instance's bound ports and the record registry. At make it
resolves each path against the registry's schema for the port's record into
`(input, src, dst, len)` byte copies, checks that the bound port's layout is
the registry's, and allocates one staging buffer per output. A mux takes at
most 64 inputs, one bit each in its masks.

A path with no leaf under it falls back to an element: the leaf that is a
proper prefix of the path, with the rest parsed as canonical decimal indices,
at most one per dimension. The element lowers to one byte copy at
`offset + flat(indices) * stride`. An index past its dimension fails make
with `MuxError::Index`; any other unresolved path is `UnknownField`. Make
checks bounds itself, since a hand-written config skips Python.

A frame output is re-rooted at `{ns}.{id}.{port}`, so two systems emitting
one record announce distinct components.

## Allocation and failure

Everything is allocated at make. Steady state borrows input records from the
ring, copies into fixed staging, and writes the staging. A full output ring
counts toward `publish_dropped`. A record shorter than its frame is skipped
with a fault. A record the ring cannot read is skipped; a cycle with any
logs one `mux` fault.
