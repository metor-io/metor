# Plan: eclipse + condition-based commissioning (realism arc 4)

Landed 2026-07-11. Two deviations from the plan as written, both measured during
implementation: the warm-up phase is where the boot tumble actually damps (ctrl holds its
identity reference before any ModeCmd, so warm-up takes ~5 s and the rate at its exit is
~0.1 rad/s — the detumble skip-gate reasoning holds, just later than sketched); and the
shadowed-boot nav assertion is an absolute no-divergence cap (< 1 rad) rather than
"bounded by boot error + 0.1" — a cold-start mag-only filter under closed-loop feedback
legitimately wanders (~0.23 → ~0.34 rad measured) since ctrl moves the truth underneath
the unobserved about-B̂ axis.

The next realism arc after `realism-plan.md` (disturbances/MTQ/desat) and `gps-wmm-plan.md`.
Two work items, coupled through the `Sensors` frame and the sequence tests:

1. **Eclipse** — a deterministic Earth-shadow model in the plant; the sun sensor goes
   invalid in shadow; SRP gates off; nav runs the MEKF magnetometer-only through the outage.
2. **Condition-based commissioning** — the `commissioning` sequence rewritten from open-loop
   timed waits to state-gated phase transitions (detumble → coarse point → fine point →
   Completed) with per-phase timeouts that safe the spacecraft.

## Scope decisions to confirm (flagged, with recommendations)

1. **Shadow model: cylindrical umbra.** At 400 km cone vs cylinder differ <1 s of a ~2100 s
   eclipse arc; penumbra transit ~8 s — both below this target's fidelity. Documented
   non-goals.
2. **Six-head CSS, restored from cube-sat and finished properly** (user-confirmed — the
   port dropped it). cube-sat (`main.rs:123-158`) modeled one cosine-response photodiode
   per body face (±X/±Y/±Z, 90° half-angle FOV each) but its FSW then consumed the
   truth-rotated sun vector (`sun_vec: sun_pos_b // TODO: do this more legit`) instead of
   reconstructing from the readings. This arc does it legit end-to-end: the plant publishes
   only the six readings — `max(0, n̂ᵢ·ŝ_b)·illuminated + noise` per head — and **nav**
   reconstructs the sun vector by opposed-pair differencing,
   `ŝ_b ≈ normalize([r₊ₓ−r₋ₓ, r₊ᵧ−r₋ᵧ, r₊𝓏−r₋𝓏])`. Six orthogonal 90° cones cover the
   full sphere (any unit vector has a component ≥ 1/√3 ⇒ a lit reading ≥ 0.577), so FOV is
   genuinely modeled per head while sun availability still reduces to illumination — as
   real targets design it.
3. **Validity is derived FSW-side, not a plant-provided bit.** With real readings there is
   no `sun_valid` field: nav declares the sun lost when every reading is below
   `CSS_THRESHOLD` (0.1 ≈ 50σ of head noise, ~6× below the minimum lit reading) — the
   "intensity above threshold" logic real CSS electronics implement, and it detects eclipse
   from the sensor itself. The `World.illuminated` truth flag stays for the panel to plot
   against nav's behavior.
4. **Sequence tracking-error gate: add `Input<Gps>` to the `mode` slot contract.** The
   sequence computes `target_for(law, gps)` + `angular_distance` from contracts fns it
   already links. Slot contracts are shared port-for-port, so `safe_mode` gains an unused
   `_gps` in the same position. (Rejected alternative: a ctrl-published tracking-status
   frame — cleaner layering, more plumbing for one consumer.)
5. **Detumble-phase gating: rate-threshold, wheels-capture-sized.** Enter only above
   |ω̂| > 1.0 rad/s, exit below 0.8 (capturing 1.0 rad/s loads the worst axis to ≈38% of
   RW_MOMENTUM_MAX — comfortable; B-cross is reserved for rates the wheels shouldn't
   capture). The 0.15 rad/s boot tumble skips the phase, so the 33 s closed_loop window
   survives with `init_rate` unchanged. Gating on wheel state (disarmed/near-saturation)
   needs `Input<Wheels>` on the contract — deferred follow-up.
6. **Estimator-ready gate: q̂ successive-delta settle** (< 1e-3 rad for 0.2 s of polls);
   fixed dwell as fallback if noisy.
7. **Deterministic eclipse test knob: `PlantParams.init_orbit_phase`** (rad, rotates the
   initial in-plane pos/vel; default 0.0 leaves all byte streams untouched, zero new RNG
   draws).
8. **Phase timeout → `Outcome::Failed`** (run_state 3) after publishing `ModeCmd::safe`;
   `Aborted` stays the operator-cancel path.

## Verified against the code

- `mekf::State::estimate_attitude` is already const-generic over the observation count —
  eclipse mode is a one-element `estimate_attitude([mag], [mag_ref], [σ])` call. **No
  mekf.rs change needed.** About-B̂ attitude is instantaneously unobservable; gyro
  propagation + the orbit rotating B̂ bounds it over test-length shadows.
- **The default target never eclipses in-test**: sun at 2024-01-01 is ŝ ≈ (0.18, −0.90,
  −0.39); shadow entry is at orbit phase ≈33° ≈ 500 s in (period 5554 s), eclipse arc
  ≈35 min. closed_loop's 33 s stays sunlit and byte-identical at phase 0.
- Ctrl clamps the dipole to the `MTQ_MAX_DIPOLE` const, so tests cannot crank torquer
  authority above 0.2 A·m² — detumble decel is capped ~5.3e-4 rad/s²; detumble tests use
  the timeout path / near-exit thresholds, never a full detumble.
- Sequence yield primitive: `wait(Duration::ZERO)` spins within the same poll; the
  per-cycle poll is `wait(Duration::from_micros(1))` (Simulated `now` advances ≥8333 µs per
  cycle). Every await is a cooperative cancel point.
- The `#[sequence]` macro takes at most one params parameter, encoded from the `allow`
  node's properties — commissioning thresholds/timeouts are KDL-patchable per test, and
  every field must be spelled out (no dlopen serde defaults).

## Shadow geometry (contracts)

```rust
/// Cylindrical-umbra Earth shadow: illuminated unless the spacecraft is on the
/// anti-sun side AND inside the shadow cylinder of radius EARTH_RADIUS.
pub fn in_earth_shadow(pos_eci: &V3, sun_eci: &V3) -> bool {
    let s = sun_eci.normalize();
    let along: f64 = pos_eci.dot(&s).into_buf();
    if along >= 0.0 { return false; }
    let perp = *pos_eci - s * along;
    perp.norm().into_buf() < EARTH_RADIUS
}
```

## Frame / param changes (zerocopy layouts)

- `Sensors` 80→104 B: `sun_b: V3` is **replaced** by `css: [f64; 6]` (all-f64 layout stays
  padding-free: 8 + 24 gyro + 48 css + 24 mag). The FSW never sees a sun vector — only the
  readings. RNG draw order becomes bias(3), gyro(3), **css(6)**, mag(3), gps(6): the sun
  draws grow 3→6, so the mag/GPS byte streams shift — a documented stream break (same
  acceptance as the Tesla-mag change; parity unaffected, closed_loop thresholds retuned
  only if measurement demands). Heads reuse `meas_sigma` (unitless cosine reading, like the
  old sun vector).
- `World` 56→64 B: `illuminated: u8` + `_pad: [u8; 7]` (truth flag for the panel).
- New contracts const `CSS_THRESHOLD: f64 = 0.1` — shared by nav (validity gate) and the
  eclipse test's assertions. Head normals are the plant's model (±X/±Y/±Z), not contract.
- `ModeCmd`: `const DETUMBLE: u8 = 4` + `detumble()` ctor (layout unchanged).
- `PlantParams.init_orbit_phase: f64` (spelled in target.kdl).
- New `CommissioningParams`: rate_detumble_enter 1.0 / exit 0.8; est_delta_rad 0.001 /
  est_dwell_s 0.2; coarse_err_rad 0.2 / coarse_dwell_s 0.5; confirm_dwell_s 1.0; timeouts
  warmup 10 / detumble 900 / settle 60 / confirm 30 s. All on the `allow` line.
- Frame VTables change (Sensors/World): panel re-announces; stale recorded bundles won't
  decode the new frames — note in the commit.

## Plant: eclipse + CSS wiring

- Compute `let illuminated = !in_earth_shadow(&pos_eci, &sun_eci);` once per cycle.
- `disturbance_torques` gains an `illuminated: bool` parameter; `srp_b`/`srp_force_b`
  become zero in shadow (fix the "always lit" comment); its unit test passes `true` and
  gains a shadowed-SRP-is-zero assertion.
- Six CSS heads on the body faces (cube-sat parity, `cube-sat/main.rs:123-158`): per head
  `reading = max(0, n̂ᵢ·ŝ_b)·(illuminated as f64) + noise(meas_sigma)` — the FOV clamp and
  eclipse gate apply before the noise, so a dark head still reads its noise floor like real
  electronics. Six draws in the old sun-draw slot. Publish `Sensors { css }` and
  `World { illuminated }`.

## Nav

Nav owns the CSS processing (FSW math lives with its consumer, per the contracts→plant
reorganization):

```rust
/// Sun vector from the six face-mounted CSS readings by opposed-pair differencing;
/// `None` when no head sees the sun (eclipse — every reading is noise-floor).
pub fn sun_from_css(css: &[f64; 6]) -> Option<V3> {
    if css.iter().all(|r| *r < CSS_THRESHOLD) { return None; }
    Some(normalize([css[0]-css[3], css[1]-css[4], css[2]-css[5]]))
}
```

`Some(sun_b)` ⇒ today's two-observation MEKF call; `None` ⇒
`estimate_attitude([s.mag_b.normalize()], [mag_eci], [self.sigma])` (mag-only through the
outage — never model shadow in nav). Unit-tested in the nav crate: reconstruction accuracy
against generated readings, and the all-dark ⇒ `None` gate.

## Commissioning state machine

Signature: `commissioning(att: Input<AttitudeEstimate>, gps: Input<Gps>,
params: CommissioningParams, mode: Output<ModeCmd>)`. Each phase polls with the per-cycle
wait; abort ⇒ publish safe + `Aborted`; timeout ⇒ publish safe + progress + `Failed`.
`ModeCmd`/`progress` published on transitions only.

| Phase | Entry command | Gate to next | Timeout |
|---|---|---|---|
| 0 warm-up | (none) | q̂ delta < est_delta_rad for est_dwell_s | warmup_timeout_s |
| 1 detumble (only if \|ω̂\| > enter) | `detumble()` | \|ω̂\| < exit | detumble_timeout_s |
| 2 coarse point | `settling()` | HIL tracking error < coarse_err_rad for coarse_dwell_s | settle_timeout_s |
| 3 fine point | `pointing()` | error holds for confirm_dwell_s ⇒ Completed | confirm_timeout_s |

Boot runs go 0→2→3 (`ModeCmd` list stays `[SETTLING, POINTING]`). `safe_mode` gains
`_gps` in the same port position. Settling now lands ~1.5–2 s in (estimator settle) vs the
old 100 ms ⇒ closed_loop CYCLES ~4000→4400 (measure).

## target.kdl sketch

`init_orbit_phase 0.0` on plant; `input frame="gps"` on the slot +
`connect "plant" -> "mode" frame="gps"`; the `allow occupant="commissioning"` line carries
every CommissioningParams field.

## Tests

- Changed: sequences.rs (budget ~4400, new progress-line enumeration, abort scenario keeps
  `[SAFE]`), closed_loop.rs (CYCLES retune only), momentum/alarms/bundle unaffected (verify
  momentum's patch anchors + desat margin against the shifted settling).
- New contracts unit: shadow-function geometry cases. New plant unit: SRP zero in shadow;
  CSS head readings (lit face reads its cosine, back face reads noise floor, eclipse reads
  noise floor on all six). New nav unit: `sun_from_css` reconstruction accuracy against
  generated readings + all-dark ⇒ `None`.
- New `tests/eclipse.rs` (static path, momentum.rs pattern, slot started empty):
  shadowed run (anti-solar `init_orbit_phase` computed by scanning `in_earth_shadow`, not
  hardcoded; assert illuminated==0, every `css` reading < CSS_THRESHOLD, srp_b==0, estimate
  bounded on mag+gyro only) and transition run (enter shadow mid-run; assert `illuminated`
  and the max CSS reading drop in lockstep and the estimate error stays < ~0.2 rad across
  the transition).
- New detumble sequence tests: timeout→`Failed` with modes `[DETUMBLE, SAFE]` and zero
  wheel torque while detumbling (~500 cycles); optional exit-into-ladder run
  (`[DETUMBLE, SETTLING, POINTING]`, ~6000 cycles) — drop if the |ω̂| exit gate proves
  flaky when ω is near-B̂-parallel (the B-cross blind spot).
- House rules everywhere: per-cycle spawned samplers, `spec.process = false`, static link
  where speed matters.

## Risks

Closed-loop timing retune (bounded, empirical — now from BOTH the later settling command
and the CSS RNG-stream shift); the reconstructed sun vector's noise differs from the old
direct observation (per-head σ maps through the differencing to ~√2·σ per axis before
normalization — inside the MEKF's 0.02 weight, but verify convergence empirically);
brittle event-count assertions (enumerate once); slot-contract lockstep for future ports
(`SlotOccupantMismatch` is at least loud); every CommissioningParams field must be spelled
on the allow line; estimator-settle gate chatter under cranked noise (params make it
tunable); detumble |ω̂| exit stalls when ω is B̂-parallel (target-gated so it never runs;
tests assert the timeout path).

## Verify

`cargo test -p adcs-contracts`, `-p adcs-plant --no-default-features`, `-p adcs-fsw2`
(closed_loop parity, sequences ladder, eclipse, momentum, alarms, bundle), clippy clean.
