# Plan: real GPS sensor (error model) + WMM magnetic field

Two realism upgrades to `adcs-fsw2`, both confirmed with the user:

1. **GPS sensor** the controller flies on (was: ctrl flew on the truth `body` frame).
2. **WMM** (NOAA World Magnetic Model, `libs/wmm`) for the true + reference magnetic field,
   replacing the crude tilted-dipole `mag_field_eci`/`k0()`.

User scoping decisions:
- **ctrl flies on GPS** — a new noisy `gps` frame drives the pointing-law target.
- **nav self-references** — nav computes its own sun + WMM-mag references from the GPS
  position and its own sim-time epoch, dropping its consumption of the truth `world` frame.

## GPS error model (the "good" basic model)

First-order **Gauss-Markov** (exponentially-correlated) position error per axis — the textbook
GPS model (Brown & Hwang), since GPS position error is dominated by slowly-varying
iono/ephemeris terms, not white noise:

    e[k+1] = φ·e[k] + w,   φ = exp(-DT/τ),   w ~ N(0, σ_pos²·(1-φ²))

with σ_pos ≈ 5 m, τ ≈ 100 s. Velocity error is **white** (~0.05 m/s per axis) — Doppler/
carrier-derived velocity is far less correlated. Constants live in `contracts` with the physics.

The GM state (`gps_pos_err: V3`) is stateful → lives in the plant. New RNG draws are **appended
after** the existing bias/gyro/sun/mag draws so existing sensor-noise bytes stay identical.

Position noise (~5 m over a ~6778 km orbit radius → ~1e-6 rad target-direction error) is
negligible vs the ~0.003 rad convergence, so the closed-loop thresholds are unaffected.

## WMM field chain (both plant truth + nav reference)

    pos_eci --eci_to_ecef(epoch)--> ECEF xyz
            --ecef_to_geodetic--> (lat, lon, alt)          [NEW: WGS84 Bowring, in contracts]
            --WMM.calculate_field(epoch, geodetic)--> NED field (Tesla)   [height in KM, lat/lon deg]
            --ned_to_ecef(lat,lon)--> ECEF field
            --ecef_to_eci(epoch)--> mag_eci (Tesla)

All deterministic (nox-frames IERS table is baked in via `include_str!`, no wall clock) → the
bit-exact static≡dlopen parity test still holds. WMM is valid to ~850 km; a 400 km orbit is in
range. `MagneticModel` (holds C state, `&mut self`, not Clone) is built once per system.

## Changes by file

- **contracts**: add `wmm` dep + re-export `MagneticModel`; drop `k0()`/dipole `mag_field_eci`;
  add `ecef_to_geodetic`; add `mag_field_eci(&mut MagneticModel, Epoch, &V3) -> V3` (WMM);
  add GPS constants (`GPS_POS_SIGMA`/`GPS_TAU`/`GPS_VEL_SIGMA`); add `Gps` frame
  `{ timestamp, pos_eci, vel_eci }`; add a unit test asserting a sane field magnitude at 400 km.
- **plant**: hold `MagneticModel` + `gps_pos_err`; true field via WMM → `world.mag_eci` +
  `mag_b` sensor; emit new `Gps` frame (GM pos + white vel); append GPS RNG draws.
- **nav**: inputs `sensors` + `gps` (drop `world`); hold `MagneticModel` + `t_sim` counter;
  compute sun + WMM-mag references from GPS position at `epoch_at(t_sim)` (lockstep with plant).
- **ctrl**: input `gps` instead of `body`; target from `gps.pos_eci`/`gps.vel_eci`.
- **mission.kdl**: `plant->nav gps` (add), drop `plant->nav world`; `plant->ctrl gps` (was body).
- **tests/closed_loop.rs**: unchanged (taps truth `plant.body`); must still converge + parity.
- **README.md** + **docs/ergonomics-report.md**: diagrams, frame list, field paths, parity table.

## Verify
contracts+systems build; `cargo test -p adcs-fsw2` (closed_loop/sequences/bundle) converges +
static≡dlopen Δ=0; `cargo test -p wmm` + `cargo test -p metor-proto` still green; clippy clean.
