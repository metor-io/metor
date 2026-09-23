# The ADCS example

`examples/adcs-fsw3`: the cube-sat control loop from `examples/adcs-fsw2`
ported to fsw-3, as one pack, one single-process target for simulation,
and one three-target deployment with a gateway. It is the convergence
gate slice 3 deferred and the first real deployment slice 7 runs.

Ported: the contracts (frames, params, physics), the `plant`, `nav`, and
`ctrl` systems, the closed loop, the process split with a gateway, and a
convergence test. Written fresh: a `mode` system that publishes a fixed
`ModeCmd`, since slots and sequences are slice 5.

Skipped, with the fsw-2 source left where it is: sequences and the slot,
alarms, presets and dashboards, the Python expression system
(`gyro_norm`), uplink routes, and the fsw-2 host crate's bundle tests.

## Layout

```
examples/adcs-fsw3/
  Cargo.toml            package `adcs-fsw3`: no lib code, holds the tests
  pyproject.toml        depends on adcs-pack and metor-config by path
  contracts/            crate `adcs-contracts-3`: frames, params, physics
  systems/adcs/         crate `adcs-systems-3`, pack id `adcs`:
                        plant.rs nav.rs ctrl.rs mode.rs lib.rs (pack())
    pyproject.toml      [tool.metor.pack] id = "adcs", module adcs_pack
  sim.py                one Target: plant, nav, mode, ctrl
  target.py             Deployment: plant, fsw, gw
  tests/convergence.rs  the gate
```

The three crates join the workspace. Crate names carry a `-3` suffix so
they can sit beside the fsw-2 crates until those are deleted; the pack
id and Python module do not.

## Contracts

`contracts/src/lib.rs` is `examples/adcs-fsw2/contracts/src/lib.rs` with
the derives swapped and nothing else changed:

- Every frame: `#[derive(Frame, IntoBytes, Immutable, KnownLayout,
  FromBytes, Clone, Copy, Debug)]`, `#[frame(name = "..")]`,
  `#[frame(timestamp)]`, `#[frame(nest)]` on `Wheels::wheels`.
  `ReactionWheel` keeps its standalone component derives from
  `metor-component`.
- Every params struct: `Deserialize, JsonSchema` in place of
  `Serialize, Schema, ParamsDocs`. `CtrlParams` loses its `Default`
  impl for `#[serde(default = "..")]` on each field, which is what fills
  the generated module's defaults.
- The physics functions, constants, and the four unit tests come over
  unchanged. Dependencies: `metor-fsw-3`, `metor-component`, `nox`,
  `nox-frames`, `wmm`, `hifitime`, `zerocopy`, `serde`, `schemars`.

Frame and port names are the fsw-2 ones, so every component path a
panel layout names (`plant.sensors.gyro_b`, `plant.wheels.wheels.0
.ang_momentum`, `nav.attitude_estimate.q_hat_b_eci`, `mode.mode_cmd`)
reads the same under fsw-3's `{namespace}.{instance}.{port}.{field}`
rooting, given the instance ids `plant`, `nav`, `ctrl`, `mode`.

## Systems

Each state struct keeps its `new(params)` constructor and gains a
`#[system] impl` block whose `execute` takes `&mut self` first:

```rust
#[system]
impl PlantState {
    /// Steps the vehicle one cycle under the newest commands.
    fn execute(&mut self, now: Timestamp,
               torque_cmd: &mut Input<TorqueCmd>, mtq_cmd: &mut Input<MtqCmd>,
               sensors: &mut Output<Sensors>, gps: &mut Output<Gps>,
               wheels: &mut Output<Wheels>, body: &mut Output<BodyState>,
               world: &mut Output<World>, disturb: &mut Output<Disturbances>) { .. }
}
```

Mechanical changes from fsw-2: `state: &mut S` becomes the receiver;
`Output::publish(&f)` becomes `Output::write(&f)` with its result
dropped; `Input::latest()` still yields `Ok(Some(..))`, now a `Latest`
that derefs to the frame, so `.clone()` off it stays; `tracing::info!`
lines stay and reach `log`. Input port names on `plant` are
`torque_cmd` and `mtq_cmd`, on `ctrl` `estimate`, `gps`, `mode`,
`sensors`, `wheels`, as fsw-2 named them.

`mode.rs` is new: `Mode` with params `{ mode: u8 = POINTING, law: u8 =
LAW_NADIR }` and one output `mode_cmd`, writing `ModeCmd::stamped(now)`
with those bytes every cycle. Registration:

```rust
pub fn pack() -> SystemTable {
    let mut table = SystemTable::new();
    table.register("plant", PlantState::new).expect("valid records");
    table.register("nav", NavState::new).expect("valid records");
    table.register("ctrl", CtrlSystem::new).expect("valid records");
    table.register("mode", Mode::new).expect("valid records");
    table
}
#[cfg(feature = "export")]
metor_fsw_3::export_pack!(pack);
```

`crate-type = ["cdylib", "rlib"]` with `export` default on, as fsw-2
had it; the rlib build in the test turns it off. The crate depends on
`metor-fsw-3` with `default-features = false`. The plant and nav unit
tests come over unchanged.

## Targets

`sim.py`, the loop in one process, is the slice-3 shape:

```python
fsw = Target(cycle_rate=120.0, namespace="sim")
torque = fsw.loop(TorqueCmd)
mtq = fsw.loop(MtqCmd)
plant = fsw.add("plant", Plant(torque_cmd=torque, mtq_cmd=mtq, <fsw-2 params>))
nav = fsw.add("nav", Nav(sensors=plant.sensors, gps=plant.gps, meas_sigma=0.02))
mode = fsw.add("mode", Mode())
ctrl = fsw.add("ctrl", Ctrl(estimate=nav.attitude_estimate, gps=plant.gps,
                            mode=mode.mode_cmd, sensors=plant.sensors,
                            wheels=plant.wheels, <fsw-2 gains>))
torque.connect(ctrl.torque_cmd)
mtq.connect(ctrl.mtq_cmd)
fsw.add("pub", Publish(listen="0.0.0.0:2240", all=True))
```

`target.py` is the fsw-2 deployment over slice-7 primitives, and needs
loops for the same reason the fsw-2 file needed `delayed=True`: the
plant reads commands that a later-defined target produces.

```python
plant = Target(cycle_rate=120.0, namespace="plant")
torque, mtq = plant.loop(TorqueCmd), plant.loop(MtqCmd)
sim = plant.add("plant", Plant(torque_cmd=torque, mtq_cmd=mtq, ..))
plant_pub = plant.add("pub", Publish(listen="0.0.0.0:2240", items=[sim]))

fsw = Target(cycle_rate=120.0, namespace="fsw")
mirror = fsw.add("plant", Subscribe(peer=plant_pub, items=[sim]))
nav = fsw.add("nav", Nav(sensors=mirror.port(sim.sensors), gps=mirror.port(sim.gps), ..))
mode = fsw.add("mode", Mode())
ctrl = fsw.add("ctrl", Ctrl(estimate=nav.attitude_estimate, gps=mirror.port(sim.gps), ..))
fsw_pub = fsw.add("pub", Publish(listen="0.0.0.0:2241", all=True))

cmds = plant.add("ctrl", Subscribe(peer=fsw_pub, items=[ctrl.torque_cmd, ctrl.mtq_cmd]))
torque.connect(cmds.port(ctrl.torque_cmd))
mtq.connect(cmds.port(ctrl.mtq_cmd))

gw, _ = gateway("gw", peers=[plant_pub, fsw_pub], listen="0.0.0.0:2250")
Deployment(targets=[plant, fsw, gw])
```

The mirror is named `plant` so the dashboards' paths read the same on
the `fsw` member, as fsw-2 did; its copies keep their origin's schema,
so the gateway holds one table per plant frame.

## Gate

`tests/convergence.rs` evaluates `sim.py` through `cli::run::prepare`
(`pack dev` refreshes the pack), overrides the clock to `Simulated {
dt: DT }`, and builds twice: once with `adcs_systems_3::pack()`
registered statically under the `adcs` prefix, once through
`Pack::open` on the cdylib `pack dev` staged. Each run steps a fixed
cycle count and asserts, through sink systems the test registers beside
the pack:

- the pointing error from `nav.attitude_estimate` against the nadir
  target falls below `convergence_sample`'s bound within the count;
- `nav.log` carries the first-fix line;
- the two runs' `ctrl.torque_cmd` streams are identical.

`tests/deploy.rs` runs `target.py` under `metor run --cycles N` with
ports from `METOR_DEPLOY_PORTS`, as the fixture gate does, and checks
the gateway's db announces `plant.sensors.gyro_b` and
`fsw.nav.attitude_estimate.omega_b`. Both tests skip when `wmm`'s C
library cannot build, as the fsw-2 tests do.

## Tasks

1. Workspace members, `contracts` crate, unit tests green.
2. `systems/adcs` crate: the four systems, `pack()`, `export_pack!`,
   `pack dev` renders the module (golden not pinned; the fixture's is).
3. `sim.py`, `target.py`, `pyproject.toml`; `metor run sim.py --cycles
   600 --sim-dt 0.00833` exits zero.
4. `tests/convergence.rs` and `tests/deploy.rs`.
5. `README.md` for the example; `TODO.md` slice 3's T7 note closed.

Two agents: one for tasks 1 and 2, one for 3 to 5 once the pack builds.
Each task one commit, tests and clippy green, fmt on touched files.
