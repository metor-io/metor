# Nesting: frame fields and system params

Frames and system params are both trees of named slots. A frame's leaves are
components and a system's leaves are ports. Both name their leaves by dotted
path, under the same rules.

- `#[derive(Fields)]`: a reusable struct of components, nested inside frames.
- `[P; N]: Param`: an array of params, such as `[Input<Fss>; 8]`.
- `#[derive(SystemParams)]`: a struct of params, passed to a system as one
  argument.

## Path rules

| Construct | Path |
|---|---|
| field or fn argument `x: T` | adds segment `x`; a `SystemParams` struct argument adds none |
| `x: [T; N]` | `x.0` … `x.{N-1}` |
| `#[fsw(flatten)] x: T` | adds no segment; `T` must be a `Fields` or `SystemParams` struct |
| `#[fsw(rename = "y")] x: T` | adds segment `y` |
| `_x: T` | skipped; `#[fsw(skip = false)]` opts back in |
| `#[fsw(skip)] x: T` | skipped |

`Fields`, `Frame`, and `SystemParams` read these attributes with one parser.

A frame's leaf path is `{port_path}.{field path}`. A param's port name is its
path, so its port path is `{namespace}.{id}.{param path}`, and a frame on that
port names its leaves under it: `ns.adcs.fss.3.sun_vec`.

Errors:

- Two leaves of a frame at one path fail the derive when it can see them, and
  otherwise the build, as `BuildError::DuplicateLeaf` (M0338).
- Two ports of one name fail the build as `BuildError::DuplicateInput` or
  `DuplicateOutput`. Two `Cap`s or `Rng`s of one name fail the definition as
  `DefError::DuplicateParam`.
- An empty port or param name fails the definition as `DefError::EmptyName`.
  It comes from flattening a leaf the derive cannot see through, or from a
  `FnSignature` with fewer `NAMES` than arguments.
- A `Cap` under a field or an array fails the definition as
  `DefError::NestedCap`. Each `Cap` is a keyword of the system's Python class,
  so its name must be one segment.

## Frame fields

`#[derive(Fields)]` expands to `AsVTable` and `Metadatatize`. The struct is a
group of components with no name, id, or timestamp of its own.
`#[derive(Frame)]` is `Fields` plus `Payload`, `Record`, `Frame`, a timestamp,
and the frame-id tag.

```rust
#[derive(Fields, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct Inner { pub x: f64, pub y: f64 }

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct Nested {
    #[fsw(timestamp)]
    pub timestamp: Timestamp,
    pub inner: Inner,
    pub fss: [Inner; 8],
    pub z: f64,
}
```

Both derives read `#[fsw(..)]`, and `Frame` also accepts `#[frame(..)]`.
Struct attributes: `name`, `depth`, `max_len` (`Frame` only), `group`. Field
attributes: `timestamp` (`Frame` only), `flatten`, `rename`, `skip`, `unit`,
`metadata(..)`.

A frame used as a field of another frame is a plain group. Its name, id, and
timestamp are dropped, and the outer frame's apply to every leaf. For this,
`AsVTable::vtable_fields` applies neither a timestamp nor a frame id.
`RecordSchema::frame` applies both once, at the root: `with_timestamp` from
`Frame::STAMP`, then `with_frame`.

Frames do not implement `Componentize` or `Decomponentize`. The ground decodes
frames through the vtable, nothing called the derived impls, and the derived
`Componentize` named nested leaves without their path.

## Params

### The wake sink is a trait parameter

```rust
pub trait Param<W: InputWake> {
    type In;
    type Out;
    type Item<'a> where Self: 'a;
    ..
}
```

`SystemFn` and `SequenceFn` require `Params: Param<NoWake>`, and
`AsyncSystemFn` requires `Params: Param<Notifier>`.

| Param | Implements |
|---|---|
| `Input<T, W>`, `DynInputs<W>`, `Abort<W>`, `PublishInputs<W>` | `Param<W>` |
| `Output<T>`, `DynOutputs`, `SubscribeOutputs`, `Timestamp`, `Log`, `Records` | `Param<W>` for every `W` |
| `Sample<T>`, `Rng`, `Alarms`, `Cap<K>`, `Seq<'static>` | `Param<NoWake>` |

A param on the wrong kind of system is a compile error rather than a
definition error. An async fn writes its inputs `Input<T, Notifier>`.

### Port names

`Param::append_defs` and `Param::bind_out` take the param's `ParamPath`, a
borrowed chain of segments that allocates only when it joins two or more of
them into a name. `bind_in` stays positional: bindings are handed out in
parameter order, and arrays and structs take theirs in field order.

`FnSignature::NAMES` holds the fn's argument names. The tuple of the fn's
params gives argument `i` the path `NAMES[i]`, or no segment when the
argument's `Param::FLATTEN` is set, as it is for a `SystemParams` struct. A
tuple is only ever `FnSignature::Params`; `#[system]` and `#[sequence]`
refuse a tuple argument, whose elements would share one name.

### `[P; N]: Param`

```rust
#[system]
fn sun(fss: [&mut Input<Fss>; 8], est: &mut Output<SunVec>) { .. }
```

Element `i` is named `{path}.{i}`. `#[system]` keys the argument by its
element's key, so `[&mut Input<Fss>; 8]` is keyed `[Input<Fss>; 8]`.

An element must implement `ArrayParam`: `Input`, `Sample`, `Output`, an array
of those, or a `SystemParams` struct. Any other element type is a compile
error.

### `#[derive(SystemParams)]`

A struct whose fields are params. It is the view the fn receives:

```rust
#[derive(SystemParams)]
struct Sensors<'a> {
    fss: [&'a mut Input<Fss>; 8],
    gyro: &'a Sample<Gyro>,
}

#[derive(SystemParams)]
struct WheelIo<'a> {
    speed: &'a mut Input<WheelSpeed>,
    cmd: &'a mut Output<WheelCmd>,
}

#[derive(SystemParams)]
struct Actuators<'a> {
    wheels: [WheelIo<'a>; 4],
    mtq: &'a mut Output<MtqCmd>,
}

#[system]
fn adcs(#[state] s: &mut Adcs, now: Timestamp, sensors: Sensors<'_>, act: Actuators<'_>) { .. }
```

Ports: `fss.0` … `fss.7`, `gyro`, `wheels.0.speed`, `wheels.0.cmd`, …
`wheels.3.cmd`, `mtq`. A struct passed as a fn argument adds no segment; a
struct held by a field keeps the field's name, as `wheels` does.

The derive implements `Param<NoWake> for Sensors<'static>`: `In` and `Out` are
the tuples of the fields' `In`s and `Out`s, and `Item<'a>` is `Sensors<'a>`,
built from the fields' items. `#[system_params(async)]` implements
`Param<Notifier>` instead, and its inputs are written `Input<T, Notifier>`.

The derive refuses a `Seq`, `Timestamp`, or `Abort` field, even inside an
array: these stay fn arguments, where `#[system]` and `#[sequence]` check
them. It also refuses a tuple field, and `flatten` on a field whose type is a
single param or an array.

## Config and stubs

Port names stay flat dotted strings in `PortDef`, the pack schema, and the
config wire format. Stubgen rebuilds the structure from the names, so the
schema carries nothing new.

Inputs, as `__init__` keywords:

- A leaf is `Sources[T]`, which allows fan-in.
- A run of segments `0` … `N-1` is `Sequence[Sources[T]]`. The config raises
  `ConfigError` when its length is not `N`.
- A named group is a generated class with one keyword per child, nested on the
  system's class and named after the group's path in PascalCase:
  `wheels.N.*` gives `Adcs.Wheels`.

Outputs, as attributes:

- A leaf is `OutPort[T]`.
- A run of indices is `tuple[OutPort[T], ...]` of length `N`.
- A named group is a generated class with one attribute per child, named like
  an input group with an `Out` suffix.

```python
adcs = fsw.add("adcs", Adcs(
    fss=[f.out for f in fss],
    gyro=imu.gyro,
    wheels=[Adcs.Wheels(speed=e.speed) for e in encoders],
))
fsw.add("wheel_2", WheelSim(cmd=adcs.wheels[2].cmd))
```

A group class's constructor raises `ConfigError` on an unknown keyword or a
mismatched record, and the system raises it on a group of another class.
`None` leaves a leaf, list, or group unwired. Two groups share a class only
when they render the same class. Otherwise, as for a segment that is a Python
keyword or `self`, stubgen fails with M0409. The config flattens groups and
lists back to dotted port names before it writes the target.
