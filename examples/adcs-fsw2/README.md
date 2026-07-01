# adcs-fsw2 — closed-loop ADCS on metor-fsw-2, with `dlopen`'d systems

A self-contained spacecraft attitude-determination-and-control mission built on the
`metor-fsw-2` framework: a plant/dynamics system, an MEKF navigation filter, and a
Yang-LQR controller in a closed feedback loop (reusing the `metor-fsw/adcs` math and
`nox` six-dof dynamics), commissioned by a runtime **sequence slot**.

```text
  plant ──sensors / body──▶ nav ──attitude_estimate──▶ ctrl
    ▲                  │                                ▲ │
    │   body ──────────┴────────────────────────────────┘ │
    │                  └──attitude_estimate──▶ mode ──mode_cmd──▶ ctrl
    └──────────────────── torque_cmd ────────────────────┘   (one-cycle-delayed)
```

The **plant** propagates a real 400 km orbit (point-mass gravity + the orbital velocity) and
the attitude dynamics, driven by a three-wheel **reaction-wheel** actuator (friction +
momentum saturation + per-wheel arming). It emits a simulated sensor suite (gyro with bias
walk, a sun observation, and a dipole-model magnetometer), reaction-wheel telemetry, the
ground-truth **body** state (true attitude + body rate + the ECI orbit/GPS position/velocity),
and the **world** environment — the true ECI sun direction (nox-frames' Vallado model at the
mission epoch) and magnetic field that the sensors observe.

The **nav** filter takes its inertial sun/magnetic references from the `world` frame and runs
the MEKF. The **ctrl** controller follows the **pointing law** the `mode` slot commands
(`ModeCmd.law` — Nadir or velocity-vector/HIL), computing its target attitude from the orbit
state, and produces the body torque that closes the loop back into the plant.

The `mode` **slot** auto-runs the `commissioning` sequence at startup, walking the spacecraft
idle → settling → pointing as the controller drives it onto the velocity-vector target; a
`safe_mode` sequence is the second allowed occupant (Loaded by an operator to drop to a
nadir-pointing safe state).

## Crate layout

| Crate | Path | Role |
|---|---|---|
| `adcs-contracts` | `contracts/` | The shared compile-time contract: the frame structs (sensors / body / world / attitude_estimate / mode_cmd / torque_cmd / wheels), the per-system `Params`, and the shared physics (orbital constants, the magnetic-field + sun-direction models, and the Nadir/HIL pointing laws). Linked by the cdylibs (and the test), **not** by the host. |
| `adcs-plant` | `systems/plant/` | The orbiting rigid-body plant + reaction wheels + sensor suite, a `cdylib` ending in `export_system!(PlantSystem)`. |
| `adcs-nav` | `systems/nav/` | The MEKF filter cdylib (models the sun/mag references from the orbit state). |
| `adcs-ctrl` | `systems/ctrl/` | The Yang-LQR controller cdylib (selects the pointing-law target from `ModeCmd`). |
| `adcs-commissioning` / `adcs-safe-mode` | `systems/commissioning/`, `systems/safe-mode/` | The `#[sequence]` occupants of the `mode` slot. |
| `adcs-fsw2` | (this crate) | The mission **host**: builds + `dlopen`s the cdylibs and runs the coordinator. Links only `metor-fsw-2` — it is fully schema-agnostic (frames validated from serialized VTables, params encoded from each `.so`'s exported schema). |

Each system crate is `crate-type = ["cdylib", "rlib"]`: the cdylib is what the host loads;
the rlib lets the convergence test also link the systems statically for the parity check.
The `export_system!` C-ABI symbols ride an `export` feature (on by default for the cdylib,
off when the test links the rlib) so the system rlibs link into one test binary without a
duplicate `fsw_*` symbol clash.

## Watch it live in metor-panel

`cargo run` first builds the system cdylibs (incremental — only changed crates recompile),
`dlopen`s them, then paces the loop on a wall clock at 120 Hz and streams every output frame
to a running metor-panel over TCP, in metor-proto's wire format — the same format metor-db
ingests.

1. Start **metor-panel** (it boots a metor-db on `127.0.0.1:2240` and opens the UI):
   ```sh
   cargo run -p metor-panel
   ```
2. In another terminal, run the mission — telemetry **down**, command **up**:
   ```sh
   cargo run -p adcs-fsw2 -- run examples/adcs-fsw2/mission.kdl --build --wall \
       --telemetry 127.0.0.1:2240 --uplink 127.0.0.1:2240
   ```
   `--telemetry` streams every frame to the panel; `--uplink` opens a **second** connection
   that ingests the panel's `SequenceCommand`s so you can drive the `mode` slot live
   (Load/Start/Abort `commissioning` or `safe_mode`). Uplink and downlink use separate
   connections (docs/messages.md §4.5) — both point at the same metor-db endpoint.
3. In the panel, the `plant` / `nav` / `ctrl` / `mode` (and `coordinator`) instances appear
   in the component tree. Plot e.g. `nav.attitude_estimate.q_hat_b_eci` against
   `plant.body.q_b_eci` and watch the estimate track truth as the controller slews the
   spacecraft onto the commanded pointing target; `plant.sensors.gyro_b` shows the rate
   damping, `plant.wheels.0.ang_momentum` a reaction wheel's momentum building up,
   `ctrl.torque_cmd.torque_b` the commanded torque, and `plant.world.sun_eci` /
   `plant.world.mag_eci` the real ECI sun direction and magnetic field. The sequence view shows `commissioning`
   stepping to completion; Load/Start `safe_mode` from there to command nadir safing.

The mission converges in ~30 s of real time. The terminal prints only a heartbeat — the
host stays schema-agnostic (it doesn't decode the frames), so convergence is watched in the
panel (or asserted headlessly by `cargo test`). Ctrl-C to stop. If the panel isn't running
the downlink/uplink just fail to connect and the control loop runs unaffected.

> Component names are prefixed by the **instance** name (`nav.attitude_estimate.q_hat_b_eci`),
> so two instances of one system type never collide — the metor-fsw-2 output registry
> applies that prefix when announcing each frame's schema to the panel.

### Booting with the reaction wheels disarmed

Set `disarmed=#true` on the `plant` system in `mission.kdl` to bring the spacecraft up with
every reaction wheel offline (the cube-sat `--disarmed` parity): the plant applies no control
torque until the wheels are armed, so the spacecraft tumbles freely. (Live operator arm/disarm
of individual wheels is a panel-command surface metor-fsw-2 does not yet expose — see the
ergonomics report.)

## Headless test (no panel needed)

```sh
cargo test -p adcs-fsw2     # builds the cdylibs, then asserts convergence + parity
```

`tests/closed_loop.rs` runs the **same** `mission.kdl` two ways — `plant`/`nav`/`ctrl` linked
statically (rlibs, resolved via a `Registry`) and the same systems `dlopen`'d from their
cdylibs — with the `mode` slot's sequences dlopen in both. It asserts both converge onto the
commanded pointing target and that the dlopen run matches the static one **bit-for-bit** (same
systems, same params, same seed, just loaded vs linked). `tests/sequences.rs` exercises the
`mode` slot end-to-end (auto-run, interactive Load→Start→Abort, and the downlinked sequence
events); `tests/bundle.rs` checks the mission bundles and runs. All build real cdylibs, so
they are slower than plain unit tests and are gated off `miri`.
