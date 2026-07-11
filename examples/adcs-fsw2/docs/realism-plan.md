# Plan: disturbance torques + magnetorquers + desaturation, and honest wheel physics

The second realism arc after `gps-wmm-plan.md`, all confirmed with the user:

1. **Environmental disturbance torques** — gravity-gradient, aero drag (force also on the
   orbit), residual magnetic dipole, SRP. Without them wheel momentum never builds, so
   nothing in the mission ever *needs* managing.
2. **Magnetorquers** — a second actuator (`τ = m × B`), carrying **desaturation** (the
   cross-product momentum-dumping law) and a **B-cross detumble** law (`LAW_DETUMBLE`,
   selectable but not yet commanded by any sequence — the commissioning rewrite is a later
   arc).
3. **Wheel/body momentum physics fixed** — the full Euler equations with the stored wheel
   momentum coupled in, friction inside the dynamics, physical sign conventions, and an
   exact-limit saturation foldback.

User scoping decisions: include the detumble law now; un-normalize `Sensors.mag_b` to
physical Tesla (real magnetometers measure magnitude, and the desat law needs `|B|`);
physically honest disturbance defaults with param knobs the tests/demos crank. Plus, from
review: fix the missing Euler term in `nox` itself rather than routing around it.

## Physics & sign conventions

Quaternion convention matches the existing code: `v_eci = q_b_eci ⊛ v_b`.

- Rotational dynamics (in `nox::six_dof` now — it previously integrated `ω̇ = τ/I`
  element-wise with **no** `ω × Iω` term, applying body-diag inertia to a world-frame rate):
  `I ω̇_b = τ_b − ω_b × (I ω_b)`, torque world-frame at the API, rotated internally. The
  wheel coupling rides the plant's effector: `τ_b −= ω_b × h_w` per RK4 substep, with
  `h_w` at the **step midpoint** (`h_post + rw_torque·DT/2` — the wheels update before the
  body steps; one-sided bookkeeping loses total angular momentum secularly, ~2.6% per
  40 s run vs ~1e-5 centered).
- Per-wheel (signed scalar `s` along the body-fixed unit axis; `ang_momentum = s·axis` is
  now the wheel's **physical** momentum — the sign convention flipped):
  - motor on wheel `τm = clamp(−u, ±RW_TORQUE_MAX)` if armed (`u` = commanded body torque
    along the axis), friction `−RW_COULOMB·sign(Ω) − RW_VISCOUS·Ω` outside a stiction
    deadband (may stop the wheel within a step, never reverse it),
  - saturation: `ḣ` clamped so `|s|` lands exactly on `RW_MOMENTUM_MAX` (unloading always
    flows — replaces the old snap-to-zero),
  - reaction on the body `= −ḣ·axis` (covers motor **and** friction).
  - The friction coefficients are retuned (1e-5 / 1e-7): the cube-sat values were
    telemetry-only and would cap the wheel near 40 rad/s if fed into the dynamics.
- Invariant: `L_eci = q ⊛ (I∘ω_b + h_w)` is conserved with disturbances/MTQ off —
  `adcs-plant`'s conservation test.
- Disturbances (deterministic, zero new RNG draws, evaluated at the pre-step true state):
  - gravity gradient `τ = (3μ/|r|³)·r̂_b × (I∘r̂_b)` (~3.5e-8 N·m),
  - aero `F = −½ρ·Cd·A·|v|·v` (co-rotating atmosphere ignored), `τ = r_cp × F_b`
    (~1e-7 N·m); the force also enters `v̇`,
  - residual dipole `τ = m_res × B_b` (Tesla, un-normalized WMM),
  - SRP `τ = r_cp × (−P·A·Cr·ŝ_b)` — always lit, no eclipse model yet.
- Magnetorquer laws (both `k·(x × B)/|B|²`-shaped, so `τ = m × B = −k·x_⊥`):
  - desat `x = h_w` (telemetered wheel momentum, measured field) — always on outside
    detumble; only the field-perpendicular component is dumpable per instant, the orbit
    rotates `B̂` to reach the rest,
  - detumble `x = ω` (gyro-based **B-cross** — classic Ḃ-based B-dot is noise-dominated at
    a 120 Hz sample rate).

## Changes by file

- **nox**: `DU::from_body_force` does the full Euler equations (world-frame force API,
  documented); regression test pins `L_w`/KE conservation of a torque-free anisotropic
  tumble. cube-sat's call site rotates its body-frame wheel torque accordingly.
- **contracts**: RW_*/MAG_SENSOR_SIGMA/MTQ_MAX_DIPOLE/P_SRP/MU constants; `ReactionWheel`
  rewritten (wire layout unchanged); `Disturbances` + `MtqCmd` frames; `LAW_DETUMBLE`;
  `desat_dipole`/`detumble_dipole`/`clamp_dipole`; `disturbance_torques` (the shared
  model); `PlantParams` disturbance knobs + `init_wheel_h`; `CtrlParams` gains.
- **plant**: `mtq` input + `disturb` output; Tesla magnetometer; disturbances applied +
  telemetered; `propagate` free fn (six_dof_rk4 + per-substep wheel coupling) so the
  physics tests skip the port harness.
- **ctrl**: `sensors`/`wheels` inputs + `mtq` output; the LQR output rotated **into the
  body frame** (the Yang recipe's left error quaternion and ECI rate make its torque
  ECI-frame — the old plant applied it un-rotated, self-consistently wrong) and clamped
  per axis to `RW_TORQUE_MAX`; law dispatch (detumble idles the wheels).
- **nav**: normalizes the Tesla `mag_b` before the MEKF.
- **mission.kdl**: plant params spelled out (dlopen schema encoding has no serde
  defaults); `k_desat`/`k_detumble`; `plant→ctrl` sensors+wheels edges; `ctrl→plant`
  `mtq_cmd` delayed back-edge; `RW_MOMENTUM_HIGH` alarm on
  `plant.wheels.wheels.0.ang_momentum`.
- **tests**: contracts (saturation, friction decay, disturbance magnitude bands, law
  signs), plant (L conservation, detumble damps a tumble), `tests/momentum.rs` (desat-on
  vs desat-off dump margin + live alarm-path resolution + mission stays converged).

## Found along the way (fixed in their own commits)

- `nox::six_dof` had no Euler coupling at all (above).
- A disconnected `TcpDownlink` stalled **every** ring at depth from boot (its ReceiveAll
  tap views stopped draining when the batch queue filled behind the reconnect backoff) —
  the whole mission flew on frozen data whenever no panel was listening.
- Canceling a task parked in `stellarator::sleep` corrupted the maitake timer wheel
  (cancel's lost wake + future kept alive until a stale-waker deallocation + a refcount
  double-decrement) — the downlink's parked sender tripped it at coordinator teardown.
- The wheel-preload scenario is honest about *why* desat exists: with ~0.06 N·m·s stored,
  the gyroscopic coupling of even a modest tumble exceeds the wheels' 2e-3 N·m authority
  and the spacecraft is genuinely uncontrollable — the momentum test preloads a
  controllable 0.017 N·m·s and scales the alarm band around it instead.

## Verify

`cargo test -p nox` (Euler regression), `-p adcs-contracts`, `-p adcs-plant
--no-default-features` (physics units), `-p adcs-fsw2` (closed_loop convergence +
static≡dlopen bit parity, momentum, sequences, alarms, bundle), `-p metor-fsw-2`
(telemetry drain regression), `-p stellarator`/`-p maitake` (cancel-while-sleeping),
clippy clean across all touched crates.
