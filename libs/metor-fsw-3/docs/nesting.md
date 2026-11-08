# Nesting: frame fields and system params

Frames and system params are both trees of named slots. A frame's leaves are
components; a system's leaves are ports. Both name their leaves by dotted
path, and both follow the same path rules.

Three pieces:

- `#[derive(Fields)]`: a reusable struct of components, nested inside frames.
- `[P; N]: Param`: an array of params, such as `[Input<Fss>; 8]`.
- `#[derive(SystemParams)]`: a struct of params, passed to a system as one argument.

## Problem

A struct reused inside a frame needs the four `metor_component` derives, the
four zerocopy derives, the `#[fsw]` attribute spelling, and `#[frame(nest)]` on
the parent's field:

```rust
#[derive(metor_component::AsVTable, metor_component::Metadatatize,
         metor_component::Componentize, metor_component::Decomponentize,
         IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct Inner { pub x: f64, pub y: f64 }

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
pub struct Nested {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    #[frame(nest)]
    pub inner: Inner,
    pub z: f64,
}
```

A system takes one port per fn argument. Port names are `&'static str`, so a
system cannot take eight sun sensors as `[Input<Fss>; 8]`, and cannot group
related ports into a struct.

## Path rules

One set of rules for frame fields and for params.

| Construct | Path |
|---|---|
| field or fn argument `x: T` | adds segment `x`; a `SystemParams` struct argument adds none |
| `x: [T; N]` | `x.0` … `x.{N-1}` |
| `#[fsw(flatten)] x: T` | adds no segment |
| `#[fsw(rename = "y")] x: T` | adds segment `y` |
| `_x: T` | skipped; `#[fsw(skip = false)]` opts back in |
| `#[fsw(skip)] x: T` | skipped |

Two leaves with the same path are an error. Frames report it at compile time
where the derive can see it, and otherwise as `BuildError::DuplicateLeaf`
when a port's frame holds one component id twice. Params report it as
`BuildError::DuplicateInput` or `DuplicateOutput`, and two `Cap`s or `Rng`s
of one name as `DefError::DuplicateParam`. An empty port or param name, from
flattening a leaf or a `FnSignature` with too few `NAMES`, is
`DefError::EmptyName`.

A frame's leaf path is `{port_path}.{field path}`. A param's port name is its
path, so its port path is `{namespace}.{id}.{param path}`, and a frame on that
port names its leaves under it: `ns.adcs.fss.3.sun_vec`.

`[T; N]: AsVTable` already names elements `x.0` … `x.{N-1}`.

## Frame fields

### `#[derive(Fields)]`

Expands to `AsVTable` and `Metadatatize`. The struct is a group of components
with no name, id, or timestamp of its own.

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

`#[derive(Frame)]` is `Fields` plus `Payload`, `Record`, `Frame`, a timestamp,
and the frame-id tag.

### Attributes

Both derives read `#[fsw(..)]`. `#[frame(..)]` stays accepted on `Frame` as an
alias. Struct-level: `name`, `depth`, `max_len` (`Frame` only), `group`.
Field-level: `timestamp` (`Frame` only), `flatten`, `rename`, `skip`, `unit`,
`metadata(..)`.

### Removing `nest`, `Componentize`, and `Decomponentize`

`nest` only changes the derived `Componentize`/`Decomponentize`. `AsVTable` and
`Metadatatize` always recurse through the field's type.

The derived `Componentize` disagrees with the vtable: it recurses without a
path prefix, so `Nested` emits `x`, `y`, `z` while its vtable announces
`nested.inner.x`. Nothing in the workspace calls the derived impls. The
`Frame` trait only requires them as bounds.

`Frame` drops the `Componentize + Decomponentize` bounds, the derives stop
emitting them, and `nest` is removed. The ground decodes frames through the
vtable. The standalone `metor_component` derives stay for other users.

### A frame used as a field

A `Frame` placed inside another frame is treated as a `Fields` group. Its
name, id, and timestamp are dropped. The outer frame's timestamp and frame id
apply to every leaf.

This needs `AsVTable::vtable_fields` to stop applying `with_timestamp` and
`with_frame` itself. The `Frame` derive applies them in `Record::schema_with_root`
instead, once, at the root.

## Params

### The wake sink is a trait parameter

`Param` takes the wake sink as a trait parameter instead of on its associated
types:

```rust
pub trait Param<W: InputWake> {
    type In;
    type Out;
    type Item<'a> where Self: 'a;
    ..
}
```

`SystemFn` and `#[sequence]` fns require `Params: Param<NoWake>`.
`AsyncSystemFn` requires `Params: Param<Notifier>`.

| Param | Implements |
|---|---|
| `Input<T, W>`, `DynInputs<W>`, `Abort<W>`, `PublishInputs<W>` | `Param<W>` |
| `Output<T>`, `DynOutputs`, `SubscribeOutputs`, `Timestamp`, `Log`, `Records` | `Param<W>` for every `W` |
| `Sample<T>`, `Rng`, `Alarms`, `Cap<K>`, `Seq<'static>` | `Param<NoWake>` |

A param on the wrong kind of system is a compile error. This replaces
`DefError::SampleOnAsync`, `RngOnAsync`, `AlarmsOnAsync`, and `IoOnAsync`.

`Input<T, D>` today implements `Param` for any `D` and hands the fn an
`Input<T, W>`. With the trait parameter, `Input<T, W>` implements only
`Param<W>`, so an async fn must write `Input<T, Notifier>`, as it does today.

### Port names

`Param::append_defs` takes the path prefix of the param instead of popping
names from a flat iterator:

```rust
fn append_defs(at: &ParamPath<'_>, cx: &DefCx<'_>, defs: &mut Defs) -> Result<(), DefError>;
```

`ParamPath` composes segments without allocating until a port name is built.
`Input::def` and `Output::def` take `Cow<'static, str>`. `PortDef::name` is
already a `Cow`.

`bind_out` reads names the same way. `bind_in` stays positional: bindings are
handed out in parameter order, and arrays and structs take theirs in field
order.

`FnSignature::NAMES` stays: it holds the fn's argument names, each the first
segment of that argument's paths unless the argument is flattened.

This fixes the tuple impl, which today calls `param_name` once per element and
so consumes more names than `#[system]` emits when a fn argument is a tuple.
A tuple adds no segment of its own and is only used for `FnSignature::Params`;
a tuple fn argument is rejected by `#[system]`. Use a `SystemParams` struct.

### `[P; N]: Param`

```rust
impl<W: InputWake, P: Param<W>, const N: usize> Param<W> for [P; N] {
    type In = [P::In; N];
    type Out = [P::Out; N];
    type Item<'a> = [P::Item<'a>; N] where Self: 'a;
}
```

Element `i` is named `{path}.{i}`. `bind_in` and `bind_out` build the arrays at
make time. `get` and `refresh` visit elements in order.

A system takes the items:

```rust
#[system]
fn sun(fss: [&mut Input<Fss>; 8], est: &mut Output<SunVec>) { .. }
```

`#[system]` maps the argument type to its `Param` key by stripping references
inside arrays: `[&mut Input<Fss>; 8]` → `[Input<Fss>; 8]`.

Arrays of `Sample`, `Output`, and `SystemParams` structs work the same way. Arrays of
`Timestamp`, `Seq`, `Log`, `Rng`, `Alarms`, `DynInputs`, and `DynOutputs` are
rejected by `#[system]`.

### `#[derive(SystemParams)]`

A struct whose fields are params. The user writes the view the fn receives:

```rust
#[derive(SystemParams)]
struct Sensors<'a> {
    fss: [&'a mut Input<Fss>; 8],
    gyro: &'a Sample<Gyro>,
    mag: &'a mut Input<Mag>,
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

Ports: `fss.0` … `fss.7`, `gyro`, `mag`, `wheels.0.speed`, `wheels.0.cmd`, …
`wheels.3.cmd`, `mtq`.

A `SystemParams` struct passed as a fn argument is flattened: the argument name adds
no segment. A `SystemParams` struct held by a field of another `SystemParams` struct keeps
its field name, as `wheels` does above. Two flattened arguments with a shared
port name fail the build with `BuildError::DuplicateInput` or
`DuplicateOutput`, which already check every system's ports.

The derive maps each field type to its `Param` key with the same rule as
`#[system]`, and implements `Param<NoWake> for Sensors<'static>`:

- `In` is the tuple of the fields' `In`s, and `Out` the tuple of their `Out`s.
- `Item<'a>` is `Sensors<'a>`, built from the fields' items.
- `append_defs` and `bind_out` walk the fields, each adding its segment.

`Seq<'static>` already uses this pattern.

`#[system_params(async)]` implements `Param<Notifier>` instead. Its inputs are written
`Input<T, Notifier>`:

```rust
#[derive(SystemParams)]
#[system_params(async)]
struct Feeds<'a> {
    fss: [&'a mut Input<Fss, Notifier>; 8],
    est: &'a mut Output<SunVec>,
}
```

A field that does not implement the struct's `Param<W>`, such as a `Sample` in
an async struct, is a compile error. The derive also refuses a `Seq`,
`Timestamp`, or `Abort` field, even inside an array: these stay fn arguments,
where `#[system]` and `#[sequence]` check them. It refuses a tuple field, and
`flatten` on a field it can tell is a leaf or an array.

## Config and stubs

Port names stay flat dotted strings in `PortDef`, the pack schema, and the
config wire format. Stubgen builds the structure back from the names, so the
schema carries nothing new.

Inputs, as `__init__` keywords:

- A leaf stays `Sources[T]`, which already allows fan-in.
- A run of segments `0` … `N-1` becomes `Sequence[Sources[T]]`. The config
  raises `ConfigError` when its length is not `N`.
- A named group becomes a generated class with one keyword per child, nested
  on the system's class and named after the group's path in PascalCase:
  `wheels.N.*` gives `Adcs.Wheels`.

Outputs, as attributes:

- A leaf stays `OutPort[T]`.
- A run of indices becomes `tuple[OutPort[T], ...]`, of length `N`.
- A named group becomes a generated class with one `OutPort` property per
  child, named after its path in PascalCase with an `Out` suffix.

```python
adcs = fsw.add("adcs", Adcs(
    fss=[f.out for f in fss],
    gyro=imu.gyro,
    mag=mag.out,
    wheels=[Adcs.Wheels(speed=e.speed) for e in encoders],
))
fsw.add("wheel_2", WheelSim(cmd=adcs.wheels[2].cmd))
```

A group class's constructor raises `ConfigError` on an unknown keyword or a
mismatched record. `None` leaves a leaf, list, or group unwired. Two groups
share a class only when they render the same class; otherwise, as for a
segment that is a Python keyword or `self`, stubgen fails with M0409. The config flattens groups and lists back to dotted port
names before it writes the target.

## Testing

Unit, beside the code:

- Path rules: segment, index, flatten, rename, skip; nested arrays of groups.
- Collisions: two flattened arguments or groups with a shared port fail to
  build.
- A frame in a frame announces the outer timestamp and frame id only.
- `[P; N]` defs, binds, and items for `Input`, `Sample`, `Output`; `N = 0`.
- A `SystemParams` struct with nested structs and arrays; a flattened argument.
- `#[system_params(async)]` on an async system; its items wake on a record.
- `#[system]` rejects a tuple argument and an array of `Timestamp`.
- Compile-fail (trybuild): `Sample` on an async fn, a `SystemParams` struct on the
  wrong kind of system, a `Sample` in an async `SystemParams` struct.
- Schema equality: a `Fields` struct produces the same vtable and metadata as
  the four standalone derives did.

Integration, in `tests/`: a target with eight `Fss` producers at different
rates feeding one `[Input<Fss>; 8]` system, wired from Python; the same with
an async system taking a `#[system_params(async)]` struct.

Kani: `ParamPath` builds the same string `DefCx::path` does.
