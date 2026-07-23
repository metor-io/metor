//! Shared **compile-time contracts** for the `adcs-fsw2` dlopen target (dl-open.md §8).
//!
//! This crate is the agreement every system `cdylib` is built against: the frame structs
//! that flow over the rings (their `VTable`s must match byte-for-byte on both sides of a
//! wire — the `frame_id`/`compatible()` check enforces it, descriptor.rs:123) and one
//! `Params` struct per system (the config each constructor needs, crossing `fsw_create` as
//! canonical postcard bytes — dl-open.md §6.3).
//!
//! It is shared **among the cdylibs** (`adcs-systems`/`adcs-sequences`) and linked by
//! the convergence test (to decode outputs and to register the systems statically), but it
//! is **not** linked by the target host at runtime: the host validates frames from the
//! serialized `VTable`s and encodes params from the exported `Params` schema, never linking
//! a frame or param Rust type.

use hifitime::{Duration, Epoch};
use metor_fsw_2::metor_proto::types::Timestamp;
use nox::{ArrayRepr, Quaternion, Vector, tensor};
use nox_frames::earth::{ecef_to_eci, eci_to_ecef, ned_to_ecef};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use wmm::GeodeticCoords;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// The WMM magnetic model handle — re-exported so the plant (true field) and nav (reference
/// field) can each own one without a direct `wmm` dependency. Holds C-library state and is
/// stepped with `&mut self`, so build it once per system.
pub use wmm::MagneticModel;

/// A body-frame / world-frame 3-vector in nox's array representation.
pub type V3 = Vector<f64, 3, ArrayRepr>;
/// A quaternion in nox's array representation.
pub type Quat = Quaternion<f64, ArrayRepr>;

/// Physics / FSW step (matches the cube-sat example: 120 Hz).
pub const DT: f64 = 1.0 / 120.0;

// --- Orbital constants (cube-sat values) -----------------------------------
// Shared by the plant (gravity + orbit init) and the controller/nav (pointing-law targets
// and the reference magnetic field), so they live in the contract.

/// Gravitational constant (m³·kg⁻¹·s⁻²).
pub const G: f64 = 6.6743e-11;
/// Mass of Earth (kg).
pub const M: f64 = 5.972e24;
/// Earth's gravitational parameter `G·M` (m³·s⁻²).
pub const MU: f64 = G * M;
/// Spacecraft mass (kg).
pub const MASS: f64 = 2825.2 / 1000.0;
/// Earth radius (m).
pub const EARTH_RADIUS: f64 = 6378.1e3;
/// Orbit altitude (m) — a 400 km circular orbit.
pub const ALTITUDE: f64 = 400.0e3;

/// Principal moments of inertia (kg·m²) — the cube-sat values. Shared by the plant
/// (dynamics) and the controller (LQR gain synthesis), so it lives in the contract.
pub fn inertia_diag() -> V3 {
    tensor![15204079.70002e-9, 14621352.61765e-9, 6237758.3131e-9]
}

// --- Actuator envelope --------------------------------------------------------
// The limits both sides agree on: the plant enforces them in its dynamics, and an FSW that
// knows its actuators clamps what it commands to them. The dynamics themselves (bearing
// friction, rotor inertia, the sensor/GPS error models, the disturbance environment) are
// simulation math and live in `adcs-systems`' plant module.

/// Wheel momentum saturation limit (N·m·s) — at the limit the motor folds back to zero net
/// torque; unloading torque always flows.
pub const RW_MOMENTUM_MAX: f64 = 0.04;
/// Maximum motor torque per wheel (N·m).
pub const RW_TORQUE_MAX: f64 = 0.002;
/// Maximum magnetorquer dipole per axis (A·m²) — at |B| ≈ 4e-5 T that is ~8e-6 N·m of
/// authority, plenty against ~1e-7 N·m secular disturbances but far below the wheels.
pub const MTQ_MAX_DIPOLE: f64 = 0.2;

// --- WMM magnetic field ------------------------------------------------------
// The true (plant, at its real position) and reference (nav, at the GPS position) Earth
// magnetic field, from the NOAA World Magnetic Model, replacing the crude tilted dipole.

/// WGS84 ellipsoid semi-major axis (m) and first-eccentricity-squared, for the ECEF→geodetic
/// conversion WMM's input needs.
const WGS84_A: f64 = 6378137.0;
const WGS84_E2: f64 = 6.694379990141316e-3; // e² = 2f - f², f = 1/298.257223563

/// ECEF Cartesian → WGS84 geodetic `(latitude_rad, longitude_rad, altitude_m)`, by the standard
/// iterative (Bowring-seeded) method — nox-frames has the ECI↔ECEF↔NED rotations but no geodetic
/// conversion, and WMM takes geodetic coordinates.
pub fn ecef_to_geodetic(ecef: &V3) -> (f64, f64, f64) {
    let [x, y, z] = ecef.into_buf();
    let lon = y.atan2(x);
    let p = (x * x + y * y).sqrt();
    // Seed latitude assuming spherical, then refine (a handful of iterations converge to well
    // below meter level at orbital altitude).
    let mut lat = z.atan2(p * (1.0 - WGS84_E2));
    let mut n = WGS84_A;
    for _ in 0..5 {
        n = WGS84_A / (1.0 - WGS84_E2 * lat.sin().powi(2)).sqrt();
        let alt = p / lat.cos() - n;
        lat = z.atan2(p * (1.0 - WGS84_E2 * n / (n + alt)));
    }
    let alt = p / lat.cos() - n;
    (lat, lon, alt)
}

/// The Earth magnetic field in ECI (Tesla) at an inertial position `pos_eci` and `epoch`, from
/// the NOAA WMM: rotate the position into ECEF, convert to geodetic, evaluate the model (WMM
/// wants latitude/longitude in degrees and height above the ellipsoid in **km**), then rotate
/// the NED field back through ECEF into ECI. Returned **un-normalized**. Shared by the plant
/// (true field at its real position) and nav (reference field at the GPS position).
pub fn mag_field_eci(model: &mut MagneticModel, epoch: Epoch, pos_eci: &V3) -> V3 {
    let ecef = eci_to_ecef(epoch).dot(pos_eci);
    let (lat, lon, alt) = ecef_to_geodetic(&ecef);
    let geodetic = GeodeticCoords::with_elliposid_height(
        lat.to_degrees(),
        lon.to_degrees(),
        alt / 1000.0, // WMM height is in km
    );
    let (elements, _) = model.calculate_field(epoch, geodetic);
    // WMM returns the field in the local NED frame (X north, Y east, Z down), in Tesla.
    let b_ned = V3::from_buf(elements.b_field());
    let b_ecef = ned_to_ecef(lat, lon).dot(&b_ned);
    ecef_to_eci(epoch).dot(&b_ecef)
}

/// The target start epoch (UTC) the environment models are evaluated against. A **fixed**
/// constant (not wall-clock) so a `Simulated` run is reproducible — the parity test relies on
/// the static and dlopen runs computing the identical sun direction.
pub fn target_epoch() -> Epoch {
    Epoch::from_gregorian_utc(2024, 1, 1, 0, 0, 0, 0)
}

/// The epoch `t_sim_s` seconds into the target (the plant advances this from a deterministic
/// per-cycle counter, never wall time).
pub fn epoch_at(t_sim_s: f64) -> Epoch {
    target_epoch() + Duration::from_seconds(t_sim_s)
}

/// The unit vector pointing at the sun in ECI at `epoch` — the real-world sun direction from
/// nox-frames' Vallado model, replacing the fixed fake sun direction.
pub fn sun_dir_eci(epoch: Epoch) -> V3 {
    nox_frames::earth::sun_vec(epoch)
}

/// Cylindrical-umbra Earth shadow: illuminated unless the spacecraft is on the anti-sun
/// side AND inside the shadow cylinder of radius [`EARTH_RADIUS`]. Shared by the plant (the
/// truth the sensors and SRP live under) and the eclipse test (which scans it for the
/// shadow-entry orbit phase).
///
/// Non-goals at this fidelity: the umbra cone vs. cylinder differ by under a second of a
/// ~35-minute eclipse at 400 km, the penumbra transit is ~8 s, and the Earth is a sphere
/// here (WGS84 flattening ~0.3%).
pub fn in_earth_shadow(pos_eci: &V3, sun_eci: &V3) -> bool {
    let s = sun_eci.normalize();
    let along: f64 = pos_eci.dot(&s).into_buf();
    if along >= 0.0 {
        return false; // sunward hemisphere
    }
    let perp = *pos_eci - s * along;
    let perp: f64 = perp.norm().into_buf();
    perp < EARTH_RADIUS
}

// --- Coarse sun sensing ---------------------------------------------------------

/// The CSS validity threshold: a head reading below this is noise floor, and when **every**
/// head is below it the sun is lost (eclipse). Shared by nav (the FSW-side validity gate —
/// the "intensity above threshold" logic real CSS electronics implement) and the eclipse
/// test's assertions. The dimmest lit reading of the six-face arrangement is ≥ 1/√3 ≈ 0.577
/// (any unit vector has a component that large), so 0.1 splits the bands by ~50σ of head
/// noise on each side.
pub const CSS_THRESHOLD: f64 = 0.1;

// ---------------------------------------------------------------------------
// Frames — the compile-time wire contracts
// ---------------------------------------------------------------------------

/// Simulated sensor measurements produced by the plant each cycle: the gyro rate (body
/// frame), the six coarse-sun-sensor head readings, and the magnetometer field in the body
/// frame. The inertial **references** for the vector observations are modeled by nav from
/// the orbit state (cube-sat's `Nav::from_sensors`), not handed over here — and the sun
/// **vector** is nav's to reconstruct from the raw CSS readings, never the plant's to leak.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "sensors")]
pub struct Sensors {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Measured body-frame angular rate (rad/s).
    pub gyro_b: V3,
    /// The six coarse-sun-sensor head readings, one cosine-response photodiode per body
    /// face in the order `[+X, +Y, +Z, −X, −Y, −Z]` (cube-sat's CSS arrangement): a lit
    /// head reads `n̂·ŝ_b` (clamped at zero — a 90° half-angle FOV), a dark or eclipsed
    /// head reads its noise floor. Validity is the FSW's call: see [`CSS_THRESHOLD`].
    pub css: [f64; 6],
    /// Magnetometer field observation in the body frame (Tesla).
    pub mag_b: V3,
}

/// The **GPS** measurement: the spacecraft's inertial (ECI) position + velocity, corrupted by
/// the plant's GPS error model (a first-order Gauss-Markov position error + white velocity
/// noise). This is the noisy orbit state the flight software actually flies on (cube-sat's
/// `GPS` sensor, now with noise): the controller derives its pointing-law target from it, and
/// nav evaluates its sun/magnetic references at it.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "gps")]
pub struct Gps {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Measured inertial (ECI) position (m).
    pub pos_eci: V3,
    /// Measured inertial (ECI) velocity (m/s).
    pub vel_eci: V3,
}

/// The navigation filter's attitude estimate.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "attitude_estimate")]
pub struct AttitudeEstimate {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Estimated attitude: the body←ECI rotation.
    pub q_hat_b_eci: Quat,
    /// Estimated body-frame angular rate (rad/s).
    pub omega_b: V3,
    /// Estimated gyro bias in the body frame (rad/s).
    pub b_hat_b: V3,
}

/// The plant's ground-truth **body state**: the true attitude + body rate and the inertial
/// orbit state (position/velocity), the spacecraft's real condition each cycle. The plant
/// propagates a real 400 km orbit (gravity + the orbital velocity); this frame is the truth a
/// `.so`'s statics never reach the host with, tapped from the registry to measure convergence.
/// The flight software does not consume it — nav and the controller fly on the noisy [`Gps`]
/// measurement instead — so it is truth telemetry only.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "body")]
pub struct BodyState {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// True attitude: the body←ECI rotation.
    pub q_b_eci: Quat,
    /// True body-frame angular rate (rad/s).
    pub omega_b: V3,
    /// Inertial (ECI) position (m).
    pub pos_eci: V3,
    /// Inertial (ECI) velocity (m/s).
    pub vel_eci: V3,
}

/// One body-axis-aligned reaction wheel — the plant's actuator **and** its telemetry, one and
/// the same struct. The stored `ang_momentum` is the wheel's **physical** momentum: the motor
/// spins the wheel *against* the commanded body torque (`ḣ = τ_motor + τ_friction`), and the
/// torque delivered to the body is the reaction `−ḣ`. This crate carries only the wire shape
/// and the constructors; the dynamics that fill it in (bearing friction, the saturation
/// foldback) are `adcs-systems`' `WheelDynamics`.
#[derive(
    metor_fsw_2::AsVTable,
    metor_fsw_2::Metadatatize,
    metor_fsw_2::Componentize,
    metor_fsw_2::Decomponentize,
    IntoBytes,
    Immutable,
    KnownLayout,
    FromBytes,
    Clone,
    Debug,
)]
#[repr(C)]
pub struct ReactionWheel {
    /// Body-frame spin axis (a unit vector; body axis i for wheel i).
    pub axis: V3,
    /// The wheel's physical stored angular momentum, `s·axis` (N·m·s).
    pub ang_momentum: V3,
    /// The commanded torque set point (the projection of the commanded **body** torque onto
    /// `axis` — the motor spins the wheel against it).
    pub torque_set_point: V3,
    /// The torque delivered to the **body** this step, the reaction `−ḣ·axis` (N·m).
    pub torque: V3,
    /// Signed wheel speed along `axis` (rad/s).
    pub speed: f64,
    /// Friction torque on the wheel this step (N·m) — inside the dynamics.
    pub friction: f64,
    /// `1` armed, `0` disarmed (offline — applies no control torque).
    pub arm: u8,
    _pad: [u8; 7],
}

impl ReactionWheel {
    /// A wheel on `axis`, armed or disarmed at boot.
    pub fn new(axis: V3, arm: bool) -> Self {
        Self {
            axis,
            ang_momentum: V3::zeros(),
            torque_set_point: V3::zeros(),
            torque: V3::zeros(),
            speed: 0.0,
            friction: 0.0,
            arm: arm as u8,
            _pad: [0; 7],
        }
    }

    /// Whether the wheel is armed (online).
    pub fn armed(&self) -> bool {
        self.arm != 0
    }
}

/// Reaction-wheel telemetry: the three wheels themselves (each body-axis-aligned, element i on
/// body axis i). The wheels are the plant's actuator; this is the same `[ReactionWheel; 3]` the
/// plant integrates, telemetered directly so the panel can plot each wheel's momentum/torque.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "wheels")]
pub struct Wheels {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    #[metor_fsw(nest)]
    pub wheels: [ReactionWheel; 3],
}

/// The true **world** (inertial/ECI) environment at the spacecraft each cycle: the sun
/// direction and the magnetic field the attitude sensors observe. Produced by the plant (which
/// knows the true epoch + position). Truth telemetry only — nav no longer consumes it (it
/// models its own references from the GPS position), so this is what makes the real ECI
/// sun/field visible in the panel next to nav's noisy estimate.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "world")]
pub struct World {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Unit vector pointing at the sun, in ECI (nox-frames' Vallado model).
    pub sun_eci: V3,
    /// Earth magnetic field in ECI (Tesla), the NOAA WMM at the true position/epoch.
    pub mag_eci: V3,
    /// `1` when the spacecraft is in sunlight, `0` in Earth shadow — the truth flag the
    /// panel plots against the estimator's sun-loss behavior.
    pub illuminated: u8,
    _pad: [u8; 7],
}

impl World {
    /// The truth environment for one cycle (`_pad` keeps the `#[repr(C)]` layout
    /// padding-free, zerocopy `IntoBytes` requires it).
    pub fn new(timestamp: Timestamp, sun_eci: V3, mag_eci: V3, illuminated: bool) -> Self {
        Self { timestamp, sun_eci, mag_eci, illuminated: illuminated as u8, _pad: [0; 7] }
    }
}

/// The per-cycle disturbance-torque telemetry: each environmental source in the body frame,
/// their sum, and the applied magnetorquer torque (control, so excluded from `total_b` —
/// telemetered here so the desat demo can plot authority against the environment). Truth
/// telemetry from the plant; the flight software does not consume it.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "disturb")]
pub struct Disturbances {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Gravity-gradient torque (N·m, body frame).
    pub gg_b: V3,
    /// Aerodynamic drag torque (N·m, body frame).
    pub aero_b: V3,
    /// Residual-magnetic-dipole torque (N·m, body frame).
    pub mag_b: V3,
    /// Solar-radiation-pressure torque (N·m, body frame).
    pub srp_b: V3,
    /// The applied magnetorquer torque `m × B` (N·m, body frame) — control, not disturbance.
    pub mtq_b: V3,
    /// The summed environmental torque (excludes `mtq_b`).
    pub total_b: V3,
}

/// The commanded magnetorquer dipole (the second actuator back-edge into the plant, beside
/// [`TorqueCmd`]): desaturation dumps wheel momentum through it, detumble damps body rate.
/// The plant clamps each axis to its `mtq_max_dipole` param and applies `τ = m × B_true`.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "mtq_cmd")]
pub struct MtqCmd {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Commanded magnetic dipole in the body frame (A·m²).
    pub dipole_b: V3,
}

/// The target-mode command a slot sequence emits each transition (sequences-slots.md §4):
/// the discrete ADCS phase plus the active **pointing law**. Produced by the `mode` slot's
/// occupant (`adcs-sequences`' `commissioning` / `safe_mode`) and consumed by the controller, which
/// selects its target attitude from `law`. `_pad` keeps the `#[repr(C)]` layout padding-free
/// (zerocopy `IntoBytes` requires it).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "mode_cmd")]
pub struct ModeCmd {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// The commanded target phase: `0` idle, `1` settling, `2` pointing, `3` safe.
    pub mode: u8,
    /// The commanded pointing law: `0` nadir, `1` velocity-vector (HIL).
    pub law: u8,
    _pad: [u8; 6],
}

impl ModeCmd {
    /// The target-phase byte values, mirrored by the example test's assertions.
    pub const IDLE: u8 = 0;
    pub const SETTLING: u8 = 1;
    pub const POINTING: u8 = 2;
    pub const SAFE: u8 = 3;
    /// Magnetorquer detumble: the commissioning ladder's first rung when the boot rate is
    /// beyond what the wheels should capture.
    pub const DETUMBLE: u8 = 4;

    /// The pointing-law byte values (cube-sat's `Mode::{NadirPoint, HilPoint}`).
    pub const LAW_NADIR: u8 = 0;
    pub const LAW_HIL: u8 = 1;
    /// Magnetorquer-only rate damping (B-cross): the wheels idle while the torquers bleed the
    /// body rate. Selectable but not yet commanded by any sequence.
    pub const LAW_DETUMBLE: u8 = 2;

    /// A `ModeCmd` for `mode` + `law`, with a zero timestamp (the sequence has no per-cycle
    /// `now` handle; the field is telemetry ordering only — the slot's `SequenceStatus`
    /// carries the authoritative run state).
    const fn at(mode: u8, law: u8) -> Self {
        Self { timestamp: Timestamp(0), mode, law, _pad: [0; 6] }
    }

    /// Idle — no active pointing (holds nadir).
    pub const fn idle() -> Self {
        Self::at(Self::IDLE, Self::LAW_NADIR)
    }
    /// Settling — reaction wheels enabled, slewing toward the velocity-vector target.
    pub const fn settling() -> Self {
        Self::at(Self::SETTLING, Self::LAW_HIL)
    }
    /// Pointing — converged, holding the velocity-vector (HIL) target.
    pub const fn pointing() -> Self {
        Self::at(Self::POINTING, Self::LAW_HIL)
    }
    /// Safe — the safing branch (entered on abort): nadir-pointing.
    pub const fn safe() -> Self {
        Self::at(Self::SAFE, Self::LAW_NADIR)
    }
    /// Detumble — magnetorquer-only rate damping (the wheels idle under `LAW_DETUMBLE`).
    pub const fn detumble() -> Self {
        Self::at(Self::DETUMBLE, Self::LAW_DETUMBLE)
    }

    /// The same command stamped with the sequence's cycle time (`sequence::now()`,
    /// review E7) instead of the constructors' zero placeholder.
    pub const fn stamped(mut self, now: Timestamp) -> Self {
        self.timestamp = now;
        self
    }
}

/// The commanded body-frame control torque (the feedback back-edge into the plant).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "torque_cmd")]
pub struct TorqueCmd {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Commanded control torque in the body frame (N·m).
    pub torque_b: V3,
}

// ---------------------------------------------------------------------------
// Pointing laws — the controller target as a function of the orbit state
// ---------------------------------------------------------------------------

/// The shortest-arc body←ECI quaternion that rotates the spacecraft's `-Y` body axis onto the
/// inertial direction `dir_eci` (cube-sat `FSW::nadir_point` / `hil_point`).
fn point_minus_y_at(dir_eci: V3) -> Quat {
    let r = dir_eci.normalize();
    let body_axis: V3 = tensor![0.0, -1.0, 0.0];
    let [x, y, z] = body_axis.cross(&r).into_buf();
    let w = 1.0 + body_axis.dot(&r).into_buf();
    Quat::new(w, x, y, z).normalize()
}

/// Nadir-pointing target: point at the (negated) position vector — i.e. down at Earth.
pub fn nadir_point(pos_eci: &V3) -> Quat {
    point_minus_y_at(pos_eci.normalize())
}

/// Velocity-vector ("HIL") pointing target: point along the orbital velocity.
pub fn hil_point(vel_eci: &V3) -> Quat {
    point_minus_y_at(vel_eci.normalize())
}

/// The controller's target attitude for a given pointing `law` and orbit state, with the
/// NaN-guard from cube-sat (`main.rs:405`): a non-finite target falls back to identity.
pub fn target_for(law: u8, pos_eci: &V3, vel_eci: &V3) -> Quat {
    let t = match law {
        ModeCmd::LAW_NADIR => nadir_point(pos_eci),
        ModeCmd::LAW_HIL => hil_point(vel_eci),
        _ => Quat::identity(),
    };
    if t.0.into_buf().iter().any(|f| !f.is_finite()) {
        Quat::identity()
    } else {
        t
    }
}

// ---------------------------------------------------------------------------
// Magnetorquer laws — the commanded dipole as a function of the measured state
// ---------------------------------------------------------------------------

/// `k·(x × B)/|B|²` — the shared shape of both magnetorquer laws: the resulting torque
/// `m × B = −k·x_⊥` opposes the component of `x` perpendicular to the field (the parallel
/// component is unreachable this instant; the orbit rotates B̂ to get at it). Returns zero
/// when the field is degenerate.
fn cross_dipole(k: f64, x_b: &V3, mag_b: &V3) -> V3 {
    let b2: f64 = mag_b.dot(mag_b).into_buf();
    if b2 < 1e-18 {
        return V3::zeros();
    }
    x_b.cross(mag_b) * (k / b2)
}

/// The momentum-desaturation dipole: `m = k·(h_w × B)/|B|²` bleeds the stored wheel momentum
/// through the torquers (`τ = −k·h_⊥`) while the attitude controller holds pointing.
pub fn desat_dipole(k_desat: f64, h_wheels_b: &V3, mag_b: &V3) -> V3 {
    cross_dipole(k_desat, h_wheels_b, mag_b)
}

/// The detumble dipole (gyro-based **B-cross**): `m = k·(ω × B)/|B|²` damps the body rate
/// (`τ = −k·ω_⊥`). Equivalent to classic Ḃ-based B-dot (`Ḃ_b = −ω × B_b` for a slowly-varying
/// inertial field) but immune to the derivative being noise-dominated at a 120 Hz sample rate.
pub fn detumble_dipole(k_detumble: f64, omega_b: &V3, mag_b: &V3) -> V3 {
    cross_dipole(k_detumble, omega_b, mag_b)
}

/// Per-axis dipole clamp to the torquer's authority (±`max` A·m²).
pub fn clamp_dipole(m: V3, max: f64) -> V3 {
    V3::from_buf(m.into_buf().map(|x| x.clamp(-max, max)))
}

// ---------------------------------------------------------------------------
// Convergence sampling (the test reads these off the tapped truth/orbit outputs)
// ---------------------------------------------------------------------------

/// The convergence sample a [`BodyState`] yields against the **identity** target:
/// `(attitude_error_rad, body_rate_rad_s)`. Retained for a fixed-target reference.
pub fn convergence_sample(b: &BodyState) -> (f64, f64) {
    let target = Quat::identity();
    let err = b.q_b_eci.angular_distance(&target).into_buf().abs();
    let rate = b.omega_b.norm().into_buf();
    (err, rate)
}

/// The convergence sample against the **commanded** pointing-law target: the attitude
/// tracking error (rad) to the `law`'s target for this body/orbit state, and the body-rate
/// magnitude (rad/s). This is how the closed loop is judged once the controller points at a
/// moving (nadir/velocity-vector) target rather than a fixed identity attitude.
pub fn tracking_sample(body: &BodyState, law: u8) -> (f64, f64) {
    let target = target_for(law, &body.pos_eci, &body.vel_eci);
    let err = body.q_b_eci.angular_distance(&target).into_buf().abs();
    let rate = body.omega_b.norm().into_buf();
    (err, rate)
}

// ---------------------------------------------------------------------------
// Per-system Params — the config each constructor needs (dl-open.md §6.3)
// ---------------------------------------------------------------------------
//
// Each derives `Serialize`/`Deserialize`/`Schema` — the postcard contract across
// `fsw_create`, and `Deserialize` is also what the static `Registry` path uses to
// read the same params off `target.kdl` (the parity test's static path).

/// Plant parameters: the initial attitude/rate offset, the sensor-noise sigma, the RNG seed
/// (so a run is reproducible), whether the reaction wheels boot disarmed, and the disturbance
/// environment. The disturbance defaults are physically honest for a ~3 kg spacecraft at
/// 400 km (secular torques ~1e-7 N·m — days to load a wheel); tests and demos crank them so
/// momentum management is visible in seconds.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq, metor_fsw_2::ParamsDocs)]
pub struct PlantParams {
    /// Initial attitude offset from the target, radians about the [1,1,1] axis.
    pub init_angle: f64,
    /// Initial body-rate magnitude, rad/s about [1,1,1].
    pub init_rate: f64,
    /// 1-sigma sensor noise (rad/s for gyro, unitless for the normalized sun vector; the
    /// magnetometer's Tesla-valued sigma lives with the plant's sensor model).
    pub meas_sigma: f64,
    /// RNG seed, so a run is reproducible.
    pub seed: u64,
    /// Bring the spacecraft up with every reaction wheel offline (the `--disarmed` parity):
    /// no control torque is applied until the wheels are armed. Defaults to `false`.
    #[serde(default)]
    pub disarmed: bool,
    /// Atmospheric density (kg/m³) — ~3e-12 at 400 km, solar-mean.
    #[serde(default = "default_rho")]
    pub rho: f64,
    /// Drag coefficient.
    #[serde(default = "default_cd")]
    pub cd: f64,
    /// Aerodynamic reference area (m²).
    #[serde(default = "default_area")]
    pub area_aero: f64,
    /// Center-of-pressure offset from the center of mass, body frame (m) — the drag/SRP
    /// torque arm.
    #[serde(default = "default_cp_offset")]
    pub cp_offset_b: [f64; 3],
    /// Residual magnetic dipole, body frame (A·m²).
    #[serde(default = "default_m_res")]
    pub m_res_b: [f64; 3],
    /// SRP reference area (m²).
    #[serde(default = "default_area")]
    pub area_srp: f64,
    /// SRP reflectivity coefficient (1 absorbing … 2 mirror).
    #[serde(default = "default_cr")]
    pub cr: f64,
    /// Per-axis magnetorquer dipole limit (A·m²).
    #[serde(default = "default_mtq_max")]
    pub mtq_max_dipole: f64,
    /// Per-wheel stored-momentum preload (N·m·s) — gives the desat demos/tests something to
    /// dump at boot. Defaults to zero.
    #[serde(default)]
    pub init_wheel_h: f64,
    /// Initial in-plane orbit phase (rad): rotates the boot position/velocity around the
    /// orbit, `r·(cos θ, sin θ, 0)` / `v·(−sin θ, cos θ, 0)`. Zero is the classic +X boot
    /// (entirely sunlit for the test windows); the eclipse tests crank it to start in or
    /// near Earth shadow. Deterministic — no RNG involved.
    #[serde(default)]
    pub init_orbit_phase: f64,
}

fn default_rho() -> f64 {
    3e-12
}
fn default_cd() -> f64 {
    2.2
}
fn default_area() -> f64 {
    0.03
}
fn default_cp_offset() -> [f64; 3] {
    [0.02, 0.0, 0.0]
}
fn default_m_res() -> [f64; 3] {
    [0.002, 0.002, 0.002]
}
fn default_cr() -> f64 {
    1.5
}
fn default_mtq_max() -> f64 {
    MTQ_MAX_DIPOLE
}

/// Navigation-filter (MEKF) parameters.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq, metor_fsw_2::ParamsDocs)]
pub struct NavParams {
    /// MEKF measurement 1-sigma for the two vector observations.
    pub meas_sigma: f64,
}

/// Controller (Yang-LQR + magnetorquer laws) parameters.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq, metor_fsw_2::ParamsDocs)]
pub struct CtrlParams {
    /// LQR attitude/rate state weight (q) and control weight (r).
    pub q_weight: f64,
    pub r_weight: f64,
    /// Momentum-desaturation gain (1/s): at |B| ≈ 4e-5 T, 5e-4 gives a ~2000 s unloading time
    /// constant — MTQ desat is honestly slow. Zero disables desat.
    #[serde(default = "default_k_desat")]
    pub k_desat: f64,
    /// B-cross detumble gain (N·m·s/rad): 5e-5 saturates the 0.2 A·m² torquer near
    /// |ω| ≈ 0.16 rad/s and damps with an unsaturated time constant of ~300 s.
    #[serde(default = "default_k_detumble")]
    pub k_detumble: f64,
}

fn default_k_desat() -> f64 {
    5e-4
}
fn default_k_detumble() -> f64 {
    5e-5
}

/// The target's flight gains. `Ctrl` is `#[system]`-authored, so this
/// `Default` also becomes the pack entry's declared defaults blob on the
/// dlopen path — a `system` node may spell only its overrides.
impl Default for CtrlParams {
    fn default() -> Self {
        Self {
            q_weight: 5.0,
            r_weight: 8.0,
            k_desat: default_k_desat(),
            k_detumble: default_k_detumble(),
        }
    }
}

/// The commissioning sequence's gates and budgets — every phase transition is
/// condition-based, and every phase has a timeout that safes the spacecraft
/// ([`Outcome::Failed`](metor_fsw_2::Outcome)). Spelled out in full on the target's
/// `allow occupant="commissioning"` line (the dlopen occupant encoder has no serde
/// defaults), which is also how tests patch individual gates.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq, metor_fsw_2::ParamsDocs)]
pub struct CommissioningParams {
    /// Enter the detumble phase only above this estimated rate (rad/s). Sized by wheel
    /// capture: absorbing 1.0 rad/s loads the worst axis to ≈38% of the momentum limit, so
    /// anything slower goes straight to the wheels and the B-cross phase is reserved for
    /// genuinely hot tumbles.
    pub rate_detumble_enter: f64,
    /// Leave detumble below this estimated rate (rad/s) — hysteresis against `enter`.
    pub rate_detumble_exit: f64,
    /// Estimator-settle gate: successive q̂ deltas below this (rad)…
    pub est_delta_rad: f64,
    /// …for this long (s) completes warm-up.
    pub est_dwell_s: f64,
    /// Coarse-pointing gate: tracking error to the commanded law's target below this (rad)…
    pub coarse_err_rad: f64,
    /// …for this long (s) advances to fine pointing.
    pub coarse_dwell_s: f64,
    /// Fine pointing confirms (→ `Completed`) after the error holds for this long (s).
    pub confirm_dwell_s: f64,
    /// Per-phase timeouts (s): expiry publishes `ModeCmd::safe` and fails the sequence.
    pub warmup_timeout_s: f64,
    pub detumble_timeout_s: f64,
    pub settle_timeout_s: f64,
    pub confirm_timeout_s: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The WMM field chain (ECI → ECEF → geodetic → WMM → ECI) returns a physically sane field
    /// at a 400 km orbit: an Earth surface field is ~25–65 µT, weaker aloft, so the magnitude
    /// should land in the tens-of-µT band — a guard that the km/deg units and frame rotations
    /// are right (a meters-for-km height slip, say, would blow WMM far out of range).
    #[test]
    fn wmm_field_is_sane_at_orbit() {
        let mut model = MagneticModel::default();
        let radius = EARTH_RADIUS + ALTITUDE;
        // A few positions around the orbit, so we exercise more than one geodetic latitude.
        for pos in [
            tensor![radius, 0.0, 0.0],
            tensor![0.0, radius, 0.0],
            (tensor![1.0, 1.0, 1.0] as V3).normalize() * radius,
        ] {
            let b = mag_field_eci(&mut model, target_epoch(), &pos);
            let mag = b.norm().into_buf();
            assert!(
                (1.0e-5..6.0e-5).contains(&mag),
                "WMM field magnitude at 400 km should be tens of µT, got {mag} T"
            );
        }
    }

    /// The shadow cylinder from first principles: sub-solar lit, anti-solar at orbit radius
    /// shadowed, terminator-perpendicular lit (outside the cylinder), and an anti-sun point
    /// nudged just past one Earth radius of lateral offset lit again.
    #[test]
    fn earth_shadow_geometry() {
        let sun: V3 = (tensor![0.2, -0.9, -0.4] as V3).normalize();
        let r = EARTH_RADIUS + ALTITUDE;
        assert!(!in_earth_shadow(&(sun * r), &sun), "sub-solar point is lit");
        assert!(in_earth_shadow(&(sun * -r), &sun), "anti-solar point is shadowed");
        // A direction perpendicular to the sun line: on the terminator, outside the cylinder.
        let perp = sun.cross(&tensor![0.0, 0.0, 1.0]).normalize();
        assert!(!in_earth_shadow(&(perp * r), &sun), "terminator point is lit");
        // Anti-sun but laterally offset past the cylinder radius: lit.
        let graze = (sun * -r) + perp * (EARTH_RADIUS * 1.01);
        assert!(!in_earth_shadow(&graze, &sun), "outside the shadow cylinder is lit");
        let inside = (sun * -r) + perp * (EARTH_RADIUS * 0.99);
        assert!(in_earth_shadow(&inside, &sun), "inside the shadow cylinder is dark");
    }

    /// Both magnetorquer laws produce a torque `m × B` that opposes their regulated quantity
    /// (stored momentum / body rate) for generic geometry, do nothing useful for the
    /// unreachable field-parallel component, and guard a degenerate field.
    #[test]
    fn dipole_laws_oppose_momentum_and_rate() {
        let b: V3 = tensor![2e-5, -1e-5, 3e-5];
        for x in [
            tensor![0.01, 0.02, -0.005] as V3,
            tensor![-1.0, 0.5, 0.25] as V3,
            tensor![0.0, 3e-2, 0.0] as V3,
        ] {
            let tau = desat_dipole(0.1, &x, &b).cross(&b);
            let along: f64 = tau.dot(&x).into_buf();
            assert!(along < 0.0, "desat torque does not oppose momentum: {along}");
            let tau = detumble_dipole(0.1, &x, &b).cross(&b);
            let along: f64 = tau.dot(&x).into_buf();
            assert!(along < 0.0, "detumble torque does not oppose rate: {along}");
        }
        // Field-parallel input: the cross law has no authority there.
        let parallel = b * 2.0e3;
        let tau = desat_dipole(0.1, &parallel, &b).cross(&b);
        let t: f64 = tau.norm().into_buf();
        assert!(t < 1e-12, "cross law claimed authority along B: {t}");
        // Degenerate field guard.
        let zero: f64 = desat_dipole(0.1, &parallel, &V3::zeros()).norm().into_buf();
        assert_eq!(zero, 0.0);
    }

    /// `ecef_to_geodetic` recovers the altitude of a known orbit radius (on the equator, the
    /// geodetic height above the ellipsoid is `|r| - a`).
    #[test]
    fn geodetic_recovers_equatorial_altitude() {
        let radius = EARTH_RADIUS + ALTITUDE;
        let (lat, _lon, alt) = ecef_to_geodetic(&tensor![radius, 0.0, 0.0]);
        assert!(lat.abs() < 1e-9, "equatorial point has ~zero latitude, got {lat}");
        assert!(
            (alt - (radius - WGS84_A)).abs() < 1.0,
            "recovered altitude within 1 m of |r| - a: {alt}"
        );
    }
}
