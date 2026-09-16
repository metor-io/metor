# Authoring surface

The second slice of metor-fsw-3: records with their codec on the type so
one `write` serves frames and messages, the `system(fn)` builder, params,
the log port, and the ADCS control loop as the acceptance gate.

Out of scope: dynamic frames, shared state, sequences and slots, adapters,
Python, links. Each of those consumes what this slice defines.

## Why this slice

Every ADCS example system is a plain fn over ports, takes a params struct,
and logs through `tracing`. None of that exists yet, and all three shape
the descriptor the pack ABI will freeze. Settling them in-process first
keeps the ABI from churning the way it did in fsw-2.

## Records

A ring record is bytes. A `Record` names the bytes, bounds their size, and
says how a value becomes bytes and back. A `Frame` is a record whose bytes
are a `#[repr(C)]` struct.

```rust
pub trait Record {
    const NAME: &'static str;
    const ID: ComponentId = ComponentId::new(Self::NAME);
    /// Largest record this type writes. A frame's is `size_of::<Self>()`.
    const MAX_LEN: usize;
    const ALIGN: usize = 1;
    /// Writes per cycle this record expects; a ring holds `DEPTH * ring_depth`.
    const DEPTH: usize = 1;
    /// What a read yields: `&'a F` for a frame, an owned value for a message.
    type Read<'a>;
    /// The record bytes: a frame returns `as_bytes()`, a message
    /// serializes into `buf` and returns the prefix it filled.
    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError>;
    fn decode(bytes: &[u8]) -> Result<Self::Read<'_>, DecodeError>;
}

pub trait Frame: Record + AsVTable + Componentize + Decomponentize
    + Metadatatize + IntoBytes + FromBytes + KnownLayout + Immutable
{
    fn timestamp(&self) -> Timestamp;
}
```

`#[derive(Frame)]` emits both impls: `encode` returns `as_bytes()` and
ignores `buf`, `decode` is `ref_from_prefix`, and `Read<'a> = &'a Self`.
A frame's write is the single memcpy into the ring it always was.

A message is `#[derive(Record)]`, the mirror of `#[derive(Frame)]`:

```rust
#[derive(Record, Serialize, Deserialize, MaxSize)]
pub struct SequenceCommand { pub channel: u8, pub kind: CommandKind }

#[derive(Record, Serialize, Deserialize)]
#[record(name = "log", max_len = 4096, depth = 8)]
pub struct LogEvent { .. }
```

The derive emits the `Record` impl with `Read<'a> = Self` and the two
method bodies calling `record::postcard::{encode, decode}`, two short
fns beside the trait. `NAME`
defaults to the snake-cased struct name, as the `Frame` derive does.
`MAX_LEN` defaults to postcard's `POSTCARD_MAX_SIZE`, so a message of
fixed-size fields states no length and `MaxSize` proves the bound; a
message with a `String` or `Vec` has no such bound and must write
`max_len`, and the derive says so when both are missing. The codec is a
property of the type, chosen once; the port never learns which one it
was. A second codec is a hand-written impl over another pair of fns,
and a type that needs two encodings gets a newtype, not a port change.

`LogEvent` is a `metor-proto-wkt` type, so its impl is written by hand in
this crate; it is the same six lines the derive would emit.

`PortDef.frame` becomes `PortDef.id`. There is no kind field: the two ends
of an edge agree on an id, a max length, and an alignment, and the build
already checks all three. Frames and messages share the one id space.

## Ports

`Input<T>` and `Output<T>` stay the only port types, now bounded by
`Record`, with one `write` and one `drain` for every record.

```rust
impl<T: Record> Output<T> {
    pub fn write(&mut self, value: &T) -> Result<(), SendError>;
}
impl<T: Record> Input<T> {
    pub fn drain(&mut self) -> impl Iterator<Item = Result<T::Read<'_>, RecvError>>;
}
impl<F: Frame> Input<F> {
    pub fn latest(&mut self) -> Result<Option<FrameGrant<'_, F>>, ReadError>;
}

pub enum SendError { Oversize { len: usize, max: usize }, Encode(EncodeError), Ring(WriteError) }
pub enum RecvError { Ring(ReadError), Decode(DecodeError) }
```

`write` offers `encode` a scratch buffer of `MAX_LEN` bytes allocated at
bind and hands the ring whatever slice comes back. A slice longer than
`MAX_LEN` fails with `Oversize` before the ring is touched, so a ring
sized for `depth` records always holds `depth`. A frame never uses the
scratch; a postcard message fills it.

`drain` yields `T::Read<'_>`: a borrowed `&F` for frames, an owned value
for messages. A message with `String` fields allocates on decode, which
is that message's choice. `latest` stays on the `Frame` bound because
fan-in orders by frame timestamp and bytes carry none. Messages are read
by draining, per producer in bind order, which is how fsw-2 read them too.

Two `impl` blocks that both define `write` under different bounds would
not compile: a frame may also derive `Serialize`, and coherence has no
negative bounds. Putting the codec on the type is what lets one method
serve both without specialization.

## Builder

```rust
pub struct NavState { .. }
impl NavState { pub fn new(p: NavParams) -> Self { .. } }

#[system]
impl NavState {
    fn execute(&mut self, now: Timestamp,
               gps: &mut Input<Gps>, gps_backup: &mut Input<Gps>,
               estimate: &mut Output<AttitudeEstimate>) { .. }
}

table.register("nav", NavState::new);
table.register("mode", Mode::default);
```

A system is a struct with an `execute` method. The receiver is the
state, so state is first because Rust puts it first. Every other
parameter is one of `&mut Input<T>`, `&mut Output<T>`, `&mut Log`, or
`Timestamp`, in any order, each implementing `Param`.

```rust
pub trait Param {
    type In;      // the bound input, or ()
    type Out;     // the bound writer, or ()
    type Item<'a>;
    fn defs(name: &'static str, inputs: &mut Vec<PortDef>, outputs: &mut Vec<PortDef>);
    fn bind(views: &mut Views, writers: &mut Writers) -> (Self::In, Self::Out);
    fn get<'a>(i: &'a mut Self::In, o: &'a mut Self::Out, now: Timestamp) -> Self::Item<'a>;
}
```

`#[system]` goes on the impl block because an attribute on a method can
only emit impl items and cannot attach anything to the fn value. It
emits the block unchanged plus `impl SystemFn for NavState`: the
parameter names of `execute` as literals in order, and the `Param`
tuple for its signature. Other methods in the block are untouched. The
attribute reads identifiers and doc comments only; it never classifies
a type. A block without `execute` is a macro error. Two ports of one
record type are ordinary; two parameters with one name do not compile.
The names are also where the pack manifest and Python stubs will read
port names and doc comments from, so the attribute is the one place a
system describes itself.

A macro implements `SystemFn` dispatch for up to sixteen `Param`s.
`FnSystem<S>` implements `System` with `State = S`,
`Inputs = InSet<Params>`, and `Outputs = OutSet<Params>`. `InSet::bind`
walks the tuple and consumes one view list per input parameter;
`OutSet::bind` does the same for writers, then takes one more for the
`log` output it always appends, see Log. The builder is sugar over the
existing trait and the positional bind contract is unchanged; adapters
see nothing new.

Construction is a plain fn passed at registration: `Fn() -> S` or
`Fn(P) -> S` with `P: DeserializeOwned`, told apart by a marker type
parameter on the `Ctor<S, M>` trait, the way Bevy's `IntoSystem` does
it. No trait on the state, no builder methods. A stateless system is a
unit struct with the same impl block.

There is no by-type naming and no free-fn form: one authoring path. The
derive bundles remain for the trait path.

## Params

```rust
pub struct SystemConfig {
    pub id: String,
    pub ty: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub inputs: Vec<InputConfig>,
}
```

Params are a serde value tree, which is what Python emits. `Null` decodes
as an empty object, so a params struct with serde defaults on every field
needs no config entry. An unknown key is an error, via `serde_ignored`, so
a typo in a target file fails the build instead of silently taking a
default. There is no separate defaults blob; serde attributes on the
params struct are the defaults.

The table takes factories:

```rust
pub trait SystemFactory {
    type System: System;
    fn def(&self) -> SystemDef;
    fn make(&self, params: Params<'_>)
        -> Result<(Self::System, <Self::System as System>::State), ParamError>;
}
impl SystemTable {
    pub fn register(&mut self, ty: &str, factory: impl SystemFactory + 'static);
}
```

`register(ty, ctor)` wraps a `Ctor<S, M>` for `S: SystemFn` in a
factory that decodes `P` from the entry's value and calls the ctor. The
trait path registers any
`Fn(Params<'_>) -> Result<(S, S::State), ParamError>`, and
`Params::decode::<P>()` is how such a closure reads its struct. A decode
failure surfaces as `BuildError::Params { id, source }`.

When a pack crosses the ABI later, the value tree can cross as JSON text
and be decoded on the far side by the same code. fsw-2's schema-walking
postcard encoder is not needed.

## Log

Every fn system has an output named `log` carrying `LogEvent`, appended
by `FnSystem` as the last entry of its outputs. It is an ordinary port:
the coordinator allocates and binds it like any other, and nothing about
logging lives in the coordinator.

`Log` is a handle over that output with `info`, `warn`, and `fault`
methods that stamp `now` and write one `LogEvent`. A system takes it as
a `&mut Log` parameter when it wants to log directly. Systems may also
log through `tracing`: `metor_fsw_3::log::layer()` returns a
`tracing_subscriber` layer the binary installs, which converts each
event to a `LogEvent` and pushes it onto a thread-local queue of
`MAX_LINES = 64`, drop-newest, counting drops.

`FnSystem::execute` is clear, call, drain: it empties the queue, calls
the user's `execute` with `Log` borrowed from the `log` slot, then drains
the queue into the same slot stamped with `now`, plus one warning line
when the drop count is nonzero. One writer, one stamping site. Events
emitted outside a system's `execute` are cleared, not attributed; a
binary that wants them on a console adds a `fmt` layer beside ours.

Instance identity comes from the ring, not the record: the ring is
`<id>.log`, and the downlink names the instance when it announces the
ring. `source` keeps the tracing target.

The trait path is explicit. A hand-written system that wants logs
declares `Output<LogEvent>` itself and calls `log::drain(now, &mut out)`
if it also wants tracing. Same pieces, no hidden behavior.

A log line allocates its strings. Systems log at edges, not per cycle.
`LogEvent::DEPTH = 8` sizes the ring for a burst.

## Build changes

Pass 1 grows one check: port names unique within a def, which only a
hand-written bundle can violate. Pass 4 sizes each ring by
`MAX_LEN * DEPTH * ring_depth`. `BuildError::FrameMismatch` becomes
`IdMismatch`. Factories run in pass 5 with the entry's params.

## Gate: the ADCS loop

A new crate `examples/adcs-fsw3` holds the contracts and the three systems
from `examples/adcs-fsw2` as one crate, registered statically. The
`Frame` derive already handles the nested wheel array; the three state
structs gain a `#[system]` impl block, and their existing `new(params)`
constructors register as-is. Since
slots come later, a fourth fn system `mode` publishes a fixed nadir
`ModeCmd` so the controller has a target. `plant` is listed first and reads the previous
cycle's commands, which is the loop the fsw-2 target spelled with
`delayed=True`.

The integration test builds the table, runs under the simulated clock,
and asserts the pointing error converges below a bound within a fixed
cycle count, with `nav.log` carrying the first-fix line. A `main` runs the
same config on the wall clock.

## Tests

- Record: a derived message and a derived frame both satisfy the bounds;
  `PortDef` reports the record's id, length, and alignment; a frame's
  `encode` output equals `as_bytes()`; a derived message's `MAX_LEN` is
  its `POSTCARD_MAX_SIZE`; a `trybuild` case for a message with a
  `String` field and no `max_len`.
- Ports: an encoder past `MAX_LEN` is `SendError::Oversize` and leaves the
  ring untouched; a message round-trips through `write` and `drain`; a
  corrupt record is `RecvError::Decode` and the iterator continues; a
  frame drains as `&F` and a message as an owned value.
- Builder: defs carry parameter names in parameter order and skip
  `Timestamp`; outputs before inputs binds; two inputs of one record type
  bind to two rings; a unit-struct system registers with `Default`; a
  `Fn(P) -> S` ctor sees the decoded params; `NavState::execute` is
  callable directly.
- Macro: `trybuild` cases for a block without `execute`, and for a
  parameter whose type is no `Param`, with the error naming the
  parameter.
- Params: `Null` decodes a struct with defaults; a missing required field
  and an unknown key are both `BuildError::Params` naming the system.
- Log: a `Log::info` call and a `tracing::info!` inside execute both land
  on that system's `log` ring with the cycle stamp; an event between
  systems is dropped; the 65th tracing line in a cycle is counted and
  reported once; a fn system with no `Log` parameter still has a `log`
  output; a ring for a `DEPTH = 8` record holds eight times `ring_depth`.
- Kani: `write` never hands the ring a record longer than `MAX_LEN`.

## Open decisions

None outstanding. Earlier rounds settled: codec on the record type, the
`#[system]` impl block with `execute` as receiver method, JSON params,
`#[derive(Record)]` with inferred name and length, and the fn sugar
owning the log drain.

