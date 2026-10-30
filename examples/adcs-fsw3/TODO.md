# TODO: from ADCS loop to a full ESPA-class spacecraft

This example flies one subsystem, ADCS, on a 2.8 kg cube-sat plant. The goal
is a realistic **~150 kg ESPA-class smallsat**:
- an Earth-observation mission with electric and chemical propulsion,
- a 28 V power system with deployable arrays and a thermal network,
- modes and fault protection,
- rendered star tracker and camera images for the autonomy work.

This document lists the missing subsystems, the environment they depend on, the
flight software that manages them, and the framework features that must land
first.

How values are marked:
- **[src]**: from a datasheet or a flight paper, listed under Sources.
- **[typ]**: a typical literature value; tune it.
- **[derived]**: computed for this spacecraft and orbit, with the formula
  inline. The drag and lifetime figures come from NRLMSIS 2.0 (`pymsis`),
  averaged over latitude, longitude, and local time, at three constant activity
  levels:
  - min: F10.7 = 70, Ap = 4
  - moderate: 150 / 10
  - max: 220 / 20

  Each lifetime is therefore a bound, not a solar-cycle average.

## Contents

0. [Scope and shape](#0-scope-and-shape)
1. [Framework prerequisites](#1-framework-prerequisites)
2. [Environment and orbit](#2-environment-and-orbit)
3. [ADCS rescale](#3-adcs-rescale)
4. [Power (EPS)](#4-power-eps)
5. [Thermal](#5-thermal)
6. [Propulsion](#6-propulsion)
7. [Modes, FDIR, and LEOP](#7-modes-fdir-and-leop)
8. [Command and data handling](#8-command-and-data-handling)
9. [Fault injection](#9-fault-injection)
10. [Scenarios and tests](#10-scenarios-and-tests)
11. [Rendered images: star trackers and the imager](#11-rendered-images-star-trackers-and-the-imager)
12. [Payload and image autonomy](#12-payload-and-image-autonomy)
13. [RF and the ground segment](#13-rf-and-the-ground-segment)
14. [Panel](#14-panel)
15. [Suggested order](#15-suggested-order)
16. [Open decisions](#16-open-decisions)

---

## 0. Scope and shape

### 0.1 The plant is its own pack

**The simulation and the flight software never share a pack.** Today one
`adcs` pack registers `plant` beside `nav`, `ctrl`, and `mode`. That ends with
the first restructure.

- **Sim packs** hold everything that stands in for the universe and the
  hardware: dynamics, environment, sensor and actuator models, fault
  injection, and image rendering. They run only on the `plant` target.
- **Flight packs** hold everything that would fly. They run only on the `fsw`
  target.
  - They never depend on a sim pack or on a sim-only contract.
  - They must build for a target where no sim crate exists.
  - A CI check enforces this: build the flight packs with the sim crates
    absent from the dependency graph.
- **Contracts split the same way.**
  - `contracts/hw` holds what crosses the hardware boundary: sensor frames,
    actuator commands, and unit housekeeping. This is the interface-control
    document, and both sides depend on it.
  - `contracts/truth` holds ground-truth frames: body state, environment, and
    true disturbances. Only sim packs and tests read it.
  - Shared math, such as frames conversions, WMM, and the sun vector, goes in
    a third crate both sides may use. On orbit, flight code evaluates the same
    models.
- **Tests** may link both sides. The convergence test already does.

The existing `sim.py` single-process loop stays. It adds sim-pack and
flight-pack systems to one target, which the rule allows: it limits packs, not
targets.

### 0.2 The spacecraft ("ESPA-150") [derived unless marked]

The representative bus draws on:
- Apex Aries, Airbus Arrow 150, Terran Nebula, and York S-CLASS (the bus),
- Planet SkySat-C (EO reference).

| Item | Value |
|---|---|
| Envelope | 0.60 (x) × 0.70 (y) × 0.90 (z) m, inside the ESPA 0.61 × 0.71 × 0.97 m port box [src Moog] |
| Mass | 150 kg wet: body ~138 kg (bus ~85, payload ~35, propellant 5–15) + 2 × 6 kg wings |
| Axes | +z = separation axis = imager boresight (nadir); +x = ram; y = array/SADA axis (≈ orbit normal) |
| Inertia, deployed | ≈ diag(25.7, 10.2, 21.3) kg·m², products 1–2 % of diagonal |
| Inertia, stowed | ≈ diag(13.6, 10.2, 9.1) kg·m² |
| CoM | ~0.40 m above the separation plane (ESPA limit 0.508 m), lateral offset 5–15 mm, shifting a few mm as propellant depletes |
| Arrays | 2 × ~1 m² wings on a single-axis SADA about y; 603 W BOL / 543 W EOL |
| Battery | 8s Li-ion, 28 V unregulated bus (24–33.6 V), ~860 Wh |
| Propulsion | Hall thruster on −x (thrust along ram) + 4 canted 1 N green monoprop thrusters |
| ADCS | 4 × 1 N·m·s wheels in a pyramid, 3 × 20 A·m² MTQs, FOG IMU, 2–3 star trackers, CSS, magnetometer, 2 GPS |
| Payload | 20 cm aperture, f = 1.5 m, 5 µm pixel pushbroom imager: 1.83 m GSD at 550 km |
| Storage | 1–4 TB |

How these were computed:
- **Body inertia.** A box with a dense core: `I = k·m/12·(b² + c²)` with
  k = 0.7, giving diag(10.5, 9.4, 6.8).
- **Wing inertia.** Each wing is a 0.9 × 1.1 m plate centred 1.05 m out along
  ±y. The two add diag(15.3, 0.8, 14.4).
- **Thruster placement.** The Hall thruster sits on −x so thrust points along
  +x (ram). Drag make-up then runs in the nominal nadir attitude, with no slew
  per burn.

### 0.3 The orbit

Rideshare drops at SSO 500 km with LTAN 10:30, then raises to **550 km**
(Hohmann, 27.5 m/s) [derived].
- At 550 km with arrays, the 25-year rule is likely met passively. The FCC
  5-year rule is not, so the example carries a disposal burn (§6).
- At LTAN 10:30, |β| runs 15–25° through the year, so a single-axis SADA
  about orbit normal loses cos β ≈ 3–10 %.

The SSO inclination comes from
`cos i = −2a^{7/2}·ω̇☉ / (3·J2·Re²·√μ)`, with the node rate checked at
0.9856°/day.

| h (km) | Period (min) | v (km/s) | Ground speed (km/s) | i_SSO (°) | Max eclipse, β = 0 | β* (no eclipse above) |
|---|---|---|---|---|---|---|
| 400 | 92.56 | 7.669 | 7.22 | 97.03 | 36.1 min | 70.2° |
| 500 | 94.62 | 7.613 | 7.06 | 97.40 | 35.8 min | 68.0° |
| **550** | **95.65** | **7.585** | **6.98** | **97.59** | **35.6 min** | **67.0°** |
| 600 | 96.69 | 7.558 | 6.91 | 97.79 | 35.5 min | 66.1° |

### 0.4 Pack layout

```
contracts/hw          sensor frames, actuator commands, unit HK (the ICD)
contracts/truth       ground-truth frames (sim + tests only)
contracts/models      shared math: frames, WMM, sun, orbit helpers

sim/env               orbit (J2, drag), sun, moon, atmosphere, eclipse, ground sites
sim/dynamics          rigid body + wheels + SADA + propellant mass
sim/sensors           gyro, CSS, mag, GPS, star tracker (measurement then image level)
sim/eps               arrays, battery, PCDU, loads
sim/thermal           node network
sim/prop              Hall thruster, monoprop thrusters, tanks
sim/obc               clock, watchdog, SEU/SEL, NVM, storage device
sim/render            star field and Earth scene renderers (§11)
sim/faults            fault injection
sim/rf                link and ground stations (§13, last)

fsw/adcs              nav, ctrl, pointing laws, momentum management
fsw/power             SoC, battery modes, load shed
fsw/thermal           heaters, limits
fsw/prop              burn planning, inhibits, startup state machines
fsw/cdh               time, HK, events, storage, scheduler, cmd dispatch, health
fsw/modes             mode manager, LEOP and safe-mode sequences
fsw/payload           imaging planner, onboard image processing
fsw/radio             beacon, queues, pass handling (§13, last)
```

Whether each directory is a separate pack crate or one crate per side holds
several modules is §16. The rule in §0.1 holds either way.

### 0.5 Target shape when complete

```
plant target  (120 Hz)   sim/* packs
fsw target    (120 Hz)   fsw/* packs
ground target            ground stations, operator commands (§13)
gateway                  db for the panel
```

---

## 1. Framework prerequisites

These are fsw-3 gaps the example hits. The workaround column says what the
example can do until a slice lands.

### F1. Multi-rate by skipped cycles

Every system runs every cycle. The slow work is:

| System | Rate |
|---|---|
| Thermal | 1 Hz |
| Housekeeping | 1 Hz |
| EPS | 10 Hz |
| Star tracker | 5–10 Hz |
| GPS | 1 Hz |
| Renderers | 1–10 Hz |

The decision is to implement this as skipped cycles. The skip should still be
declared in config, not written into each system, for one mechanical reason:

- **fsw-3 rings are lossless and apply backpressure.** A reader that stops
  reading pins ring space. Once the ring fills, the producer's `try_write`
  fails with `WouldBlock` (`ring/src/lib.rs:734`).
- A 1 Hz system that returns early for 119 cycles therefore stalls the 120 Hz
  producers it reads, and every other reader of those producers with them.
- Every hand-written skip must remember to call `latest()` or `drain()` each
  cycle, and the failure appears far from the cause.

Proposal:
- [ ] `fsw.add(.., every=N, phase=k)` wraps the step in a small **rate
      adapter**, the way `channel=` does.
- [ ] On a skipped cycle the adapter advances the system's frame cursors to
      the newest record and does not run the body.
- [ ] Message inputs are drained when the body runs. Build checks that
      `ring_depth` holds N cycles of each message producer.
- [ ] Phases spread slow systems over different cycles, so every 1 Hz system
      does not land on cycle 0.
- [ ] `status` reports the rate; the exec timeline draws it.
- [ ] The system itself integrates over the real elapsed time from `now`.

Workaround until then: self-decimation from `now`, with a `latest()` call on
every frame input each cycle.

### F2. Alarms / limit checking

The fsw-2 `AlarmSystem` was not ported. The wkt `AlarmDef*` messages exist,
and the panel alarm pane and meter limits read them. FDIR needs yellow and red
limits everywhere.
- Workaround: a flight `limits` system in the example that publishes
  `AlarmDefs`/`AlarmRaised`.
- It moves into fsw-3 as a built-in afterwards.

### F3. Enums

There are two separate gaps.

**Unit enums in frames (modes, states).**
- The metor-component derives already support `#[repr(..)]` unit enums.
  `Metadatatize` emits `enum_variants`, which the panel shows as names
  (`libs/metor-component/derive-impl/src/metadatatize.rs:15`).
- But fsw-3's `Frame` requires zerocopy `FromBytes` (`src/frame.rs:16`).
  A unit enum with fewer than 256 variants cannot satisfy it, because not
  every byte is a valid variant.
- I have not compiled this; a compile test should confirm it.
- Fix: move `Frame`'s read path to `TryFromBytes`, or add a
  `#[frame(enum = Mode)]` attribute on a `u8` field that emits the variant
  metadata.
- Workaround: a `u8` field read through `TryFrom<u8>`, as `ModeCmd` does now.

**Message enums with data (telecommands).**
- `#[derive(Record)]` is struct-only (fsw-3 TODO, slice 2).
- Workaround: structs with an opcode field.

### F4. Telecommand routing

Commands are a `Subscribe`, and `command_ids` moves them through the gateway
db. There is no dispatcher, no acknowledgement, and no rejection telemetry.
- Workaround: a flight `cmd` system that fans one `Telecommand` out to
  per-subsystem outputs and writes `CommandAck`.

### F5. Postcard `Sample<T>`

`Sample` holds only `Frame + Copy` (slice 5 TODO). Redlines in LEOP, burn, and
imaging sequences want to read message-typed state.
- Workaround: keep redline inputs as frames.

### F6. Presets from fsw-3

There is no `PresetDefs` broadcast.
- Workaround: ship panel layouts as files.

### F7. Runtime parameters

Params are JSON decoded once at build. There are no parameter tables, no
`UpdateComponent` handler, and no golden copy. FDIR thresholds, heater
setpoints, load-shed tables, and burn plans must be ground-commandable.
- Workaround: tunables live in a frame owned by the system and are changed by
  telecommand.

### F8. Persistent state across a reset

Nothing survives a `Reset` or a restart. These must persist:
- boot count and reset cause,
- deploy-done flags and the 7-day propulsion inhibit clock,
- ground-watchdog hours,
- the mode.

Workaround: a sim `nvm` system (F-RAM is hardware) serves the persistent block
back after a reboot.

### F9. Whole-target reboot

`Reset` restarts one system. An OBC reset restarts the fsw target, and the
PCDU may power-cycle it.
- Workaround: channel `Reset` of every flight system, driven by a `reboot`
  message from `sim/obc`.

### F10. User-injected clocks

Today `Clock` is a closed enum (`src/coordinator/config.rs:213`):
- `Wall { rate }` stamps `Timestamp::now()` and sleeps out the cycle budget.
- `Simulated { dt }` stamps `epoch + cycle·dt` and never sleeps.

`Coordinator::now` and `run` match on it (`src/coordinator/run.rs:111`,
`:127`). Nothing else can drive a target's time.

**The feature: users supply the clock.** The design question is whether a
clock is two parts or one:
- the **time source**: what `now` is this cycle;
- the **pacer**: when the next cycle starts.

Built-in clocks cover the common cases. A user clock implements a trait and
registers from a pack like a system does. It is selected in Python with
`Target(clock=...)`.

| Clock | Time source | Pacer | Use |
|---|---|---|---|
| Wall | host time | real-time budget | hardware in the loop, flight |
| Simulated | `epoch + cycle·dt` | none (unpaced) | tests, batch runs |
| **Scaled** | `epoch + cycle·dt` | `dt / factor` of wall time | watching an orbit at 10× or 100× |
| **Onboard (user)** | an OBC model: drift, aging, temperature, jumps on a GPS or ground sync, reset to 0 on power loss | whatever it wraps | the fsw target runs on the clock it would fly with |
| **Driven (user)** | a time record from a ring, e.g. the plant's `sim_time` over a `Subscribe` | steps when the driver's tick arrives | lockstep across targets (NOS3's time driver) |

What this settles or opens up:
- **Speed-up.** Scaled covers it, and a runtime clock-control message
  (pause, single-step, set factor) lets an operator slow down around an event.
  This replaces the old "time acceleration" item.
- **Onboard time versus coordinator time** (formerly §16). The fsw target's
  `Timestamp` *is* onboard time: an injected clock wrapping the sim's OBC
  model. Flight code needs no separate time frame. Plots against truth use
  the plant's timestamps and the recorded offset.
- **Repeatable deployments.** Today each target in `target.py` free-runs at
  120 Hz on wall time, so multi-process runs are not repeatable. A driven
  clock makes the plant the time master and the fsw target step in lockstep.
  A deployment then becomes as deterministic as `sim.py`, and it can run
  faster than real time.
- **F14.** A driven or simulated clock is the natural place for "do not
  advance until the render is done".

Design points for the slice:
- [ ] The trait's shape.
  - It has to cover a pacer that awaits (wall, scaled, driven) and one that
    returns at once (simulated).
  - Guidance from existing feedback: return values and guards, and no
    callbacks threaded into futures.
- [ ] Where a user clock lives.
  - A pack entry beside systems, with its own ABI entry.
  - Or a system flagged as the clock, which reads its inputs and returns the
    next `now`. This reuses rings, params, and channels, and it is the
    smaller change.
- [ ] Monotonicity.
  - Onboard clocks can step backward on a sync or a reset.
  - Rings and `Input::latest` order by timestamp.
  - Either the coordinator keeps a monotonic cycle time and exposes onboard
    time as a second value, or consumers must tolerate a backward step.
  - Decide this before the OBC clock lands.
- [ ] Cycle-budget accounting, `status` offsets, and `wait_for`/`sleep` in
      sequences all need to follow the injected clock, not the host clock.
- [ ] CLI: `--sim-dt` and `--rate` become shorthands for the built-ins. Add
      `--speed N` for Scaled.

Workaround until then: `--sim-dt` for batch runs, and wall-clock for
watching.

### F11. WMM allocates every call

See the README's "Known allocation" section.
- Workaround: patch `libs/wmm` to reuse workspaces, and evaluate the field at
  1 Hz.

### F12. Exec timeline on fsw-3

The panel's execution timeline read fsw-2 IR and `system_status` paths, and was
deleted with fsw-2. It matters once the schedule holds 30+ systems.
- Workaround: none. Rebuild the view on `{ns}.{id}.status.exec_time_ns` (git
  history has the fsw-2 version under `libs/metor-panel/src/views/exec_timeline`).

### F13. Large records (images)

Frame sizes:

| Frame | Size |
|---|---|
| Star tracker, 1024² × 12-bit | 1.6 MB |
| Star tracker, 2592 × 1944 | 7.6 MB |
| Framing EO camera | 7.5 MB |
| Pushbroom, 12k px × 1 band | ~550 Mbit/s |

Framework questions these raise:
- ring sizing per port,
- zero-copy handoff for records of this size,
- whether the thread adapter's per-cycle mirror copy is acceptable,
- how images reach the db and the panel.

Workaround: centroid-list and product-metadata frames first; raw frames on a
dedicated large ring.

### F14. Heavy compute off the loop, deterministically

A render takes 30–75 ms per EO frame against an 8.3 ms cycle. It must run as
an async system on its own thread.
- Under a simulated clock the loop must not advance past the exposure time
  until the image is ready. Otherwise runs stop being repeatable.
- This needs a "request, then wait for completion" semantic under sim time,
  while wall-clock runs keep today's free-running behavior.
- Workaround: render synchronously at a low rate under sim time.

### Related fsw-3 TODO items

- Shared state (two systems on one socket, §13).
- The `loop()`-style forward link reference (the ground topology in §13).
- Per-port message identity.

---

## 2. Environment and orbit

### 2.1 What exists
- A circular point-mass orbit (RK4).
- Drag force at a constant `rho`.
- A low-precision sun vector.
- A cylindrical umbra.
- WMM.
- ECI↔ECEF↔NED and geodetic conversions.

There is no SGP4, J2, atmosphere model, moon, topocentric helper, or pass
prediction anywhere in the repo.

### 2.2 Features

- [ ] **`sim/env` system.** It writes one truth frame per cycle, and every sim
      system reads it. The frame holds:
  - position and velocity, epoch,
  - sun vector and distance, shadow fraction, β,
  - density, magnetic field,
  - moon vector.
- [ ] **J2 gravity.** J2 = 1.08263e-3. Without it the SSO node does not
      precess, and the local time drifts about 4 min/day. Tiers: point mass,
      then J2, then J2–J4.
- [ ] **Density model.**
  - Start with an exponential table: 400 km ρ0 = 3.725e-12, H = 58.5 km;
    450 km 1.585e-12 / 60.8 km [typ, Vallado]. Extend it to 500–600 km.
  - Then F10.7/Ap scaling.
  - Then NRLMSIS 2.0, if a Rust port is acceptable; it produced the numbers
    below.
  - Drag force and drag torque read the same density.

| h | ρ min / mod / max (kg/m³) | Δv/yr, B = 57 | Δv/yr, B = 136 | Decay/yr, B = 57 |
|---|---|---|---|---|
| 500 | 9.1e-14 / 9.1e-13 / 2.7e-12 | 1.5 / 14.7 / 43.2 m/s | 0.6 / 6.1 / 18.0 | 2.7 / 27 / 78 km |
| **550** | 3.8e-14 / 4.4e-13 / 1.4e-12 | **0.6 / 7.0 / 22.8** | 0.25 / 2.9 / 9.5 | 1.1 / 13 / 42 km |
| 600 | 1.8e-14 / 2.2e-13 / 7.8e-13 | 0.3 / 3.5 / 12.3 | 0.1 / 1.4 / 5.1 | 0.5 / 6.4 / 23 km |

  The columns use:
  - ballistic coefficient `B = m/(Cd·A)` with Cd = 2.2: 57 kg/m² with arrays
    face-on (1.2 m²), 136 kg/m² edge-on (0.5 m²) [derived];
  - `Δv/yr = ½ρv²/B · t` and `da/dt = −(ρ/B)√(μa)`.

- [ ] **Faceted drag and SRP.** Both come from one geometry table: body faces,
      array wings at the SADA angle, and a CoM that moves with propellant. The
      same table drives arrays, thermal, and antenna placement.
- [ ] **Conical umbra and penumbra** with a shadow fraction, replacing the
      boolean `illuminated`. Power ramps rather than steps. The ~36-minute
      maximum eclipse is at β = 0.
- [ ] **β angle, sun distance** (1/r²: 1322–1414 W/m²), and a **low-precision
      moon** for star tracker exclusion.
- [ ] **Earth albedo and IR.** Albedo coefficient 0.3, Earth IR 237 W/m²
      [typ]. Used by thermal, CSS bias, and the renderers.
- [ ] **Ground sites.** Station and imaging-target LLA → ECEF → SEZ: range,
      range rate, azimuth, elevation, sun elevation at the site. Imaging uses
      it (§12) well before RF does (§13).
- [ ] **SAA region** for the SEU rate and star tracker hot pixels: latitude
      −50° to 0°, longitude −90° to +40° [typ].
- [ ] **Flight-side orbit knowledge.**
  - An onboard J2 propagator seeded by GPS (2 receivers), with ephemeris age
    as telemetry and an FDIR input.
  - SGP4 from an uplinked TLE as the fallback: ~1 km at epoch, growing
    1–3 km/day [src].
  - Burns (§6) must update the onboard state.

---

## 3. ADCS rescale

Most of the ADCS code carries over. The constants and several models change
with the bus.

- [ ] **Mass properties.**
  - Inertia from §0.2, with products of inertia.
  - The stowed tensor until arrays deploy.
  - CoM and inertia updated as propellant depletes.
- [ ] **Four reaction wheels in a pyramid.** Rocket Lab RW-1.0: 1.0 N·m·s,
      0.1 N·m, 5 W steady, 99 W peak [src].
  - Sizing with Ixx = 25.7 (bang-bang, `α = 4θ/t²`) [derived]:
    - 30° in 60 s needs 0.015 N·m, a 1°/s peak rate, and 0.45 N·m·s.
    - 30° in 30 s needs 0.06 N·m, 2°/s, and 0.9 N·m·s.
    - Four RW-1.0 give ~1.1–1.6 N·m·s per axis, enough for the Aries-like
      1.25°/s agility [src].
  - The allocation is a pseudo-inverse with null-space momentum management,
    and a wheel-failure reconfiguration to 3 wheels.
  - Wheel power goes into the PCDU (torque × speed plus electronics).
  - Static imbalance produces microvibration: 26.6 g·mm on RW4-class wheels
    [src]. Only the imager jitter budget needs it (§12).
- [ ] **Magnetorquers.** 3 × 20 A·m² rods, 1–2 W each [typ].
  - `τ = m·B ≈ 6e-4 N·m` peak and ~3e-4 average, which dumps 0.5 N·m·s in
    ~30 min [derived].
  - MTQ duty-cycling against magnetometer sampling (a real FSW pattern), or
    the MTQ field added to the magnetometer reading.
- [ ] **Gyro.** FOG class, LN-200S: ARW < 0.07°/√h, bias stability < 0.1°/h,
      12 W [src]. It replaces today's MEMS-style noise.
- [ ] **Solar array drive.** Moog small-sat SADA [src]:
  - stepper with 0.018° steps, 1.0°/s maximum, ±184°;
  - potentiometer telemetry, 3.7 W.
  - Array angle feeds power, drag, SRP, and inertia.
  - The FSW tracks the sun angle projected into the array plane.
- [ ] **Tip-off and detumble.**
  - Rideshare microsats target ≤ 5°/s tip-off [src SpaceX RPUG]. Sample
    uniformly 0.5–5°/s per axis.
  - Momentum is 0.45 / 0.9 / 2.2 N·m·s at 1 / 2 / 5°/s.
  - B-dot detumble on the MTQs takes ~25 / 50 / 125 min [derived].
  - Wheel-assisted detumble is allowed once the rate is under the wheel
    capacity.
- [ ] **Updated disturbances at 550 km** [derived]:

| Source | Magnitude |
|---|---|
| Gravity gradient | `1.5n²ΔI·sin2θ`: 2.9e-5 N·m max, 2.5e-5 at 30° off-nadir |
| Residual dipole | 3e-6 to 3e-5 N·m for 0.1–1 A·m² |
| SRP | 3e-7 to 1.6e-6 N·m for a 2–10 cm CoP offset |
| Drag (moderate) | ~3.5e-6 N·m |

  Gravity gradient and residual dipole dominate. All of these are far below
  the thruster torques in §6.
- [ ] **Pointing laws.**
  - Sun-safe: CSS + gyro, no ephemeris, arrays to the sun.
  - Nadir with yaw steering (to recover the cos β loss).
  - Inertial hold.
  - Target tracking (§12).
  - Burn attitude (§6).
  - Ground-station tracking (§13).
  - Minimum-drag and maximum-drag attitudes.
- [ ] **Requirements to score against**, for the 1.83 m GSD imager at 550 km
      [derived]:
  - control 0.05° (480 m on the ground),
  - knowledge 10″ (27 m geolocation),
  - jitter 0.1–0.3 IFOV, which is 0.1–0.2″ with IFOV = 3.3 µrad.

  Without a star tracker, fine pointing is out of reach, because CSS and the
  magnetometer give about 1°. That is accepted until §11 lands. A
  quaternion-level star tracker stub is optional and cheap if earlier scoring
  is wanted:
  - 5″ cross-boresight and 55″ about boresight, RMS [src ST-16RT2];
  - exclusion angles: sun 34°, Earth 22°.
- [ ] **Commissioning.** Port fsw-2's ladder: estimator warm-up, detumble,
      coarse, then fine with dwell gates. `mode` becomes a channel or slot
      again. fsw-3 `wait_for` has no dwell, so the dwell goes in the
      predicate or a helper. Magnetometer calibration uses `metor-adcs`
      `magkal`.
- [ ] **ADCS health checks.**
  - innovation residuals,
  - wheel speed limits,
  - desaturation failure,
  - sensors on switched PCDU channels. Nav and ctrl must handle a sensor that
    is off; today every sensor is always on.

---

## 4. Power (EPS)

### 4.1 Sim model (`sim/eps`)

**Arrays**
- [ ] Cell: Azur 3G30C-Advanced [src].

| Parameter | Value |
|---|---|
| Efficiency | 29.5 % BOL; 27.8 % at 5e14 e⁻/cm²; 26.5 % at 1e15 |
| Pmp coefficient | −0.23 %/°C |
| α | 0.91 |

- [ ] Per-string power:
      `P = S·f_shadow·η(T)·A·packing·max(0, n̂·ŝ)·(cosine knee)`. The array
      normal comes from the SADA angle and attitude; the temperature comes
      from thermal.
- [ ] Sizing: `P_BOL = 1361·A·η·0.85 packing·0.93 temperature·0.95 wiring`
      [derived].
- [ ] Orbit-average power uses the SMAD form
      `P_EOL·T_d / (T_e/0.65 + T_d/0.85)` [derived].

| Array | BOL | EOL | Orbit-average | Eclipse energy |
|---|---|---|---|---|
| 1 m² | 302 W | 271 W | 128 W | 77 Wh |
| **2 m²** | **603 W** | **543 W** | **257 W** | **153 Wh** |
| 3 m² | 905 W | 814 W | 385 W | 230 Wh |

  These match the reference buses: Aries 175–500 W payload OAP, Arrow 250 W
  average, S-CLASS 200 W [src].
- [ ] Failure masks per string, a radiation degradation param, and a wing
      that fails to deploy.
- [ ] Tier 2: a single-diode I–V curve per string solved at the operating
      point, which captures MPPT versus S3R/DET behavior.

**Battery**
- [ ] 8s Li-ion, 24.0–33.6 V. EnerSys ABSL 8s20p, 30 Ah, ~860 Wh [src]. At
      nominal eclipse load that is ~18 % depth of discharge, against a
      ≤ 20–30 % limit for ~27k LEO cycles over 5 years [typ].
- [ ] Thevenin 1RC per cell, scaled by series and parallel counts:
  - `V = OCV(SoC,T) − I·R0 − V1`
  - `dV1/dt = −V1/(R1C1) + I/C1`
  - `dSoC/dt = −ηI/(3600·Q(T))`
- [ ] Per-cell parameters at 25 °C [typ]:
  - R0 30–60 mΩ
  - R1 15–30 mΩ
  - τ1 30–60 s
  - R rises 2–3× at 0 °C, and capacity falls 10–20 % at 0 °C.
- [ ] Per-cell OCV table [typ NMC]; multiply by 8 for the pack.

| SoC | 0 % | 10 % | 20 % | 30 % | 40 % | 50 % | 60 % | 70 % | 80 % | 90 % | 100 % |
|---|---|---|---|---|---|---|---|---|---|---|---|
| OCV (V) | 3.00 | 3.45 | 3.55 | 3.62 | 3.68 | 3.74 | 3.82 | 3.91 | 4.00 | 4.08 | 4.18 |

- [ ] I²R heat into the battery thermal node, a capacity-fade param, and a
      charge inhibit below 0 °C.

**PCDU** — AAC Clyde STARBUCK-MINI class [src]
- [ ] 22–34 V primary bus, MPPT or S3R regulation, 4 × 120 W array strings.
- [ ] Outputs:
  - 4 high-power LCLs (≤ 10 A) for the Hall PPU, wheels, and heaters,
  - 12 nominal LCL/RLCLs (≤ 3 A),
  - 12 secondary rails (5/8/12/15 V).
- [ ] Each LCL has a trip current, a latch-or-retrigger setting, and a trip
      counter.
- [ ] Every simulated unit publishes its draw, and the PCDU sums per channel.
      Undervoltage resets the OBC and returns outputs to defaults (F9).
- [ ] Load table (mode budgets [typ]):

| Load | Power |
|---|---|
| OBC | ~10 W |
| ADCS | 20–30 W (wheels peak 99 W each on slews) |
| FOG | 12 W |
| Star trackers | ~1 W each |
| RX | 5 W |
| PCDU idle | 10 W |
| GPS | 2 W |
| Heaters | 20–40 W |
| SADA | 3.7 W |
| Imaging | +60–120 W for 10–15 min/orbit |
| X-band pass | +40–60 W for 8–10 min |
| Hall firing | +350–700 W, sunlit only |
| Catalyst preheat | ~10 W per thruster for 20–40 min |

  Resulting budgets: safe 70–90 W, nominal nadir 110–130 W. The sunlit margin
  on a 2 m² array is ~380–420 W, enough to run a 300–350 W Hall thruster
  directly [derived].
- [ ] Telemetry, the flight side's only view:
  - bus voltage, battery current, array string currents,
  - per-channel current and state, trip counters,
  - temperatures.
  - Sensor models include ADC noise and a current-sensor bias (a coulomb-count
    drift scenario).

### 4.2 Flight software (`fsw/power`)

- [ ] SoC estimator: coulomb counting re-anchored on OCV at rest, or an EKF
      over the 1RC model.
- [ ] Battery-mode ladder: FULL / NORMAL / SAFE / CRITICAL, with thresholds
      scaled to 8s. Recovery thresholds sit above each entry threshold, and
      all of them are commandable (F7).
- [ ] Table-driven load shedding: each output has a priority and the lowest
      battery mode that allows it.
- [ ] LCL trip handling: retry N times, then leave the channel off and raise
      an alarm.
- [ ] An energy gate for activities: imaging, Hall firing, and X-band passes
      each check the predicted SoC after the activity against a floor, using
      the onboard eclipse prediction.
- [ ] SADA sun tracking and yaw-steering requests to ADCS.
- [ ] Commands:
  - set output, power-cycle,
  - set the MPPT mode,
  - set heater,
  - reset trip counters,
  - set thresholds.

---

## 5. Thermal

### 5.1 Sim model (`sim/thermal`, 1 Hz via F1)

- [ ] **15–25 nodes:**
  - 6 body panels and a radiator,
  - 2 array wings,
  - battery, OBC/PCDU, and the wheel cluster,
  - FOG and star tracker heads,
  - payload optics and focal plane,
  - propellant tank and catalyst beds,
  - Hall thruster and PPU,
  - X-band transmitter.
- [ ] Per node:
      `mᵢcᵢ dTᵢ/dt = Q_sun + Q_alb + Q_IR + Q_int − εσAT⁴ + Σ G_ij(Tⱼ − Tᵢ) + Σ radiation`.
- [ ] External flux per face from the `env` frame and the geometry table.

| Surface | α | ε |
|---|---|---|
| Solar cells | 0.91 | 0.85 |
| Black anodize | 0.65–0.88 | 0.82–0.88 |
| Clear anodize | 0.27–0.35 | 0.76–0.84 |
| White paint (radiator) | 0.17 | 0.92 |
| Kapton / MLI outer | 0.38 | 0.67 |

- [ ] Internal heat: every load's power lands in its node. RF output power is
      subtracted for transmitters, and thrust power for the Hall thruster.
- [ ] Heaters on PCDU channels. Thermistors with noise, quantization, and
      open or stuck faults.
- [ ] Coupled temperature effects: array efficiency, battery R0 and capacity,
      FOG bias, star tracker dark current (§11), catalyst bed readiness (§6),
      and optics focus.

### 5.2 Flight software (`fsw/thermal`)

- [ ] Heater thermostats with hysteresis and commandable setpoints, for the
      battery, the propellant lines and tank (keep hydrazine-class
      propellants from freezing), and the optics.
- [ ] Limit tables (F2). Battery: charge 0–45 °C, discharge −20–60 °C.
      Electronics: −40–85 °C.
- [ ] Actions: a charge-inhibit request, payload off on overtemperature, and a
      Hall duty limit on PPU temperature.

---

## 6. Propulsion

The larger bus is what makes propulsion worth modeling. The baseline carries
both families, so the example exercises long low-thrust arcs and short impulse
bits:
- **Hall thruster** for orbit raise, drag make-up, and disposal;
- **4 × 1 N green monoprop** for collision avoidance, fast attitude authority
  during burns, and backup.

### 6.1 Δv budget, 5 years at 550 km [derived]

| Item | Δv (m/s) |
|---|---|
| Raise 500 → 550 km | 27.5 |
| Drag make-up | 7–23 per year, so 35–80 |
| Collision avoidance | ~1–2 per event × 2–5 per year, so 25–50 |
| Disposal to 350 km perigee | 55.7, giving a 5.2 / 1.1 / 0.5 yr lifetime at min / mod / max |
| **Total with margin** | **~150–200** |

Propellant, from `m·(1 − e^(−Δv/(Isp·g₀)))`:
- ~2.3 kg Xe on a ~1300 s Hall thruster for 200 m/s,
- ~6.5 kg of HPGP for 100 m/s.

### 6.2 Sim models (`sim/prop`)

**Hall thruster** — Busek BHT-350 (350 W, 17 mN, 1244 s) or Orbion Aurora
(100–300 W, ~60 mN/kW, 1400 s, dipole < 0.15 A·m²) [src]
- [ ] **T0:** constant thrust; `ṁ = F/(Isp·g0)`; PPU draw.
- [ ] **T1:**
  - throttle tables F(P) and Isp(P), PPU efficiency 0.90–0.93 [typ],
  - an undervoltage trip,
  - a startup state machine [src/typ]:
    1. cathode heater, 3–6 min,
    2. keeper and discharge ignition,
    3. a thermal settling ramp over minutes.
  - First-use cathode conditioning takes hours.
- [ ] **T2 disturbances:**
  - thrust misalignment of 0.5–1°, plus 0.1–0.3° thermal drift [typ];
  - the resulting torque `F·(L·sinθ + d)`: 1.4e-4 N·m, or 0.52 N·m·s per
    hour, for 0.5° and 5 mm [derived];
  - swirl torque of ~0.05–0.1 mN·m, scaled from 0.86 mN·m on the SPT-140 at
    4.5 kW [src, derived];
  - the thruster dipole coupling into the MTQs and magnetometer.

  The wheels absorb this torque while the MTQs dump it concurrently.
- [ ] **T3:** xenon regulator/plenum bang-bang control and PVT gauging.
- [ ] Burn times at 17 mN on 150 kg (a = 1.13e-4 m/s²):
  - 1 m/s takes ~2.5 h,
  - the raise takes 67 h [derived].

  Collision avoidance on EP therefore needs about a day of warning, which is
  why the chemical thrusters are there.

**Monoprop thrusters** — ECAPS 1N HPGP [src]
- 0.25–1 N at 204–231 s.
- Minimum impulse bit ≤ 30 mN·s.
- 5.5–22 bar blowdown feed.
- Aerojet GR-1 is similar: 235 s, MIB 8 mN·s, 12 W valve.

- [ ] Blowdown: `P = P₀(V_g0/V_g)^γ` and F ∝ P. Thrust falls from 1 N at
      22 bar to 0.25 N at 5.5 bar.
- [ ] Pulse mode:
  - MIB, 5–20 ms valve delays, 50–100 ms rise,
  - ±5–20 % impulse-bit scatter [typ],
  - a cold-bed penalty.
- [ ] **Catalyst bed thermal node.** Firing is permitted above 285 °C (GR-1) or
      ~350 °C (HPGP), after 20–40 min at ~10 W of preheat.
- [ ] Torque from one 1 N thruster with 0.5° and 5 mm is 8.5e-3 N·m. That
      fills 1 N·m·s wheels in about 2 minutes [derived], so attitude during a
      chemical burn uses off-pulsing of the 4 canted thrusters (SkySat style).
- [ ] Tank: propellant mass → CoM and inertia (§3), and pressure telemetry.
- [ ] Faults:
  - valve stuck open (the torque swamps the wheels),
  - leak (pressure decay),
  - clogged thruster,
  - Hall flameout, cathode failure to ignite,
  - PPU trip.

### 6.3 Flight software (`fsw/prop`)

- [ ] **Inhibits.**
  - At least three independent inhibits on propulsion.
  - Rideshare rules: no propulsion for **7 days** after separation, and never
    within 1 km of co-payloads [src SpaceX RPUG].
  - The 7-day clock persists in NVM (F8).
- [ ] **Hall startup sequence** (a `#[sequence]`):
  1. Power gate: sunlit, SoC above the floor.
  2. Heat the cathode.
  3. Ignite.
  4. Settle.
  5. Thrust arc.
  6. Shut down.
  - An abort handler closes the valves and turns the PPU off.
  - Redlines: bus undervoltage, PPU temperature, attitude error, wheel
    momentum.
- [ ] **Monoprop burn sequence:** preheat until the bed reaches temperature
      (`wait_for` with timeout), slew to burn attitude, fire, return. Same
      handler and redline pattern. The DESIGN.md burn example is this exact
      shape.
- [ ] **Burn planning.**
  - Raise: a series of Hall arcs across multiple orbits.
  - Drag make-up: triggered when the semi-major axis drops below a deadband.
  - Collision avoidance: an uplinked Δv at a time, flown chemically.
  - Disposal: a perigee-lowering campaign.
  - The onboard propagator is updated after each burn.
- [ ] Δv and propellant accounting, and PVT gauging telemetry.

---

## 7. Modes, FDIR, and LEOP

### 7.1 Spacecraft mode manager (`fsw/modes`)

```
SEPARATION → DETUMBLE → SUN_SAFE → COMMISSIONING → NOMINAL
                           ↑↓                         ├─ IMAGING
                         SAFE                         ├─ ORBIT_MAINT (Hall arcs)
                           ↑↓                         ├─ CA_BURN (chemical)
                        SURVIVAL                      └─ DOWNLINK (§13)
```

- [ ] Transition policy, after MinXSS [src]:
  - SAFE ↔ SURVIVAL is autonomous in both directions, on the battery table.
  - NOMINAL → SAFE is autonomous.
  - SAFE → NOMINAL is **ground-only**.
- [ ] Each mode maps to an ADCS law, a PCDU output set, a propulsion
      permission, and a payload permission. The manager writes these as
      commands to each subsystem.
- [ ] Mode persists in NVM, and a reboot resumes it rather than rerunning
      LEOP.

### 7.2 FDIR triggers (table-driven, commandable)

| Trigger | Response |
|---|---|
| Battery below SAFE / CRITICAL | SAFE / SURVIVAL |
| Body rate > limit for N s | DETUMBLE |
| Attitude error > X° for T, or sun lost > 1 orbit | SUN_SAFE |
| Star tracker, gyro, or GPS invalid; ephemeris stale | degrade to coarse modes |
| Wheel failure | 3-wheel reconfiguration, else SUN_SAFE on MTQs |
| Temperature above red limit | shed the unit, then SAFE |
| Burn anomaly: attitude or momentum redline, pressure drop | abort the burn, safe the valves, SAFE |
| Boot count rising > N per hour | SAFE with a minimal set |
| LCL trip count > N | leave the channel off |
| Storage > 95 % | stop low-priority partitions |
| Wheels saturated with desaturation failing | SURVIVAL |
| Semi-major axis below a floor with propulsion unavailable | ground alert (disposal planning) |

Implementation: F2 alarms are the watchpoints, and responses are sequences.
fsw-3 redlines and abort handlers cover in-sequence protection.

### 7.3 LEOP sequence [src SpaceX RPUG unless marked]

- [ ] Separation: tip-off 0.5–5°/s, arrays stowed, stowed inertia.
- [ ] **120 s** with no attitude control and no deployments.
- [ ] Array deployment: a hold-down release, an arm-then-fire interlock,
      per-wing deploy switches, a retry policy, and a battery gate. After
      deployment, switch to the deployed inertia.
- [ ] B-dot detumble on the MTQs, then sun-safe with the SADA tracking.
- [ ] GPS acquisition, onboard propagator seeding, star tracker acquisition
      (§11), and fine pointing.
- [ ] 7-day propulsion inhibit. Then Hall commissioning: cathode conditioning,
      a first short arc, and a thrust calibration against GPS-measured
      semi-major-axis change.
- [ ] Orbit raise to 550 km, then nominal operations.

---

## 8. Command and data handling

### 8.1 Sim model (`sim/obc`)

- [ ] **OBC clock.** `t_obc = t_true(1+δ) + offset`, with aging and a
      temperature term [typ]. GPS PPS discipline gives ~20 ns when locked
      [src OEM719]. GPS−UTC is 18 s. It is published as a frame that the fsw
      target's injected clock reads (F10), so the flight target's
      `Timestamp` is onboard time.
- [ ] RTC loss on power loss. The hardware watchdog resets the OBC if not
      kicked (F9).
- [ ] **SEU/SEL.** Poisson events with an SAA multiplier.
  - A corrected bit flip increments a counter.
  - An uncorrected flip corrupts a table.
  - A SEFI hangs the OBC until the watchdog resets it.
  - A SEL shows as an LCL trip.
- [ ] **NVM block (F8)** with a CRC.
- [ ] **Mass memory.** 1–4 TB, a write-rate limit, and bad blocks. SkySat
      carries 768 GB [src].

### 8.2 Flight software (`fsw/cdh`)

- [ ] **Time.** Commands the OBC clock to step or slew to GPS or a ground
      sync, and reports time quality. Flight systems read onboard time as
      their `now` through the injected clock (F10).
- [ ] **Housekeeping** collection at 0.1–1 Hz into storage partitions.
- [ ] **Events.** `LogEvent` plus alarms, stored with a severity.
- [ ] **Storage.** Partitions per data type (HK, events, high-rate ADCS,
      imagery), each with an overwrite or stop policy. Imagery dominates.
  - 10 min/orbit of 4-band pushbroom at 2.2 Gbit/s is ~165 GB raw per orbit.
  - Lossless compression gives ~2.1:1 on 12-bit data: 5.72 bpp for CCSDS 122
    [src CCSDS 120.1-G-3].
- [ ] **Scheduler.** An absolute-time command table (ATS-like), plus relative
      sequences using fsw-3 `#[sequence]`. Behavior across a clock jump or a
      reboot is defined and tested.
- [ ] **Command dispatcher (F4).** Validate, acknowledge or reject, count, and
      route.
- [ ] **Health.** Read each system's `status`, detect latched systems, and
      stop kicking the watchdog when a critical system is unhealthy.
- [ ] **Parameter tables (F7).** CRC, golden copy, and dump.
- [ ] **Software update (stretch).** File uplink into the DlSlot store, hash
      verification, `Load`, and A/B rollback.

---

## 9. Fault injection

- [ ] A `sim/faults` system takes `FaultCommand` messages from the panel or a
      scenario sequence on the plant target, and writes one fault frame per
      subsystem.
- [ ] Faults to support:

| Subsystem | Faults |
|---|---|
| EPS | cell short; capacity fade; high R0; string open; wing not deployed; SADA stuck; LCL trip; current-sensor bias; brownout |
| Thermal | heater stuck on or off; thermistor open or stuck; α degradation |
| Propulsion | valve stuck open; leak; clog; Hall flameout; cathode no-ignite; PPU trip; bed heater failure |
| ADCS | wheel failure or friction rise; gyro bias step; magnetometer offset; star tracker blinded; CSS head dead; MTQ axis open |
| C&DH | SEFI hang; reboot; RTC reset; corrupted table; bad block; SEU rate multiplier |
| Imaging | focal plane over-temperature; latch-up; hot pixels; SAA proton storm |
| Environment | F10.7 and Ap step; GPS outage |

- [ ] The flight side sees only the symptoms. Tests assert the FDIR response,
      not the fault flag.

---

## 10. Scenarios and tests

Each scenario is a `target.py` variant or param set plus a fault timeline,
with an integration test and a pass/fail oracle.

| # | Scenario | Duration | Oracle |
|---|---|---|---|
| S1 | **LEOP:** 5°/s tip-off, 120 s inhibit, one wing deploys on retry, detumble, sun-safe | ~3 h | no control before 120 s; rate < 0.1°/s within ~2 h; SoC above SAFE |
| S2 | **Nominal day:** 15 orbits, a drag make-up arc, imaging targets, desaturation | 24 h | energy positive; semi-major axis inside the deadband; images score (§12) |
| S3 | **Orbit raise:** 500 → 550 km over ~67 h of arcs with power gating | ~5 days (accelerated) | reaches 550 ± 1 km; no SAFE entries; propellant used matches Δv |
| S4 | **Collision avoidance:** 12 h warning, chemical burn with preheat | 12 h | Δv delivered within 5 %; momentum held with off-pulsing |
| S5 | **Stuck-open valve during a burn** | minutes | redline aborts; handler safes the valves; SAFE |
| S6 | **Worst-case power:** EOL arrays, SADA stuck, cold battery, Hall arc requested | 24 h | energy gate refuses the arc; no CRITICAL |
| S7 | **SEU/SAA day:** SEFI reboot mid-schedule, LCL trip on the payload | 24 h | NVM state resumes; schedule neither double-fires nor fires all at once |
| S8 | **Wheel failure** during imaging | 1 h | 3-wheel reconfiguration; degraded agility logged |
| S9 | **Disposal campaign** | weeks (accelerated) | perigee ≤ 350 km; propellant margin reported |

Test infrastructure:
- [ ] Simulated-clock tests in-process, like `convergence.rs`.
  - 24 h at 120 Hz is 10.4 M cycles. Measure throughput early and set a
    budget.
  - Multi-day scenarios (S3, S9) need a reduced-fidelity config: slow rates,
    with attitude averaged or held.
- [ ] Physical oracles as unit tests:
  - J2 node rate of 0.9856°/day at i_SSO,
  - decay rate against the §2 table,
  - array power against the cosine law,
  - OCV and coulomb conservation,
  - analytic single-node thermal steady state,
  - rocket equation against propellant used,
  - Hohmann Δv of 27.5 m/s.

---

## 11. Rendered images: star trackers and the imager

The imaging payload and star trackers are **deprioritized**. Both should come
with real rendered images, which star identification and image autonomy need,
and that is a larger piece of work than their measurement-level models. This
section is the shared rendering infrastructure. §12 builds the payload on it.

### 11.1 Star tracker images (`sim/render`)

**Reference units**

| Unit | FOV / detector | Noise | Rate | Exclusion | Power |
|---|---|---|---|---|---|
| Rocket Lab ST-16RT2 [src] | 2592 × 1944 CMOS, 7.5° × 10° half-angle, ~3 e⁻ read noise | 5″ / 55″ RMS | 2–5 Hz, lost-in-space every frame, 3°/s max | sun 34° / 22° by baffle | < 0.5 W |
| Terma T1 [src] | 20° circular, 1024² | — | 10 Hz; full performance < 0.5°/s | — | — |
| Sodern Auriga-CP [src] | — | temporal 11″ / 70″ 3σ | lost-in-space 3.8 s | sun 35°, Earth 22° | — |

**Catalog**
- [ ] Hipparcos cut at V ≤ 7.5: ~25k stars, under 1 MB [src/derived].
- [ ] Apply proper motion to epoch and ~20″ of stellar aberration.
- [ ] Average star counts in a 20° FOV: 38 stars at m < 6, 118 at m < 7
      [derived].

**Photometry** [derived]
- `e⁻ = F₀·10^(−0.4m)·A_ap·QE·τ·t`.
- On 16 mm f/1.6 optics at 100 ms, an m6 star gives 125–250 e⁻ and an SNR of
  about 9–14. The detection limit is therefore about m 6–6.5.

**Pipeline per frame**
- [ ] q_cam_eci from the true attitude and the mount; pinhole plus
      Brown–Conrady distortion.
- [ ] Cone query, then an erf-integrated Gaussian PSF (σ 0.7–1.5 px).
- [ ] Smear from sub-stepping the body rate over the exposure. At 70″/px and
      100 ms: 2.6 px at 0.5°/s, 15 px at 3°/s [derived].
- [ ] Stray light: a point-source-transmittance curve against sun, Earth-limb,
      and moon angles. Earth or moon in the FOV renders as a saturating
      extended source.
- [ ] Detector effects:
  - shot noise;
  - dark current of 10–100 e⁻/s/px, doubling every 6–8 °C (from thermal);
  - DSNU and PRNU ~1 %;
  - read noise;
  - a 12-bit ADC and full-well clipping [typ].
- [ ] **Radiation:**
  - Hot pixels at 0.01–0.1 %, growing with SAA fluence [src].
  - Particle hits as streaks. Outside the SAA there are ~5 per cm² per s,
    about 0.6 per frame; inside it, tens to ~100 per frame [typ].
- [ ] Output: the raw frame, plus a truth centroid list, so nav can run from
      centroids before star identification lands.
- [ ] **CPU cost:** a 2048² frame's noise pass takes 8–20 ms on one core, or
      2–4 ms on 8 cores with rayon. PSFs cost < 1 ms. 5–10 Hz on the CPU is
      easy, even for 3 trackers [derived].

**Flight side (`fsw/adcs`)**
- [ ] Centroiding, lost-in-space star identification, a tracking mode
      predicted from the gyro, and a quaternion into the MEKF.
- [ ] `tetra3rs` (Rust, MIT, alpha, Gaia DR3 + Hipparcos) is the candidate
      [src]. Tetra-style solving takes ~10 ms on a desktop.
- [ ] Validity flags from exclusion angles and the rate limit, and multiple
      trackers fused.

### 11.2 Earth scene images (`sim/render`)

**Ground truth**
- [ ] **EOxCloudless Sentinel-2, 10 m** [src]. Free for non-commercial use
      with attribution. Check the licence before shipping tiles or
      screenshots.
- [ ] Blue Marble at 500 m for coarse views, Black Marble for the night side,
      and **Copernicus DEM GLO-30** for terrain [src].
- [ ] Anything finer than ~5 m GSD rendered from Sentinel-2 looks soft. A
      1.83 m imager wants NAIP (0.6 m, US only, public domain) for its test
      sites.
- [ ] Global Sentinel-2 is ~4 TB, so tiles are fetched along the ground track
      into a disk cache. The panel's map `TileStore`/ureq/disk-cache code is
      the pattern to reuse.

**Pipeline per pixel**
1. Ray from K⁻¹ with distortion, then ECI → ECEF.
2. Closed-form ray–ellipsoid intersection.
3. Geodetic latitude and longitude (Bowring).
4. Optional DEM refinement, 2–3 iterations.
5. Mip level from the footprint, then a bilinear texture sample.
6. Radiometry: `L ≈ ρ·E☉·cosθ_s/(πd²)` plus path radiance.
7. Procedural clouds with shadows cast along the sun vector.
8. Gaussian MTF (σ 0.5–0.7 px).
9. Vignetting.
10. Detector noise; 12-bit quantization.

**Modes**
- [ ] Framing: one attitude per frame.
- [ ] Pushbroom: attitude and position per line, at a line rate of
      `v_ground/GSD = 6983/1.83 ≈ 3.8k lines/s` at 550 km [derived]. Per-line
      jitter comes from wheel microvibration (§3).

**Cost** [derived]
- [ ] A 4 Mpx frame at 60–150 ns/px takes 30–75 ms on 8 cores, so 10–30 fps
      with tiles cached.
- [ ] Real-time pushbroom at full swath (46 Mpx/s) needs ~5 cores. Render at
      reduced swath, or faster than real time offline.
- [ ] CPU first, because it is deterministic and testable. Move to wgpu only
      if pushbroom rate becomes the bottleneck.

**Labels for free.** The renderer knows ground truth: cloud masks, the target
footprint, and geolocation. Autonomy tests (§12) score against it directly.

### 11.3 Other rendered sensors (optional)

- [ ] IR horizon sensor: a 32 × 32 thermal image with the Earth disc near
      250 K against space and a softened CO₂ limb. Accuracy 0.2–1° [typ].

### 11.4 Framework and panel needs

- F13 (large records) and F14 (deterministic off-loop rendering).
- A panel image view: none exists (§14).

---

## 12. Payload and image autonomy

### 12.1 Payload (`sim/render` + `sim` camera model)

Imager, from §0.2 [derived]:
- 20 cm aperture, f = 1.5 m, 5 µm pixel.
- GSD `550e3·5e-6/1.5 = 1.83 m`.
- Diffraction λ/D ≈ 1.5 m on the ground; Q = 0.83.

SkySat-C, for comparison: 35 cm aperture, 0.9 m GSD, 8 km swath [src].

- [ ] Power, a focal plane thermal node, a latch-up channel, and a sun-in-FOV
      keep-out.
- [ ] Products: the rendered image, its metadata (time, true and estimated
      attitude, footprint), and a quality score from pointing error, jitter
      against IFOV, sun elevation, and cloud fraction. The clock error turns
      into an along-track offset of ~7 km per second of error.

### 12.2 Flight software (`fsw/payload`)

- [ ] **Imaging plan.** Uplinked targets (LLA, priority, window). Access
      prediction uses the onboard orbit, an off-nadir limit, and a floor on
      sun elevation at the target.
- [ ] **Imaging sequence:**
  1. Pre-slew with target tracking.
  2. Settle.
  3. Energy gate.
  4. Sun keep-out check.
  5. Capture.
  6. Compress and store.
  7. Return.
  - An abort handler turns the camera off.
  - Redlines: pointing error, focal plane temperature, battery mode.
- [ ] **Onboard autonomy:**
  - Cloud screening that drops frames more than 70 % cloudy. Φ-sat-1's
    CloudScout reached 92 % accuracy at 325 ms per inference [src].
  - Change detection against a reference mosaic.
  - Opportunistic retasking. CogniSAT-6 is the flight precedent [src].

  Rendered truth labels score each of these.
- [ ] **Downlink policy:** thumbnails and screening results first; full
      products on request (§13).

---

## 13. RF and the ground segment

This comes **last**. The planned link-modeling work fits here, so this section
records the requirements the rest of the spacecraft puts on it rather than a
design.

**Baseline hardware**
- S-band TT&C at ~2 Mbps down / 125 kbps up. Syrlinks EWC31: 10 kbps–2 Mbps
  at 27–33 dBm [src].
- X-band payload downlink at 200–500 Mbps: XLink-X up to 200 Mbps with
  adaptive coding and modulation; Aries 125/350 Mbps; SkySat 450 Mbps [src].

**Pass geometry at 550 km** [derived]
- Slant range: 1816 km at 10° elevation, 2705 km at 0°.
- Longest pass: 8.0 min above 10°, 12.2 min above 0°.
- FSPL at 1816 km: 164.7 dB at 2.245 GHz, 175.9 dB at 8.2 GHz.
- Doppler bound at ±7 km/s: ±52 kHz on S-band, ±190 kHz on X-band.

**Data volume**
- ~30–37 GB per X-band pass at 500 Mbps [derived].
- ~80 GB/orbit of compressed imagery (§8), so there is a permanent backlog.
  Queue policy is the core of the ops problem.

**Requirements from the rest of this document**
- [ ] Link availability and per-frame loss gate the real telemetry and command
      path. Operators see only what came down; a separate truth tap keeps
      engineers' full view.
- [ ] The downlink rate is a physical budget, and db gaps are expected.
- [ ] Antenna gain comes from attitude, with a ground-station tracking law
      (§3) for X-band.
- [ ] Transmitter power and heat go into EPS and thermal.
- [ ] Beacon, prioritized downlink queues, pass awareness from the onboard
      propagator, TX enable with expiry, ground watchdog, and command
      authentication.
- [ ] A `ground` target holds the stations and operator commands.
      Framework needs: the forward link reference, and possibly shared state.

---

## 14. Panel

- [ ] **Image view.** None exists today (`libs/metor-panel/src/views`). It
      should show:
  - star tracker frames with detected and truth centroids,
  - EO frames with footprint and cloud-mask overlays,
  - a pushbroom strip view.
- [ ] **Map.**
  - Multiple markers: the map takes one component today.
  - Imaging target and station markers.
  - The imager footprint and swath.
  - The eclipse terminator.
  - The burn arc track.
- [ ] **Per-subsystem presets** (F6 or files):
  - EPS: SoC, bus voltage, per-LCL traffic lights, SADA angle.
  - Thermal: node temperatures against limits.
  - Propulsion: tank pressure, bed temperature, thrust, Δv used.
  - ADCS: attitude, wheel momentum, star tracker validity.
  - C&DH: storage fill, boot count, time quality.
  - Modes: mode chip, alarms, sequences.
- [ ] A fault-injection panel.
- [ ] Exec timeline on fsw-3 (F12).
- [ ] 3D viewer: a GLB with deployed wings rotating with the SADA angle, and
      thruster plume markers.

---

## 15. Suggested order

Each step keeps the example runnable and its tests green.

1. **Restructure.** Split sim packs from flight packs, and split
   `contracts/hw` from `contracts/truth` (§0.1). Add the geometry table and
   the rate adapter (F1) or its workaround.
2. **Environment.** 550 km orbit, J2, density model, penumbra, β, moon,
   ground sites.
3. **ADCS rescale.** Mass properties, 4 wheels, MTQs, FOG, SADA, tip-off, new
   disturbances. Retune the convergence test.
4. **EPS.** Arrays with the SADA, the 8s battery, the PCDU, loads, and
   `fsw/power`. Add `limits` (F2).
5. **Thermal**, including the catalyst bed and tank nodes.
6. **Propulsion.** Hall and monoprop models, inhibits, startup and burn
   sequences, burn planning. Scenarios S3–S5.
7. **Modes, FDIR, LEOP**, with NVM (F8). Scenario S1.
8. **C&DH.** Time, HK, storage, scheduler, dispatcher (F3/F4), health,
   reboot (F9).
9. **Faults and the scenario suite** S2, S6–S9.
10. **Rendering infrastructure** (§11, with F13 and F14), star trackers, and
    the panel image view.
11. **Payload and image autonomy** (§12).
12. **RF and the ground segment** (§13), with the link-modeling work.

F1–F4 should become fsw-3 slices by step 8, and F13/F14 before step 10.
F10 (injected clocks) should land early, around step 2. Scaled time makes
every later step watchable, and driven clocks make the deployment tests
repeatable. The onboard clock itself arrives with C&DH in step 8.

---

## 16. Open decisions

- **Pack granularity.** One pack per sim subsystem and one per flight
  subsystem, or one sim pack and one flight pack with modules inside? The rule
  in §0.1 holds either way; the choice is how many crates to maintain.
- **Clock monotonicity** (F10). When the onboard clock steps backward, does
  the coordinator keep a monotonic cycle time beside it, or do consumers
  tolerate the step?
- **Reboot mechanism.** Per-system channel resets, or a target restart as a
  new fsw-3 feature?
- **Propulsion baseline.** Hall plus monoprop (the proposal), Hall only
  (Aries/Arrow style), or monoprop only (SkySat style)?
- **Long-run fidelity.** Should multi-day scenarios keep 120 Hz rigid-body
  attitude, or switch to a reduced model? This depends on measured
  throughput.
- **Imagery licensing.** EOxCloudless is non-commercial. Is that acceptable
  for an example in this repo, or should the example fall back to Blue Marble
  and NAIP (both public domain) with Sentinel-2 opt-in?
- **Star tracker stub.** Add a quaternion-level tracker early so fine pointing
  can be scored before §11, or wait for rendered images?

---

## Reference implementations

| Project | What to borrow |
|---|---|
| **Basilisk** (AVS Lab) | The power–data message graph (`simpleSolarPanel`, `simpleBattery`, `ReactionWheelPower`, `partitionedStorageUnit`), the thruster and facet drag effectors, `groundLocation`, and the `camera` module's noise set |
| **NOS3** (NASA IV&V) | Per-component hardware sims, a sim time driver, and fault injection through a terminal |
| **cFS** | SCH, HK, HS, LC, SC, DS: the FDIR and sequencing set |
| **F Prime** | `Svc/Health`, `CmdDispatcher`, `PrmDb`, data products, and `WasmSequencer`, which is prior art for fsw-3's WASM sequencing |
| **tetra3rs / cedar-solve** | Star identification; tetra3rs is Rust |
| **SatSim (AFRL)**, **PANGU (ESA)** | Star field and scene rendering references |
| **MinXSS**, **Aalto-1** | Mode and FDIR design; reboot and detumble lessons |

## Sources

Launch and bus:
- Moog ESPA user's guide: https://www.moog.com/content/dam/moog/literature/sdg/space/structures/moog-espa-users-guide-datasheet.pdf
- SpaceX Rideshare Payload User's Guide: https://storage.googleapis.com/rideshare-static/Rideshare_Payload_Users_Guide.pdf
- Rocket Lab Motorized Lightband: https://rocketlabcorp.com/space-systems/separation-systems/motorized-lightband/
- Apex Aries: https://www.apexspace.com/platforms/leo-aries
- York platforms: https://www.yorkspacesystems.com/platforms
- Airbus Arrow 150: https://airbusus.com/wp-content/uploads/2024/01/ARROW150_24.pdf
- Terran Orbital platforms: https://terranorbital.com/wp-content/uploads/2024/10/0140_TO_SPACECRAFT-PLATFORMS-BROCHURE_101024.pdf
- SkySat (eoPortal): https://www.eoportal.org/satellite-missions/skysat
- Blue Canyon Venus / Saturn-200: https://www.bluecanyontech.com/spacecraft/venus-100/

Power:
- Azur 3G30C-Advanced: https://www.azurspace.com/media/uploads/file_links/file/bdb_00010891-01-00_tj3g30-advanced_4x8.pdf
- Array degradation (E3S ESPC 2017): https://www.e3s-conferences.org/articles/e3sconf/pdf/2017/04/e3sconf_espc2017_03005.pdf
- EnerSys ABSL 8s20p: https://www.satnow.com/products/batteries/enersys/98-1271-absl-8s20p-28v-30ah
- EnerSys ABSL CM230: https://www.enersys.com/496413/globalassets/documents/product-documentation/absl/emea/emea_apac-en-ds-absl-cm230-8s4p-0722-web.pdf
- AAC Clyde STARBUCK-MINI: https://www.aac-clyde.space/wp-content/uploads/2021/11/AAC_DataSheet_Starbuck-Mini-updated.pdf
- Moog small-sat SADA: https://www.moog.com/content/dam/moog/literature/sdg/space/spacecraft-mechanisms/moog-small-sat-solar-array-drive-assembly-datasheet.pdf

Propulsion:
- Busek BHT-350: https://www.busek.com/bht350
- Busek BHT-200: https://www.busek.com/bht200
- Busek BHT-600: https://www.busek.com/bht600
- Orbion Aurora: https://orbionspace.com/wp-content/uploads/2025/12/Orbion-Space-Technology-Aurora-Data-Sheet-2026.pdf
- Heaterless hollow cathode: https://www.researchgate.net/publication/376216665_Development_of_heaterless_hollow_cathode_for_low-power_Hall_thruster
- SPT-140 swirl torque: https://arc.aiaa.org/doi/10.2514/6.2025-0062
- ECAPS HPGP: https://blog.satsearch.co/2021-05-27-on-high-performance-green-propulsion-hpgp-solutions-for-small-satellites-with-bradford-space
- Aerojet GR-1 (NTRS): https://ntrs.nasa.gov/api/citations/20140012587/downloads/20140012587.pdf
- Dawn B20: https://satsearch.co/products/dawn-aerospace-b20-thruster-green-chemical-propulsion

ADCS:
- Rocket Lab RW-1.0: https://rocketlabcorp.com/assets/Uploads/RL-RW-1.0-Data-Sheet.pdf
- Rocket Lab 12 N·m·s wheel: https://rocketlabcorp.com/assets/Uploads/Rocket-Lab-12-Nms-RW4-RW5-reaction-wheel-datasheet.pdf
- Honeywell HR 0610: https://satcatalog.s3.amazonaws.com/components/216/SatCatalog_-_Honeywell_-_HR_0610_-_Datasheet.pdf
- Northrop LN-200S: https://satcatalog.s3.amazonaws.com/components/317/SatCatalog_-_Northrop_Grumman_-_LN-200S_-_Datasheet.pdf
- KVH DSP-1760: https://www.geoobchod.cz/resources/products_documents/DS_DSP1760.pdf
- Rocket Lab ST-16RT2: https://rocketlabcorp.com/assets/Uploads/RL-ST16RT2-Data-Sheet.pdf
- Sodern Auriga-CP: https://www.cubesatshop.com/wp-content/uploads/2023/03/2022-02-Datasheet-Auriga-CP-v2.pdf
- Terma T1: https://www.terma.com/media/d1ifyxd3/terma_t1_str_data_sheet.pdf
- Pumpkin GPSRM1 (OEM719): https://www.pumpkininc.com/space/datasheet/710-00908-D_DS_GPSRM_1.pdf

Imaging and rendering:
- Yale Bright Star Catalogue: https://data.nasa.gov/dataset/bright-star-catalog
- Hipparcos: https://www.cosmos.esa.int/web/hipparcos/faqs-facts
- Star tracker hot pixels from protons: https://www.mdpi.com/2079-9292/12/7/1629
- Hot pixel annealing: https://pubs.aip.org/aip/adv/article/14/9/095221/3313804/
- tetra3rs: https://github.com/ssmichael1/tetra3rs
- cedar-solve: https://github.com/smroid/cedar-solve
- Basilisk camera: https://hanspeterschaub.info/basilisk/Documentation/simulation/sensors/camera/camera.html
- SatSim: https://github.com/ssc-ai/satsim
- PANGU: https://www.star-dundee.com/products/pangu-planet-and-asteroid-natural-scene-generation-utility/
- EOxCloudless Sentinel-2: https://collections.eurodatacube.com/eoxcloudless-s2-global-mosaic-10/
- Blue Marble NG: https://neo.gsfc.nasa.gov/archive/bluemarble/bmng/readme.pdf
- Black Marble VNP46A2: https://ladsweb.modaps.eosdis.nasa.gov/missions-and-measurements/products/VNP46A2/
- Copernicus DEM: https://registry.opendata.aws/copernicus-dem/
- CCSDS 120.1-G-3 (compression): https://ccsds.org/Pubs/120x1g3.pdf
- Φ-sat-1: https://www.eoportal.org/satellite-missions/phisat-1
- CogniSAT-6: https://ubotica.com/one-year-of-cognisat-6-pioneering-live-earth-intelligence/

RF:
- Syrlinks EWC31: https://satsearch.co/products/syrlinks-ewc31-s-s-band-transponder
- IQ Spacecom XLink-X: https://www.iq-spacecom.com/images/Datasheets/IQspacecom_XLink-X_datasheet_02.2024.pdf

Operations and FDIR:
- MinXSS-1 on-orbit: https://arxiv.org/pdf/1706.06967
- MinXSS design: https://arxiv.org/pdf/1508.05354
- Aalto-1 lessons: https://arxiv.org/pdf/2101.07722
- SGP4 accuracy (Oltrogge): https://amostech.com/TechnicalPapers/2014/Poster/OLTROGGE.pdf
- Basilisk: https://github.com/AVSLab/basilisk/tree/develop/src/simulation
- NOS3: https://github.com/nasa/nos3
- F Prime Svc: https://github.com/nasa/fprime/tree/devel/Svc
- cFS: https://github.com/nasa/cfs
