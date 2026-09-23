# adcs-fsw3

A cube-sat attitude loop on metor-fsw-3: a rigid-body plant with a real
400 km orbit, sensors and disturbances; a MEKF filter; an LQR controller
with wheel and magnetorquer commands; and a `mode` system holding the
commanded phase and pointing law. All four are one pack, `adcs`.

```
contracts/        adcs-contracts-3: the frames, the params, the physics
systems/adcs/     adcs-systems-3: plant, nav, ctrl, mode, and pack()
sim.py            the whole loop in one process
target.py         three targets: plant, fsw, and a gateway
tests/            the convergence gate and the deployment gate
```

## Running

The loop in one process, at 120 Hz on the wall clock:

```
cargo run -p metor-fsw-3 --bin metor -- run examples/adcs-fsw3/sim.py
```

`--cycles N` stops after N cycles and `--sim-dt 0.008333333` steps a
simulated clock instead of pacing a real one. The target publishes
everything on `0.0.0.0:2240`.

The deployment — the vehicle on `plant`, the flight software on `fsw`,
and a gateway whose embedded db records both — runs every member as a
child process:

```
cargo run -p metor-fsw-3 --bin metor -- run examples/adcs-fsw3/target.py
```

Connect a panel to the gateway on `127.0.0.1:2250`; the members listen
on 2240 and 2241. `METOR_DEPLOY_PORTS=a,b,c` overrides all three, which
is how the tests pick free ones. `metor build examples/adcs-fsw3/target.py`
prints the resolved envelope without running anything.

## Tests

`cargo test -p adcs-fsw3` runs both gates. `tests/convergence.rs` builds
`sim.py`'s loop twice — once with the pack linked as an rlib, once
through the cdylib `pack dev` staged — and checks that each converges
onto its nadir target — the body `-Y` axis aimed at Earth — and that the
two command the same torques.
`tests/deploy.rs` runs `target.py` under `metor run` and asks the
gateway's db what it holds.

## Known allocation

Both the plant and the filter call `MagneticModel::calculate_field` every
cycle, and the vendored WMM C library allocates its Legendre and
spherical-harmonic workspaces inside that call. Reusing them takes a patch
to `libs/wmm`, so until then this example allocates once per cycle per
caller.

## Not ported

The fsw-2 example, at `examples/adcs-fsw2`, has more than this one: the
`mode` slot and its commissioning and safe-mode sequences, alarms,
presets and dashboards, the Python expression system, and uplink routes.
None of those exist in fsw-3 yet, so `mode` here holds one fixed command
for the whole run.
