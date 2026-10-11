# adcs-fsw3

A cube-sat attitude control loop on metor-fsw-3. The `adcs` pack holds four
systems: a rigid-body plant in a 400 km orbit with sensors and disturbances, a
MEKF filter, an LQR controller that drives reaction wheels and magnetorquers,
and a `mode` system that holds the commanded phase and pointing law. The `seqs`
pack holds `commission`, a sequence that detumbles and then holds nadir. It
builds as a wasm module as well as a library.

```
contracts/        adcs-contracts-3: frames, params, and shared physics
systems/adcs/     adcs-systems-3: plant, nav, ctrl, mode, and pack()
systems/seqs/     adcs-seqs-3: commission and pack()
sim.py            the whole loop in one process, with mode
target.py         a deployment of three targets: plant, fsw, and a gateway;
                  fsw runs commission as a wasm module
tests/            the convergence and deployment tests
```

## Running

To run the loop in one process at 120 Hz on the wall clock:

```
cargo run -p metor-fsw-3 --bin metor -- run examples/adcs-fsw3/sim.py
```

`--cycles N` stops after N cycles, and `--sim-dt 0.008333333` steps a simulated
clock instead of the wall clock. The target publishes everything on
`0.0.0.0:2240`.

The deployment runs the plant, the flight software, and a gateway that records
both, each as a child process:

```
cargo run -p metor-fsw-3 --bin metor -- run examples/adcs-fsw3/target.py
```

Connect a panel to the gateway on `127.0.0.1:2250`; the `commission` channel
takes its commands through it. The plant and the flight software listen on 2240
and 2241. `METOR_DEPLOY_PORTS=a,b,c` overrides all three ports, which lets the
tests pick free ones.
`metor build examples/adcs-fsw3/target.py` prints the resolved deployment
without running it.

## Tests

`cargo test -p adcs-fsw3` runs both tests. `tests/convergence.rs` builds
`sim.py`'s loop twice, once with the pack linked as an rlib and once loaded from
the cdylib that `pack dev` staged. Each run must aim the body `-Y` axis at
Earth, and the two runs must command identical torques. `tests/deploy.rs` runs
`target.py` under `metor run` and checks what the gateway's db holds.

## Known allocation

The plant and the filter both call `MagneticModel::calculate_field` every
cycle, and the vendored WMM C library allocates its Legendre and
spherical-harmonic workspaces inside that call. Reusing them needs a patch to
`libs/wmm`. Until then, each caller allocates once per cycle.

## Not ported

`examples/adcs-fsw2` also has a safe-mode sequence, alarms, presets and
dashboards, the Python expression system, and uplink routes. fsw-3 has none of
these yet.
