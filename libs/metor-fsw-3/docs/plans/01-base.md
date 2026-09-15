# Base library

The first slice of metor-fsw-3: rings, fixed frames, typed ports, the
`System` trait, and a coordinator that runs a list of systems from a
low-level config. Enough to wire `imu -> nav -> control` and step it.

Out of scope: messages, dynamic frames, shared state, params decoding, the
log port, sequences and slots, adapters (dylib, process, wasm), links,
gateways, Python.

## Layout

```
libs/metor-fsw-3/
  ring/        metor-fsw-3-ring    copied from metor-fsw-2/ring, frozen
  macros/      metor-fsw-3-macros  Frame, SystemInputs, SystemOutputs derives
  src/
    frame.rs        Frame trait
    port.rs         Input<F>, Output<F>
    system.rs       System, SystemDef, SystemInputs, SystemOutputs
    coordinator/    config, build passes, run loop
```

One crate for now. Modules are layered so `coordinator` is the only one that
depends on the OS clock; a core/host split for wasm32 is a later move.

## Ring

Copy `libs/metor-fsw-2/ring` whole: `lib.rs`, `wake.rs`, `sync.rs`, the
three test modules, and the three proof docs. Rename the package, keep the
region magic and version. Run `cargo test`, Miri, Kani, and Loom once after
the copy, then treat the crate as frozen.

Systems are stepped synchronously inside a cycle, so every port uses
`NoWake`. The `notify` feature stays in the copy; nothing in slice 1
enables it.

## Frames

```rust
pub trait Frame: AsVTable + Componentize + Decomponentize + Metadatatize
    + IntoBytes + FromBytes + KnownLayout + Immutable
{
    const NAME: &'static str;
    const ID: ComponentId = ComponentId::new(Self::NAME);
    const MAX_SIZE: usize = size_of::<Self>();
    fn timestamp(&self) -> Timestamp;
}
```

The component traits come from `metor-component`, shared with panel and db.
`#[derive(Frame)]` bundles those four derives (via `metor-component`'s
`derive-impl`) and reads `#[frame(name = "..")]` and `#[frame(timestamp)]`.
A frame without a timestamp field is a derive error.

Fixed frames only: `MAX_SIZE` is the struct size, a write is `as_bytes()`,
a read is `ref_from_prefix` on the grant.

## Ports

```rust
pub struct Output<F> { writer: Writer<NoWake>, .. }
pub struct Input<F>  { views: Vec<View<NoWake>>, .. }

impl<F: Frame> Output<F> {
    pub fn write(&mut self, frame: &F) -> Result<(), WriteError>;
}

impl<F: Frame> Input<F> {
    pub fn latest(&mut self) -> Result<Option<FrameGrant<'_, F>>, ReadError>;
    pub fn drain(&mut self, f: impl FnMut(&F)) -> Result<(), ReadError>;
}
```

An input holds one view per producer. Zero producers is a legal unconnected
input. `drain` visits every view in order; producers are not interleaved.
`latest` pins the newest record on each view and returns the one with the
greatest timestamp. There is no delivery, fan-in, or connection axis on a
port; the reader chooses the read.

`Vec<View>` is allocated at bind time and never grows.

## Systems

```rust
pub trait System {
    type State;
    type Inputs: SystemInputs;
    type Outputs: SystemOutputs;
    fn def() -> SystemDef;
    fn execute(&self, state: &mut Self::State, inputs: &mut Self::Inputs,
               outputs: &mut Self::Outputs);
}

pub trait SystemInputs  { fn bind(views: Vec<Vec<View<NoWake>>>) -> Self; }
pub trait SystemOutputs { fn bind(writers: Vec<Writer<NoWake>>) -> Self; }

pub struct SystemDef { pub name: &'static str, pub inputs: Vec<PortDef>,
                       pub outputs: Vec<PortDef> }
pub struct PortDef { pub name: &'static str, pub frame: ComponentId,
                     pub max_size: usize }
```

`execute` takes `&self` so a definition carries no mutable data; `State` is
the only mutable data, which is what will let a later slice share one state
between systems. Inputs are `&mut` because a read advances a cursor.

Binding is positional. `bind` receives one entry per port in `def()` order.
This is the only bind contract; adapters will receive the same lists as raw
ring handles.

`#[derive(SystemInputs)]` and `#[derive(SystemOutputs)]` generate `bind` and
the port list from a struct of `Input<F>` / `Output<F>` fields in field
order.

The `system(fn).with_state(..)` builder is deferred. It is sugar over this
trait and lands once the trait has run end to end.

## Coordinator

### Config

```rust
pub struct CoordinatorConfig { pub clock: Clock, pub depth: usize,
                               pub reader_slack: usize,
                               pub systems: Vec<SystemConfig> }
pub struct SystemConfig { pub id: String, pub ty: String,
                          pub inputs: Vec<InputConfig> }
pub struct InputConfig  { pub port: String, pub from: Vec<PortRef> }
pub struct PortRef      { pub system: String, pub port: String }

pub enum Clock { Wall { rate: Hz }, Simulated { dt: Duration } }
```

List order is step order. A producer may appear later in the list than its
consumer; the consumer then reads the previous cycle's record. Nothing marks
or checks this. The Python builder's `loop()` is a construction-order
device, not a config field.

### Table

```rust
pub struct SystemTable { .. }
impl SystemTable {
    pub fn register<S: System + 'static>(&mut self, ty: &str,
                                         make: impl Fn() -> (S, S::State));
}
```

The table maps a `ty` string to a factory. `build` looks every config entry
up here; an unknown type is a build error.

### Build

One validation gate, then trust. Passes, in order:

1. Ids unique, every `ty` in the table, every `PortRef` names a known system
   and output port, every consumer port name exists on its def.
2. Frame ids match on every edge.
3. Count readers per output ring: one per edge plus `reader_slack`.
4. Allocate one ring per output, capacity `capacity_for(max_size, depth)`,
   plus one status ring per system.
5. Bind: create the writer for each output and one view per edge, call
   `Inputs::bind` and `Outputs::bind`, box the runner.

All errors are a `BuildError` enum. No allocation after build.

### Status port

Every system gets one coordinator-owned output named `status`:

```rust
#[derive(Frame)]
#[frame(name = "status")]
pub struct SystemStatus {
    #[frame(timestamp)] pub timestamp: Timestamp,
    pub exec_time_ns: u64,
    pub exec_offset_ns: u64,
}
```

It is an ordinary output ring, so a later system may wire `<id>.status` as
an input.

### Run

```rust
impl Coordinator {
    pub fn step(&mut self, now: Timestamp);
    pub async fn run(&mut self, stop: impl Future<Output = ()>);
    pub fn cycle(&self) -> u64;
}
```

`step` is the primitive and is synchronous: it records the cycle start,
executes every runner in order, writes that runner's status record, and
increments the cycle count. `run` is an async fn on the stellarator
runtime. Each iteration picks `now` from the clock, calls `step`, then
`stellarator::sleep`s out the wall budget, or `yield_now`s under the
simulated clock. It returns when `stop` resolves, checked once per cycle.
A bounded run is a `stop` future that resolves after N cycles; a CLI run
passes a signal future. Nothing else in slice 1 awaits; the async loop is
what lets a later slice spawn IO tasks beside the cycle without changing
the loop. A status write that fails is dropped; nothing in the loop returns
an error.

The erased runner:

```rust
trait Step { fn name(&self) -> &str; fn execute(&mut self, now: Timestamp); }
struct Runner<S: System> { system: S, state: S::State,
                           inputs: S::Inputs, outputs: S::Outputs }
```

## Tests

- Ring: the copied suites, plus one run each of Miri, Kani, and Loom.
- Frame: derive output on a two-field frame; missing timestamp is an error.
- Port: publish then latest; drain order; fan-in latest picks the greater
  timestamp; empty input drains nothing; full ring returns `WouldBlock`.
- Coordinator build: unknown type, unknown system, unknown port, frame
  mismatch, duplicate id each return their `BuildError`.
- Coordinator run: three-system pipeline sees same-cycle data; a backward
  reference sees the previous cycle; fan-in from two producers; status
  records carry a nonzero exec time; simulated clock advances by `dt`.
  `step` tests run without a runtime; `run` tests use `stellarator::test`
  with a stop future that counts cycles.
- Kani: `capacity_for` fits `u32` records and never returns less than two
  records.
