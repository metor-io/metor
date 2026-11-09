# Hardware config: one target, many setups

A target runs on several hardware setups: an in-process sim, VMs, the
vehicle's computers with their ground equipment, and HITL rigs. Everything
but the hardware binding is the same in each. This doc gives the Python
conventions that keep it that way, and two core changes they need.

## Problem

`examples/adcs-fsw3` keeps `sim.py` and `target.py`. Each repeats the
wiring of nav, ctrl, and the plant, and the plant's params. Ports come from
`METOR_DEPLOY_PORTS`. A change to the wiring has to be made twice, and
nothing checks that the two agree.

## Goals

- The flight structure is written once. A setup supplies only hardware and,
  in a sim, a world.
- A setup that leaves a hardware slot unbound fails type checking.
- A test shows that two setups differ only in their hardware and world.
- Calibration sits with the hardware it describes.

## Non-goals

- New core types. `metor_config` gains no hardware, machine, or variant
  types. Hardware config is user code over the existing `Kind`s in
  `metor_config/_io.py`.
- A framework abstraction for choosing where a sim replaces hardware.
- Rendering the hardware config to NixOS, a machine or network model, and VM
  emulation. See [Future](#future).

## Reference designators

There are two kinds.

| Kind | Examples | Names |
|---|---|---|
| Logical | `GPS1`, `PT101`, `SV101` | a part on the vehicle, by function |
| Electrical | `/dev/ttyTHS1`, DAQ channel 5, CAN node `0x21` | where the software reaches it |

The hardware config maps logical to electrical. The fields of a hardware
dataclass are logical designators, and their values are electrical leaves.
Nesting, such as a DAQ with channels or a board with a daughterboard, lives
on the electrical side and is optional.

## Example

A propulsion target carries one example through this doc.

| Logical | Electrical |
|---|---|
| `GPS1` | a u-blox on a UART |
| `PT101`, `PT102`, `PT201` | channels 5, 6, 7 of `daq0` |
| `TC101`, `TC201` | channels 0, 1 of `daq1` |
| `SV101`, `SV102` | outputs 0, 1 of a valve board on CAN |

```
prop/
  prop_config/
    parts.py       electrical leaves and the parts that own drivers
    hardware.py    Hardware, the designator table, slots()
    structure.py   Slots, World, the propulsion composite
  setups/
    vehicle.py  sim.py  sim_truth.py  hitl.py
  tests/test_parity.py
```

The pack `prop_pack` and its systems (`UbloxDriver`, `DaqDriver`, `Nav`,
...) are hypothetical. The sketches use `@composite`, `Scope`, and
`Composite` from the composites design (`docs/composites.md` on
`sphw/fsw3-composite`, not on main).

## Leaves

A leaf separates where a device is from what the software needs of it. The
hardware config holds the path; the flight code holds the baud.

```python
@dataclass(frozen=True)
class Uart:
    path: str

    def __call__(self, *, baud: int) -> Tty:
        return Tty(self.path, baud=baud)


@dataclass(frozen=True)
class SimSerial:
    def __call__(self, *, baud: int) -> SimUart:
        return SimUart()


Serial = Uart | SimSerial


@dataclass(frozen=True)
class CanBus:
    iface: str

    def __call__(self, *, filters: Sequence[tuple[int, int]]) -> Can:
        return Can(self.iface, filters)
```

A leaf renders to an existing `Kind`. A new kind of wire is a new leaf
class, not a core change.

## Parts own their drivers

A part is a leaf for one model of device. Its `io(..)` returns a composite
that adds the resource and the driver, and the device model when the
backing is sim.

```python
@dataclass(frozen=True)
class Ublox:
    port: Serial

    def io(self, truth: Sources[Body] | None) -> Composite[OutPort[GpsFix]]:
        return ublox(port=self.port, truth=truth)


@composite
def ublox(s: Scope, *, port: Serial, truth: Sources[Body] | None) -> OutPort[GpsFix]:
    uart = s.resource("uart", port(baud=115_200))
    drv = s.add("drv", UbloxDriver(uart=uart))
    if isinstance(port, SimSerial):
        s.sim("model", UbloxModel(uart=uart.device, truth=need(truth)))
    return drv.fix
```

`need(truth)` raises `ConfigError` when a sim backing has no truth. Adding
a Novatel for the same slot is a new class with the same `io` signature.

## Structure

The structure is a composite that takes typed hardware slots. Each slot is
a `Protocol` whose `io(..)` returns a composite that produces logical frames
or consumes command frames.

```python
class GpsSlot(Protocol):
    def io(self, truth: Sources[Body] | None) -> Composite[OutPort[GpsFix]]: ...

class SensorSlot(Protocol):
    def io(self, truth: Sources[Tanks] | None) -> Composite[Sensors]: ...

class ValveSlot(Protocol):
    def io(self, cmd: OutPort[ValveCmd], plant: Loop[ValveCmd] | None) -> Composite[None]: ...


class Slots(NamedTuple):
    gps1: GpsSlot
    sensors: SensorSlot
    valves: ValveSlot


class Sensors(NamedTuple):
    feed: OutPort[FeedPressures]
    temps: OutPort[Temps]


class World(NamedTuple):
    body: OutPort[Body]
    tanks: OutPort[Tanks]
    valves: Loop[ValveCmd]


@composite
def propulsion(s: Scope, *, io: Slots, world: World | None) -> Prop:
    gps = s.add("gps1", io.gps1.io(world and world.body))
    sensors = s.add("sensors", io.sensors.io(world and world.tanks))
    nav = s.add("nav", Nav(gps=gps, feed=sensors.feed, temps=sensors.temps))
    ctl = s.add("ctl", PressCtl(state=nav.state))
    s.add("valves", io.valves.io(ctl.valve_cmd, world and world.valves))
    return Prop(nav, ctl)
```

The nav → control wiring is the same in every setup. Two kinds of change
follow:

- A new part for an existing slot is a new class. The structure is
  unchanged.
- A new kind of input to GNC is a new field of `Slots`. Every setup must
  bind it, and pyright reports the ones that do not.

`World` is the plant's truth and the loop the plant reads commands from. It
is one `NamedTuple` per structure, and `None` on the vehicle. The structure
passes each slot only the field it reads, so a slot's `io` is typed to its
own truth: a driver ignores it, a sim part requires it, and a device model
passes it on.

## DAQ signals

A DAQ driver emits `Volts { v: [f64; 32] }`: every channel in volts, with
the DAQ's channel calibration applied. A logical frame such as
`FeedPressures { pt101, pt102, pt201 }` is the tag list for a set of
signals. A `Mux` maps channel elements to tag fields, using
[mux element paths](#core-change-mux-element-paths), and applies each
sensor's calibration on the way, using
[calibrated mappings](#core-change-calibrated-mux-mappings). Each component
keeps one unit: `Volts` is all volts, and each field of `FeedPressures` has
one unit.

```python
@dataclass(frozen=True, eq=False)
class Daq:
    ref: str
    port: Serial
    channel_cal: Mapping[int, Cal] = field(default_factory=dict)  # DAQ cert


@dataclass(frozen=True)
class DaqChannel:
    daq: Daq
    n: int
    sensor: Cal                                                  # sensor cert
```

`Daq` compares by identity, so two sim DAQs with equal ports stay distinct.

The reusable piece is the `daq` composite: resource, driver, and an
optional model. The driver takes the DAQ's channel calibration as one param.
A DAQ that calibrates on the device takes none.

```python
@composite
def daq(s: Scope, *, d: Daq, truth: Sources[Tanks] | None) -> OutPort[Volts]:
    dev = s.resource("dev", d.port(baud=921_600))
    drv = s.add("drv", DaqDriver(dev=dev, cal=channel_cals(d)))
    if isinstance(d.port, SimSerial):
        s.sim("model", DaqModel(dev=dev.device, truth=need(truth), channels=...))
    return drv.volts
```

The model needs the same rows, inverted, to put volts on the wire.

The tag binding is a function that returns a `Mux`:

```python
def bind(frame: type[Record], rows: Mapping[str, DaqChannel],
         volts: Mapping[Daq, OutPort[Volts]]) -> Mux:
    if missing := sorted(set(frame._fields) - set(rows)):
        raise ConfigError(f"{frame.__name__} leaves {missing} unbound")
    return Mux(out=frame(**{
        tag: Calibrated(volts[r.daq].v[r.n], r.sensor) for tag, r in rows.items()
    }))
```

Two checks run in Python:

- Every field of the frame is bound. A mux leaves unmapped fields zero, so a
  missing tag would read as 0 psi.
- No channel is claimed twice, across every frame. `check_claims` counts
  `(daq, n)` pairs over all rows and checks `0 <= n < 32`.

The `DaqSensors` part adds each distinct DAQ once, then one mux per frame:

```python
@composite
def daq_sensors(s: Scope, *, rows: DaqSensors, truth: Sources[Tanks] | None) -> Sensors:
    check_claims([*rows.feed.values(), *rows.temps.values()])
    volts = {d: s.add(d.ref, daq(d=d, truth=truth)) for d in rows.daqs()}
    feed = s.add("feed", bind(FeedPressures, rows.feed, volts))
    temps = s.add("temps", bind(Temps, rows.temps, volts))
    return Sensors(feed.out, temps.out)
```

## Calibration

There are two calibrations, each kept with the hardware it describes.

| | Channel cal | Sensor cal |
|---|---|---|
| Corrects | the DAQ input: gain, ADC offset | the transducer: span, zero, nonlinearity |
| Certified by | the DAQ cert, by DAQ S/N and channel | the sensor cert, by sensor S/N |
| Lives on | `Daq.channel_cal` | `DaqChannel.sensor` |
| Applied by | the DAQ driver (or the device itself) | the tag mux |
| Unit | counts to volts | volts to engineering units |

Each moves with its own hardware. Swapping PT101 for a spare changes its
sensor cal; channel 5 keeps its own. Rewiring PT201 from channel 7 to 9 keeps
PT201's sensor cal and picks up channel 9's channel cal. Replacing `daq0`
changes every channel cal and no sensor cal.

A DAQ driver always emits volts, so volts stay in telemetry and each number
traces to one cert. `DaqDriver` here is hypothetical, a shape for real
drivers to follow.

Neither is a param threaded through the structure. Certs are data files keyed
by serial number, loaded by the setup.

```python
SN_0042 = {
    "pt101": Cal.two_point(volts=(0.5, 4.5), eng=(0, 1000)),
    "pt201": Cal.table([(0.5, 0), (1.5, 752), (2.5, 1499), (3.5, 2251), (4.5, 3000)]),
    "tc101": Cal.thermocouple("K"),
}
```

## The designator table

`Hardware` maps logical designators to electrical leaves. The table that
fills it is written once, as a function of the DAQs and the board.
Calibration varies by unit, so it is an argument.

```python
@dataclass(frozen=True)
class Hardware:
    gps1: GpsSlot
    pt101: DaqChannel
    pt102: DaqChannel
    pt201: DaqChannel
    tc101: DaqChannel
    tc201: DaqChannel
    sv101: ValveOut
    sv102: ValveOut


def hardware(*, gps1: GpsSlot, daq0: Daq, daq1: Daq, board: ValveBoard,
             cal: Mapping[str, Cal] = {}) -> Hardware:
    def ch(tag: str, daq: Daq, n: int) -> DaqChannel:
        return DaqChannel(daq, n, cal.get(tag, Cal.linear(gain=1.0, offset=0.0)))

    return Hardware(
        gps1=gps1,
        pt101=ch("pt101", daq0, 5), pt102=ch("pt102", daq0, 6), pt201=ch("pt201", daq0, 7),
        tc101=ch("tc101", daq1, 0), tc201=ch("tc201", daq1, 1),
        sv101=ValveOut(board, 0), sv102=ValveOut(board, 1),
    )


def slots(hw: Hardware) -> Slots:
    return Slots(
        gps1=hw.gps1,
        sensors=DaqSensors(
            feed={"pt101": hw.pt101, "pt102": hw.pt102, "pt201": hw.pt201},
            temps={"tc101": hw.tc101, "tc201": hw.tc201},
        ),
        valves=BoardValves({"sv101": hw.sv101, "sv102": hw.sv102}),
    )
```

`BoardValves.io` adds one `Can` resource per board, a mux from `ValveCmd`
to the board's `BoardCmd { open: [bool; 16] }` by element
(`BoardCmd(open={o.ch: cmd.field(tag) for tag, o in ...})`), and the board
driver. Given the plant's loop, it also connects that loop to the command,
so the plant sees what was commanded.

Hardware that no slot uses renders nothing.

## Setups

A setup is a small entry file. It passes hardware and a world as arguments.
The setup is chosen by file, not by environment variable.

```python
# setups/vehicle.py
fsw = Target(cycle_rate=200.0, namespace="prop")
hw = hardware(
    gps1=Ublox(Uart("/dev/ttyTHS1")),
    daq0=Daq("daq0", Uart("/dev/ttyTHS2")),
    daq1=Daq("daq1", Uart("/dev/ttyTHS3")),
    board=ValveBoard("vb1", CanBus("can0"), node=0x21),
    cal=SN_0042,
)
fsw.add("prop", propulsion(io=slots(hw), world=None))
```

A sim builds its world first:

```python
def plant(t: Target) -> World:
    valves = t.loop(ValveCmd)
    p = t.add("plant", Plant(valves=valves, seed=1))
    return World(p.body, p.tanks, valves)
```

Links, the db, and the channel registry go in a shared function each setup
calls, as `adcs-fsw3/target.py` builds its gateway.

## Sim wirings

Two wirings are both valid. Each is a different config. There is no default;
a project picks.

**(a) Resource level.** The drivers are the flight drivers. Backings become
`SimUart` and the like, and a device model speaks the wire protocol, as
`GpsReceiver` does in `adcs-fsw3/sim.py`.

```python
# setups/sim.py
fsw = Target(cycle_rate=200.0, sim_dt=0.005, epoch=0, namespace="prop")
world = plant(fsw)
hw = hardware(
    gps1=Ublox(SimSerial()),
    daq0=Daq("daq0", SimSerial()),
    daq1=Daq("daq1", SimSerial()),
    board=ValveBoard("vb1", CanBus("can0"), node=0x21),
)
io = slots(hw)._replace(valves=PlantValves())
fsw.add("prop", propulsion(io=io, world=world))
```

`SimCan` does not exist yet (`TODO.md:169`), so this setup binds the valves
at the logical frame.

**(b) Logical frame.** A sim part feeds the logical frame from truth. No
drivers run.

```python
# setups/sim_truth.py
fsw = Target(cycle_rate=200.0, sim_dt=0.005, epoch=0, namespace="prop")
world = plant(fsw)
io = Slots(gps1=TruthGps(sigma=2.0), sensors=TruthSensors(), valves=PlantValves())
fsw.add("prop", propulsion(io=io, world=world))
```

(a) tests the drivers and the calibration; (b) is cheaper and needs no
device models. Both go through the same slot boundary, so there is no
framework concept of where the cut lies.

**HITL.** Real valves on the bench, sensors from the in-process plant. The
target holds real and sim resources, so it needs the
[second core change](#core-change-targets-that-mix-real-and-sim-resources).

```python
# setups/hitl.py
fsw = Target(cycle_rate=200.0, namespace="prop")
world = plant(fsw)
hw = hardware(
    gps1=Ublox(SimSerial()),
    daq0=Daq("daq0", SimSerial()),
    daq1=Daq("daq1", SimSerial()),
    board=ValveBoard("vb1", CanBus("can0"), node=0x21),
    cal=BENCH,
)
fsw.add("prop", propulsion(io=slots(hw), world=world))
```

A rig where the plant runs on another computer and drives the flight
computer's real ports needs no mixing: the flight target is the vehicle
setup, and the rig is its own target.

## Parity test

The test renders each setup with `metor build <file> -o <out>`, masks what
a setup may change, and compares the rest.

Masked:

- `coordinator.resources` (the backings);
- `coordinator.clock`;
- the world's systems (`plant`);
- systems under a slot (`prop.gps1.`, `prop.sensors.`, `prop.valves.`);
- an input source inside a slot, which collapses to the slot's prefix.

The `io_submit` and `io_complete` entries are compared, not masked. They are
the same two entries in every setup that holds a resource, so a setup that
drops every resource, its links included, fails.

```python
SLOTS = ("prop.gps1", "prop.sensors", "prop.valves")
WORLD = {"plant"}

def slot(system: str) -> str | None:
    return next((p for p in SLOTS if system.startswith(p + ".")), None)

def masked(path: Path, out: Path) -> list[dict]:
    subprocess.run(["metor", "build", str(path), "-o", str(out)], check=True)
    (target,) = json.loads(out.read_text())["targets"]
    systems = []
    for s in target["coordinator"]["systems"]:
        if s["id"] in WORLD or slot(s["id"]):
            continue
        for i in s.get("inputs", []):
            i["from"] = [{"slot": p} if (p := slot(f["system"])) else f for f in i["from"]]
        systems.append(s)
    return systems

@pytest.mark.parametrize("setup", ["sim.py", "sim_truth.py", "hitl.py"])
def test_parity(setup: str, tmp_path: Path) -> None:
    vehicle = masked(SETUPS / "vehicle.py", tmp_path / "vehicle.json")
    assert masked(SETUPS / setup, tmp_path / "other.json") == vehicle
```

Off-nominal and failure cases: a setup that adds a system outside a slot,
or changes a param of `nav`, fails the test.

## Core change: mux element paths

A mux source or destination may name one element of an array leaf. This
covers DAQ channels and arrays generally.

### Today

`FieldTable::of` (`src/frame_fields.rs:51`) lists a frame's leaves. Arrays
come in two forms:

- A plain `[T; N]`, of structs or primitives, is split into leaves
  `x.0` ... `x.{N-1}` by `[T; N]: AsVTable`
  (`libs/metor-component/src/vtable.rs:45`). In Rust, `v.5` of
  `v: [f64; 32]` already resolves. In Python, `v` is a group, so `raw.v[5]`
  does not.
- A nox tensor (`Tensor`, `V3`, `Matrix6`) or a `FrameArray` is one shaped
  leaf, such as `f64[32]` or `f64[6,6]`. `relative` (`src/mux/plan.rs:229`)
  finds leaves equal to or under a path, so `v.5` matches nothing and fails
  as `UnknownField`.

Element paths cover both: shaped leaves need the Rust change below, and both
need the Python and stubgen changes.

### Python API

A source indexes a field:

```python
raw.v[5]            # Field(daq0.drv.channels.v.5: f64)
cov.p[1][2]         # f64[6,6] -> f64[6] -> f64
raw.field("v.5")    # the same, by path
```

A destination names elements with a mapping from index to field:

```python
BoardCmd(open={0: cmd.sv101, 1: cmd.sv102})   # dst paths open.0, open.1
```

- `ArrayField.__getitem__(i)` takes an `int` and returns
  `port.field("{path}.{i}")`: the element of a shaped leaf, whose signature
  drops the first dimension, or child `i` of a split array. A negative
  index or an index past the end raises `ConfigError`. A scalar or a group
  whose children are not `0..N` is a plain `Field`, which has no
  `__getitem__`; `OutPort.field` returns an `ArrayField` for the others.
- `signature` (`metor_config/_fields.py:28`) resolves element paths, so
  `OutPort.field` and `Record.__init__` accept them.
- Stubgen types elements. An array port property returns a generated
  `ArrayField` whose `__getitem__` returns the element's signature, so
  `raw.v[5]` is `Field[Literal["f64"]]` and `cov.p[1]` is
  `Field[Literal["f64[6]"]]`. A generated `__init__` types an array keyword as
  `Field[Literal["bool[16]"]] | Mapping[int, Field[Literal["bool"]]] | None`.
  Bounds are checked at runtime.
- A record that maps a leaf whole and one of its elements, or one element
  twice, raises `ConfigError`. Make refuses the same in a hand-written
  config: two copies of one output whose destination bytes intersect fail
  with `MuxError::Overlap { port, a, b }`.

### Semantics

- An element of `ty[d0, d1, ..]` is `ty[d1, ..]`, and a scalar when the
  leaf is one-dimensional. Each index segment drops one dimension, so
  `m.1.2` of `f64[3,3]` is `f64`.
- Type and shape match exactly, as for whole leaves.
- Element paths use the separator that struct arrays use, so a path names
  the same element whether the array holds structs or primitives.
- Unmapped elements stay zero, as unmapped fields do.

### Lowering

`FieldTable` gains `element(path) -> Option<Leaf>`: find the leaf that is a
proper prefix of `path`, parse the remaining segments as indices, check each
against the shape, and return a `Leaf` with

```
offset = leaf.offset + flat(indices) * stride
len    = ty.size() * product(shape[k..])
shape  = shape[k..]
```

where `k` is the number of indices and `stride = len`. `relative` falls back
to `element` when no leaf is under the path, and returns the one leaf at
relative path `""`. `field_copies` is unchanged, and so is `Copy`: an
element lowers to one `(input, src, dst, len)` byte copy. `step.rs` and
`apply` (`src/mux/plan.rs:277`) are unchanged.

`MuxError` gains `Index { port, path, len }` for an index past a dimension,
and `Overlap { port, a, b }` for two mappings whose destination bytes
intersect.
A segment that is not a decimal index stays `UnknownField`. Make checks
bounds again, since a hand-written config skips Python.

Adjacent copies could be merged; this is not needed for correctness.

### Tests

- Nominal: `v.5` to a scalar; `p.1` of `f64[6,6]` to `f64[6]`; `open.0` as a
  destination; whole and element mappings to different leaves of one
  output.
- Edge: the last index; index 0 of a `[f64; 1]`; a struct array path still
  resolves to its leaves.
- Failure: index out of range at config and at make; a non-numeric segment;
  indexing a scalar; a whole leaf and its element both mapped.

`docs/muxes.md` gains the element rules.

## Core change: calibrated mux mappings

A mux mapping may apply a calibration: a scalar `f64` source to a scalar
`f64` destination. Plain mappings stay byte copies. This needs element
paths, so a channel such as `v.5` is a scalar leaf.

### Python API

```python
Mux(out=FeedPressures(
    pt101=Calibrated(volts.v[5], Cal.two_point(volts=(0.5, 4.5), eng=(0, 1000))),
    pt102=other.p,                                   # a plain copy
))
```

Constructors:

| Constructor | Lowers to |
|---|---|
| `Cal.poly([c0, c1, ..])` | `Poly`, ascending: `c0 + c1·x + ..` |
| `Cal.linear(gain, offset)` | `Poly([offset, gain])` |
| `Cal.two_point(volts, eng)` | `Poly`, gain and offset computed in Python |
| `Cal.table([(x, y), ..])` | `Table`, sorted by `x` |
| `Cal.thermocouple("K")` | `Thermocouple { kind }` |
| `Cal.ratiometric(excitation, span, eng)` | `Ratiometric`, `excitation` a field of the same input |

### Semantics

```rust
pub enum Cal {
    Poly(SmallVec<[f64; 2]>),
    Table(Box<[(f64, f64)]>),
    Thermocouple { kind: TcKind },
    Ratiometric { excitation: u32, span: (f64, f64), eng: (f64, f64) },
}
```

- `Poly` evaluates by Horner. Linear is a two-term `Poly`, held inline.
- `Table` interpolates linearly between points and clamps at the ends.
- `Thermocouple` applies the NIST ITS-90 inverse for `kind`. Coefficients
  are `const` slices; the piecewise ranges are internal. The input is a
  cold-junction-compensated voltage: the DAQ compensates.
- `Ratiometric` reads `excitation` from the same record and scales
  `v / excitation` from `span` to `eng`.
- A NaN source gives a NaN destination.

### Lowering

`FieldMap` (`src/mux/params.rs:54`) gains `#[serde(default)] cal:
Option<CalParam>`. `Copy` (`src/mux/plan.rs:15`) gains `cal: Option<u16>`,
an index into `Plan::cals`. `apply` (`plan.rs:277`) copies bytes when `cal`
is `None`, and otherwise reads an `f64` at `src`, evaluates, and writes an
`f64` at `dst`. `take` applies copies one record at a time, so a
calibration's extra source, such as `excitation`, must be in the same input.
`step.rs` is unchanged.

All tables and polynomials are allocated at make. `smallvec` is a new
dependency. `MuxError` gains
`CalType` (source or destination not a scalar `f64`), `CalInput` (an extra
source in another input), and `CalTable` (unsorted or empty).

### Tests

- Nominal: each kind against reference values, including NIST type K
  points.
- Edge: table ends clamp; a one-coefficient `Poly`; `Poly` spilling past two
  coefficients.
- Failure: a calibrated mapping to an `i32` or an array; excitation from
  another input; an unsorted table.
- Prover: Kani on `Poly` and `Table` evaluation for no panics over all
  inputs.

`docs/muxes.md` states that a mapping is a copy or a calibration.

## Core change: targets that mix real and sim resources

### What the code does today

`TODO.md:168` says build refuses a target that mixes real and sim
resources. On main nothing refuses it:

- `python/metor-config/tests/test_io.py:261`
  (`test_real_and_sim_resources_mix`) renders a target holding a `Tty` and a
  `SimUart`.
- `Bindings::collect` (`src/coordinator/build.rs:609`) binds real grants and
  sim holders in one pass. The only sim refusal is `SimGrant`
  (`build.rs:651`), a sim resource granted directly.
- `IoState::build` (`src/io/host/mod.rs:257`) makes one `Dispatch` over every
  resource. `Resource::new` (`mod.rs:297`) makes a wire for a held sim
  resource and a recipe for a held real one.

The line dates from the first fsw3 commit (`ec71ee2c4`). The mechanism has
since handled both. The problem is time, not wiring.

### What breaks

**A simulated clock with a real resource.** `run`
(`src/coordinator/run.rs:90`) does not pace a simulated clock:
`cycle_budget` (`build.rs:896`) returns `None` and each cycle only yields.
The target runs as fast as the CPU allows.

- A real device sees cycles at CPU speed, not at `sim_dt`.
- `Op::Timeout` deadlines are cycle times (`src/io/host/dispatch.rs:348`),
  landed by `expire` (`dispatch.rs:504`). A 1 s read timeout fires after 1 s
  of sim time, which may be milliseconds of wall time. `Reconnect` backoff
  does the same.
- Real completions arrive in whichever io pass follows the kernel, so the
  cycle a reply lands in depends on CPU speed.

Links are real resources (`tcp_listen`, `tcp_dial`), so every sim with a
`Publish` already holds one. That is acceptable for telemetry. It is not for
a device in the loop.

**Determinism.** `check_deterministic` (`build.rs:873`) checks only the
clock. A `--deterministic` run that holds a real resource builds and is not
reproducible.

**A wall clock with a sim resource.** This works. Models run between
`io_submit` and `io_complete` (`metor_config/_target.py:295`); a driver's
request reaches its model the cycle it is sent, and the answer reaches the
driver the next cycle. `adcs-fsw3/sim.py` already runs this way. The costs:

- A plant integrates over the time between cycle stamps (`Plant.last` in
  `adcs-fsw3/systems/adcs/src/plant.rs`), so an overrun is a longer step and
  jitter enters the dynamics.
- Runs are not reproducible.

**Stamps.** A mux's `max_age` assumes every stamped input shares the
target's clock (`src/mux/step.rs:15`). A driver that copies a device's own
time, such as GNSS time, into a stamp breaks that under a simulated epoch.

### Proposal

A mixed target is one that holds at least one real and one sim resource,
links excluded. It means: real devices at real time, models stepped by the
same cycle.

1. A mixed target runs on a wall clock. A simulated clock with a real device
   is not refused; it is documented as unpaced, and setups do not do it.
2. Models in a mixed target step once per wall cycle and read the cycle
   time, as on a pure-sim wall target. No new scheduling.
3. A model may hold a real resource, such as a rig model driving a real
   UART. `_target.py` allows it today; this makes it intended.
4. Drivers stamp records with the cycle time. Device time goes in a field.
5. `TODO.md:168` is reworded to the rules above.

None of these rules adds a build check. A simulated clock with a real
device, and `--deterministic` with a real resource, are user errors.

## Dependencies

- Composites (`docs/composites.md`, not on main).
- Optional inputs in composites: `Sources[T] | None`, so a slot reads truth
  only in a sim. The sketches use it.

## Future

- Rendering the hardware config to NixOS: udev rules that make
  `/dev/metor/<ref>` symlinks, systemd `.link` files for NICs, through a Nix
  to serde bridge. Python writes data; Nix modules decide (see the prior-art
  report, NixOS section). Leaves would then hold `/dev/metor/gps1` and the
  match spec.
- A machine and network model, with hosts from target groups.
- VM emulation of a setup from the same data, as NixOS `build-vm` does.
- `SimCan`, `SimUdp`, and the rest (`TODO.md:169`), so wiring (a) covers
  every slot.
- Taring: offsetting one component by another, such as a PT read at ambient
  before a test. Cold-junction compensation from a separate channel may fit
  there; until then `Thermocouple` assumes the DAQ compensates.
- A curve on `Ratiometric`, when a load cell or other bridge with a
  multi-point cal appears.
- Units in frame field metadata. A calibrated mapping could then check its
  output unit against the destination's.
- Live reload of a target, which would let a calibration change without
  restarting its mux.
- A paced simulated clock: fixed `dt` for models, paced to wall for real
  devices. It would remove plant jitter in HITL.

