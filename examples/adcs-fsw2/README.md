# adcs-fsw2 — closed-loop ADCS on metor-fsw-2, with `dlopen`'d systems

A self-contained spacecraft attitude-determination-and-control mission built on the
`metor-fsw-2` framework: a plant/dynamics system, an MEKF navigation filter, and a
Yang-LQR controller in a closed feedback loop (reusing the `metor-fsw/adcs` math and
`nox` six-dof dynamics), commissioned by a runtime **sequence slot**.

```text
  plant ──sensors / gps──▶ nav ──attitude_estimate──▶ ctrl
    ▲                 │                                ▲ │
    │   gps / sensors / wheels ────────────────────────┘ │
    │                 └──attitude_estimate──▶ mode ──mode_cmd──▶ ctrl
    └────────── torque_cmd + mtq_cmd ───────────────────┘   (one-cycle-delayed)
```

The **plant** propagates a real 400 km orbit (point-mass gravity + drag) and the full Euler
attitude dynamics — `I·ω̇_b = τ_b − ḣ_w − ω_b × (I·ω_b + h_w)`, including the gyroscopic
coupling of the stored reaction-wheel momentum — under the **disturbance environment** a
small spacecraft actually lives on: gravity-gradient, aerodynamic drag about the CP–CG
offset (the drag force also perturbs the orbit), residual magnetic dipole, and SRP, which
switches off in **Earth shadow** (a cylindrical-umbra eclipse model; the boot orbit phase is
a plant param, so a run can start in or near the ~35-minute shadow arc). Two actuators drive
it: three **reaction wheels** (bearing friction and momentum-saturation foldback inside the
dynamics, physical signed momentum, per-wheel arming) and three **magnetorquers**
(`τ = m × B`, per-axis dipole clamp). It emits a simulated sensor suite (gyro with bias
walk, a six-head **coarse sun sensor** — one cosine-response photodiode per body face, each
FOV-clamped and dark in eclipse, publishing raw per-head readings — and a magnetometer
reading the physical **NOAA WMM** field in Tesla), a noisy **GPS** measurement (a
first-order Gauss-Markov position error + white velocity noise — the orbit state the flight
software flies on), reaction-wheel and per-source **disturbance-torque** telemetry, the
ground-truth **body** state (true attitude + body rate + ECI orbit), and the **world**
environment — the true ECI sun direction (nox-frames' Vallado model at the mission epoch),
the WMM magnetic field the sensors observe, and the illumination truth flag.

The **nav** filter reconstructs the sun vector from the raw CSS readings (opposed-pair
differencing; the sun is declared lost when every head sits at its noise floor — the FSW's
own call, from its sensor), models its own inertial references — the sun from the ephemeris
at its sim-time epoch, the field from the WMM evaluated at the **GPS** position — and runs
the MEKF, dropping to **magnetometer-only** through an eclipse (it never sees the plant's
truth). The **ctrl** controller commands both actuators from what the FSW measures: the
Yang-LQR body torque toward the **pointing law** the `mode` slot commands (`ModeCmd.law` —
Nadir, velocity-vector/HIL, or magnetorquer-only Detumble), its target computed from the
**GPS** orbit measurement and clamped to the wheel motor limit; and the magnetorquer dipole
— cross-product momentum **desaturation** (`k·(h_w × B)/|B|²`, always on, fed by the
telemetered wheel momentum and the measured field) or B-cross rate damping in Detumble. Both
command frames close the loop back into the plant one cycle delayed.

The `mode` **slot** auto-runs the `commissioning` sequence at startup — a **condition-based
ladder**, not a script of timed waits: estimator warm-up (gated on successive q̂ deltas
settling), an optional magnetorquer detumble (entered only above a rate the wheels shouldn't
capture; the boot tumble is not), then coarse and fine pointing gated on the live tracking
error to the velocity-vector target, each phase under a timeout that safes the spacecraft
and fails the sequence. Its gates and budgets are sequence **params** on the mission's
`allow` line. A `safe_mode` sequence is the second allowed occupant (Loaded by an operator
to drop to a nadir-pointing safe state).

## Crate layout

| Crate | Path | Role |
|---|---|---|
| `adcs-contracts` | `contracts/` | The shared compile-time contract: the frame structs (sensors / gps / body / world / attitude_estimate / mode_cmd / torque_cmd / mtq_cmd / wheels / disturb), the per-system `Params`, and only the physics **both sides** rely on — orbital constants + inertia, the actuator envelope (wheel torque/momentum limits, torquer dipole limit), the WMM magnetic-field + sun-direction models (the plant's truth *and* nav's references), the Nadir/HIL pointing laws, and the desat/detumble magnetorquer laws. Linked by the cdylibs (and the test), **not** by the host. |
| `adcs-systems` | `systems/adcs-systems/` | The three cyclic systems as **one pack in one cdylib** (`fn pack()` + `export_pack!`): the orbiting rigid-body **plant** and its physics (wheel dynamics, disturbance-torque model, GPS/magnetometer error models, magnetorquers, sensor suite), the MEKF **nav** filter (`sun_from_css` reconstruction, its own sun/WMM references at the GPS position, magnetometer-only through eclipses), and the Yang-LQR + magnetorquer-law **ctrl** controller. Plant and nav are fn-authored (state struct + `init` fn + `execute` fn); ctrl deliberately stays `#[system]` struct-authored so the pack exercises both styles. |
| `adcs-sequences` | `systems/adcs-sequences/` | Both async-fn sequence occupants of the `mode` slot as one pack cdylib: the condition-based commissioning ladder (params on the `allow` line) and the one-shot safing drop. |
| `adcs-fsw2` | (this crate) | The mission **host**: builds + `dlopen`s the cdylibs and runs the coordinator. Links only `metor-fsw-2` — it is fully schema-agnostic (frames validated from serialized VTables, params encoded from each `.so`'s exported schema). |

`adcs-systems` is `crate-type = ["cdylib", "rlib"]`: the cdylib is what the host loads; the
rlib lets the convergence test register the **same** `pack()` statically for the parity
check. The `export_pack!` C-ABI symbols ride an `export` feature (on by default for the
cdylib, off when the test links the rlib) so the rlib carries no `fsw_pack_*` exports.

## Two front-ends

The mission is described two equivalent ways. `mission.kdl` is the committed default the CLI
runner and every tracked test read. `mission.py` re-states it through the `metor_config`
Python front-end (`libs/metor-fsw-2/python`): `metor-fsw build|run mission.py` spawns a
subprocess CPython that evaluates the file against the recorder and emits the same `Wiring`
IR the KDL parser produces. `tests/equivalence.rs` asserts the two resolve to equivalent
wirings. `mission.py` needs only a stock `python3` (≥ 3.10) — no `pip install`, since the
`metor-fsw` binary embeds the recorder.

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
   damping, `plant.wheels.wheels.0.ang_momentum` a reaction wheel's stored momentum (physical
   and signed — the frame field is also named `wheels`, hence the doubled path),
   `ctrl.torque_cmd.torque_b` / `ctrl.mtq_cmd.dipole_b` the two actuator commands,
   `plant.disturb.total_b` the summed environmental torque next to its per-source parts (and
   `plant.disturb.mtq_b` the applied magnetorquer torque), `plant.gps.pos_eci` against
   `plant.body.pos_eci` the GPS position error, and `plant.world.sun_eci` /
   `plant.world.mag_eci` the real ECI sun direction and WMM magnetic field (the magnetometer
   `plant.sensors.mag_b` reads the same field in Tesla, body frame; `plant.sensors.css` the
   six raw sun-sensor heads, and `plant.world.illuminated` the eclipse truth flag they go
   dark under). The sequence view
   shows `commissioning` stepping to completion; Load/Start `safe_mode` from there to command
   nadir safing. The alarm view carries `ADCS_RATE_HIGH` (body rate) and `RW_MOMENTUM_HIGH`
   (wheel-0 stored momentum vs the saturation limit).

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

`tests/closed_loop.rs` runs the **same** `mission.kdl` two ways — `plant`/`nav`/`ctrl`
registered statically (the rlib's `pack()` into a `Registry`) and the same pack `dlopen`'d
from its cdylib — with the `mode` slot's sequences dlopen in both. It asserts both converge onto the
commanded pointing target and that the dlopen run matches the static one **bit-for-bit** (same
systems, same params, same seed, just loaded vs linked). `tests/momentum.rs` preloads the
wheels and asserts the desaturation law dumps stored momentum through the torquers while the
mission keeps pointing (and that the `RW_MOMENTUM_HIGH` alarm's nested target path resolves
live). `tests/eclipse.rs` boots inside / just before the Earth-shadow arc (the entry phase
is computed, not hardcoded) and asserts the CSS heads go dark, SRP switches off, and the
magnetometer-only estimate rides through the sun loss. `tests/sequences.rs` exercises the `mode` slot end-to-end (auto-run, interactive
Load→Start→Abort, and the downlinked sequence events); `tests/bundle.rs` checks the mission
bundles and runs. All build real cdylibs, so they are slower than plain unit tests and are
gated off `miri`. The physics itself is unit-tested underneath: the WMM band, shadow
geometry, and magnetorquer laws in `adcs-contracts`, and the wheel model, disturbance
magnitudes, total-angular-momentum conservation + the detumble law against the plant's own
`propagate` in `adcs-systems`.
