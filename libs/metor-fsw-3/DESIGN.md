# Metor FSW

Metor FSW is a framework for building modular flight software and ground
systems. Software is split into systems. A system is a function over some
inputs, some outputs, and a state of its own. Systems are joined by ring
buffers, and a coordinator steps them in a fixed order once per cycle.

```python
gps = fsw.add("gps", GpsDriver())
imu = fsw.add("imu", ImuDriver())
nav = fsw.add("nav", NavFilter(gps=gps.fix, accel=imu.accel))
```

`nav` reads the rings `gps` and `imu` write, and writes rings of its own
that later systems read.

## Rings

A ring (`metor-fsw-3-ring`) has one writer and a fixed number of reader
slots. Each reader keeps its own cursor, and the writer never reuses bytes
a reader has not released, so a full ring refuses a write rather than
overwrite. A ring lives in memory it is handed, so it works across
processes as well as within one. `ring/DESIGN.md` has the layout and the
memory ordering, and `ring/VERIFY.md` the Miri, Kani, and Loom checks.

## Records

Everything on a ring is a `Record`: a name, an id, a largest length, and a
codec. There are two kinds.

- A frame is a fixed `#[repr(C)]` value whose bytes are its record. Its
  fields decompose into components, the tensors telemetry announces. A
  frame may carry a timestamp.
- A message is any serde value with a codec: postcard, JSON, or raw bytes.

```rust
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
struct Imu {
    #[frame(timestamp)]
    timestamp: Timestamp,
    accel: [f64; 3],
    gyro: [f64; 3],
}

#[derive(Record, Schema, Serialize, Deserialize)]
#[record(max_len = 8)]
struct Arm {
    on: bool,
}
```

Frames and messages share one delivery rule: every reader reads every
record. `Output<T>` writes, `Input<T>` reads with `latest`, `drain`, or, on
an async system, `next`. An input fed by several producers holds one view
per producer. `latest` returns the newest record by timestamp, and records
without one in producer order.

## Systems

A system is a `#[system]` fn. Each parameter is a port named after its
identifier, and one `#[state]` parameter is the state its constructor
builds. The fn's doc comment is the system's doc.

```rust
/// Counts fixes and forwards the newest.
#[system]
fn nav(#[state] s: &mut NavState, gps: &mut Input<Gps>, out: &mut Output<Gps>) {
    if let Ok(Some(fix)) = gps.latest() {
        s.fixes += 1;
        let _ = out.write(&Gps { timestamp: fix.timestamp, pos: fix.pos });
    }
}

table.register("nav", nav(), NavState::default)?;
```

A constructor is `Fn() -> S`, `Fn(P) -> S`, or `Fn(P) -> Result<S,
ParamError>`. `P` is decoded from the instance's JSON params, and its JSON
Schema types the Python binding.

Other parameters a system may take:

- `Timestamp`: this cycle's time.
- `&mut Log`: the system's `log` output, which every system has. `tracing`
  events inside the call land there too.
- `&Sample<T>`: the newest frame on an input, taken once at the cycle's
  start.
- `DynInputs` and `DynOutputs`: ports its config lists rather than its type.

A failed write returns its error. It also counts toward one
`publish_dropped` fault on the system's `log` that cycle, so a caller that
ignores the error still leaves a trace.

A system's definition is computed from its instance config, so a type whose
ports depend on its params, such as a link, is an ordinary system.

The coordinator adds a `status` output to every system and writes
`SystemStatus` there each cycle: the execution time and its offset into
the cycle.

### Async systems

An `async fn` ending in `stop: Stop` runs on a background thread instead,
and paces itself. The thread adapter gives it mirrors of its rings and
copies records across each cycle, so the cycle never waits on it, and a
full mirror drops records for that system alone. Async systems share one
thread unless their config names another with `thread=`.

### Sequences

A `#[sequence]` async fn is polled once per cycle until it returns. Its
`&Seq` handle reads the cycle's time and waits in cycle time.

```rust
#[sequence]
async fn arm_controls(
    seq: &Seq,
    motor_en: &mut Output<MotorEn>,
    motor_health: &mut Input<MotorHealth>,
    control_arm: &mut Output<ControlArm>,
) -> Result<(), SeqError> {
    let _ = motor_en.write(&MotorEn { timestamp: seq.now(), on: true });
    seq.wait_for(Duration::from_secs(1), || healthy(motor_health)).await?;
    seq.sleep(Duration::from_millis(500)).await?;
    let _ = control_arm.write(&ControlArm { timestamp: seq.now(), armed: true });
    Ok(())
}
```

Every sequence has an `abort` input. An abort record, a tripped redline, or
`seq.abort()` raises the abort flag, which never comes down. At the next
cycle's start the step drops the body and ends `Aborted`. A failure or
timeout does not abort on its own.

`seq.on_abort(|seq| ..)` returns a guard. When it drops with the flag up,
its handler runs, innermost first, each under `catch_unwind`.
`seq.on_abort_with(value, |seq, value| ..)` does the same for a handler that
uses `value`, and the guard derefs to it. A redline is a
condition checked every cycle before the body is polled:
`redline!(seq, "name", || cond)`. The first that holds logs a `redline`
fault and aborts the sequence.

```rust
#[sequence]
async fn burn(seq: &Seq, valve: &mut Output<Valve>, pc: &Sample<Chamber>) -> Result<(), SeqError> {
    let mut valve = seq.on_abort_with(valve, |seq, valve| {
        let _ = valve.write(&Valve::closed(seq.now()));
    });
    redline!(seq, "chamber pressure", || pc.get().is_some_and(|c| c.pressure > MAX_PC));
    let _ = valve.write(&Valve::open(seq.now()));
    seq.sleep(BURN).await?;
    let _ = valve.write(&Valve::closed(seq.now()));
    Ok(())
}
```

Handlers and the body write one port through the guard, since
`Output::write` takes `&mut self`.

## The coordinator

`CoordinatorConfig` lists systems in step order. Each names its registered
type, its params, the producers of each input, and an optional adapter.

```rust
struct CoordinatorConfig { clock: Clock, ring_depth: usize, namespace: Option<String>, systems: Vec<SystemConfig> }
struct SystemConfig { id, ty, params, inputs: Vec<InputConfig>, outputs: Vec<OutputConfig>, adapter: Option<Adapter> }
```

`build` resolves each system's definition against a `SystemTable`, checks
every edge's record, allocates one ring per output with one reader slot per
edge, and binds each system over its rings. `step` executes the systems in
list order with one timestamp. `run` paces `step` on a wall or simulated
clock until its stop future resolves.

A system reads an output of a system after it through a loop edge, and
reads the value of the cycle before. The Python `loop()` builds one. The
coordinator does not mark or check it.

The coordinator steps `Box<dyn Step>` and knows nothing else. A panicking
step is faulted once and dropped. Everything beyond a system's own call is
an adapter: a table entry whose make returns a step.

## Adapters

- **Thread.** Runs an async system, as above.
- **Channel.** `channel=` wraps a cyclic system in a step that applies
  `SequenceCommand`s addressed to its name: `Start`, `Stop`, `Reset` (make
  the system again over the same rings), `Abort` (a record on its `abort`
  input), and `Load`. It adds a `commands` input, an `events` output that
  reports each accepted or refused command and how the system ended, and a
  `channel_status` output. `fsw.commands(..)` feeds every channel, and a
  `ChannelRegistry` publishes the target's channels.
- **Slot.** A `DlSlot` is one place in the schedule that loads one of the
  pack systems its allow list names, on its channel's `Load`. Every
  candidate has the first one's ports. A failed load keeps the occupant.
- **Wasm.** A pack built as a module runs each system instance in a guest
  of its own, with a fuel grant per step and a memory ceiling. Its rings
  are copied into guest memory and back around each step.
- **Dylib.** A pack built as a shared library is loaded into the process
  and steps its systems through the ABI.

## Packs and the ABI

A pack is a crate that builds a `SystemTable` and exports it with
`export_pack!`. It builds as a shared library, a wasm module, or both. One
ABI serves both, eight `metor_fsw_*` symbols over JSON:

| Symbol | Does |
|---|---|
| `abi_version` | the version the pack was built against |
| `pack_def` | every system's type, kind, declared ports, doc, and params schema |
| `def` | one instance's definition, from its config |
| `create` | one instance over the host's rings |
| `execute` | one step; returns a `Status` |
| `destroy` | drops an instance |
| `alloc`, `free` | memory the host hands across |

`metor pack dev` builds a pack and writes its Python module: one class per
system, typed from its definition and params schema.

A slot loads from a content-addressed store. `metor build` copies each
candidate into `.metor/store/<blake3>/<file>` and writes the stored path,
hash, and ports into the slot's params. A load hashes the file on its first
open. A rebuilt pack is a new path, so it never replaces a library a
running process has mapped.

## Configuration

Targets are written in Python. Pack modules supply typed system classes,
and `metor_config` records what the file adds.

```python
from adcs_pack import Ctrl, Nav, Plant, TorqueCmd
from metor_config import Target

fsw = Target(cycle_rate=100.0, namespace="cube_sat")
torque = fsw.loop(TorqueCmd)
plant = fsw.add("plant", Plant(torque_cmd=torque, seed=42))
nav = fsw.add("nav", Nav(sensors=plant.sensors))
ctrl = fsw.add("ctrl", Ctrl(estimate=nav.attitude_estimate))
torque.connect(ctrl.torque_cmd)
```

Add order is step order, so a system names only ports of systems already
added, or a loop.

A file emits one envelope, config version 2: every target's packs and
`CoordinatorConfig`, and a map of where each target runs. A file with one
`Target` is a deployment of one. `metor build` resolves the envelope:

- a `Peer` transport becomes the address of the link it names;
- a peer output becomes the definition of the port it copies;
- a db learns the commands its subscribers take;
- a slot candidate's pack becomes a stored library.

`metor run` builds and runs every target, or one with `--target`.

## Links and gateways

`Publish` serves the records wired into it over metor-proto. `Subscribe`
writes the records it is sent onto ports of its own. Each takes one
`transport`: `Listen`, `Connect`, or `Peer`, a link of the deployment that
`metor build` turns into an address.

Both are sequences that hold a `Cap<Endpoint>` named `ep`. `Target.add`
makes the transport a resource named `{link}_ep` and grants it to `ep`:
`Listen` a `tcp_listen`, `Connect` a `tcp_dial`, and `Peer` a dial that
`metor build` resolves. A listener's `allow` list of CIDR blocks lives on
the resource, so the IO layer closes a peer outside it before the link sees
it. The link's ops go through `fsw.io` once per cycle, and every buffer
comes from its arena at make.

```python
plant_pub = plant.add("pub", Publish(transport=Listen("0.0.0.0:2240"), items=[sim]))
mirror = fsw.add("plant", Subscribe(transport=Peer(plant_pub), items=[sim]))
nav = fsw.add("nav", Nav(sensors=mirror.port(sim.sensors)))
```

A subscriber faults each frame port whose table the publisher never
announced, so a peer built against other definitions shows up on `log`.

A gateway is a target whose `Db` embeds a metor-db. It subscribes to every
peer and records what arrives. The ground reads one database, not each
target's stream, and a slow reader never holds a target's rings.
`gateway(..)` builds one.
