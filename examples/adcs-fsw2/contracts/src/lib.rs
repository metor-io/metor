//! Shared **compile-time contracts** for the `adcs-fsw2` dlopen mission (dl-open.md §8).
//!
//! This crate is the agreement every system `cdylib` is built against: the frame structs
//! that flow over the rings (their `VTable`s must match byte-for-byte on both sides of a
//! wire — the `frame_id`/`compatible()` check enforces it, descriptor.rs:123) and one
//! `Params` struct per system (the config each constructor needs, crossing `fsw_create` as
//! canonical postcard bytes — dl-open.md §6.3).
//!
//! It is shared **among the cdylibs** (`adcs-plant`/`adcs-nav`/`adcs-ctrl`) and linked by
//! the convergence test (to decode outputs and to register the systems statically), but it
//! is **not** linked by the mission host at runtime: the host validates frames from the
//! serialized `VTable`s and encodes params from the exported `Params` schema, never linking
//! a frame or param Rust type.

use metor_fsw_2::metor_proto::types::Timestamp;
use nox::{ArrayRepr, Quaternion, Vector, tensor};
use postcard_schema::Schema;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

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

/// The Earth dipole coefficient vector (T) — the magnetometer reference model. Shared by
/// the plant (true field at its position) and nav (reference field at the GPS position).
pub fn k0() -> V3 {
    tensor![-30926.00e-9, 5817.00e-9, -2318.00e-9]
}

/// The modeled Earth magnetic field (ECI) at an inertial position `pos_eci`, from the tilted
/// dipole `k0()` (cube-sat `Mag::from_body` / `Nav::from_sensors`). Returned **un-normalized**.
pub fn mag_field_eci(pos_eci: &V3) -> V3 {
    let pos_norm = pos_eci.norm().into_buf();
    let e_hat = pos_eci.normalize();
    ((EARTH_RADIUS / pos_norm).powi(3)) * (3.0 * k0().dot(&e_hat) * e_hat - k0())
}

// ---------------------------------------------------------------------------
// Frames — the compile-time wire contracts
// ---------------------------------------------------------------------------

/// Simulated sensor measurements produced by the plant each cycle: the gyro rate (body
/// frame) plus two normalized vector observations (sun + magnetometer) in the body frame.
/// The inertial **references** for those observations are modeled by nav from the orbit
/// state (cube-sat's `Nav::from_sensors`), not handed over here.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "sensors")]
pub struct Sensors {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Measured body-frame angular rate (rad/s).
    pub gyro_b: V3,
    /// Normalized sun observation in the body frame.
    pub sun_b: V3,
    /// Normalized magnetometer observation in the body frame.
    pub mag_b: V3,
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

/// The spacecraft's inertial **orbit state** (the GPS product): position and velocity. The
/// plant propagates a real 400 km orbit (gravity + the orbital velocity), and this frame
/// feeds both nav (the magnetic-field reference is a function of position) and the
/// controller (the Nadir/HIL pointing-law target is a function of position/velocity).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "orbit_state")]
pub struct OrbitState {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Inertial (ECI) position (m).
    pub pos_eci: V3,
    /// Inertial (ECI) velocity (m/s).
    pub vel_eci: V3,
}

/// Reaction-wheel telemetry: per-wheel speed, stored angular momentum, and applied torque
/// (each wheel is body-axis-aligned, so the i-th element is wheel i, on body axis i). The
/// wheels are the plant's actuator — this frame is telemetered (fan-out 0) so the panel can
/// plot the momentum building up as the spacecraft detumbles.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "wheels")]
pub struct Wheels {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Per-wheel angular speed (rad/s); element i is the body-axis-i wheel.
    pub speed: V3,
    /// Per-wheel stored angular momentum about its body axis (N·m·s).
    pub momentum_b: V3,
    /// Per-wheel applied torque about its body axis (N·m).
    pub torque_b: V3,
}

/// The mission-mode command a slot sequence emits each transition (sequences-slots.md §4):
/// the discrete ADCS phase plus the active **pointing law**. Produced by the `mode` slot's
/// occupant (`adcs-commissioning` / `adcs-safe-mode`) and consumed by the controller, which
/// selects its target attitude from `law`. `_pad` keeps the `#[repr(C)]` layout padding-free
/// (zerocopy `IntoBytes` requires it).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "mode_cmd")]
pub struct ModeCmd {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// The commanded mission phase: `0` idle, `1` settling, `2` pointing, `3` safe.
    pub mode: u8,
    /// The commanded pointing law: `0` nadir, `1` velocity-vector (HIL).
    pub law: u8,
    _pad: [u8; 6],
}

impl ModeCmd {
    /// The mission-phase byte values, mirrored by the example test's assertions.
    pub const IDLE: u8 = 0;
    pub const SETTLING: u8 = 1;
    pub const POINTING: u8 = 2;
    pub const SAFE: u8 = 3;

    /// The pointing-law byte values (cube-sat's `Mode::{NadirPoint, HilPoint}`).
    pub const LAW_NADIR: u8 = 0;
    pub const LAW_HIL: u8 = 1;

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

/// Ground-truth attitude + body rate the plant knows (for the convergence assertion).
/// Produced but, in this graph, not consumed by another system — tapped from the output
/// registry to measure convergence (dl-open.md §8), which is how the dlopen run is
/// observed without a process-global shared log (a `.so`'s statics never reach the host).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "truth")]
pub struct Truth {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// True attitude: the body←ECI rotation.
    pub q_true_b_eci: Quat,
    /// True body-frame angular rate (rad/s).
    pub omega_true_b: V3,
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
// Convergence sampling (the test reads these off the tapped truth/orbit outputs)
// ---------------------------------------------------------------------------

/// The convergence sample a [`Truth`] frame yields against the **identity** target:
/// `(attitude_error_rad, body_rate_rad_s)`. Retained for a fixed-target reference.
pub fn convergence_sample(t: &Truth) -> (f64, f64) {
    let target = Quat::identity();
    let err = t.q_true_b_eci.angular_distance(&target).into_buf().abs();
    let rate = t.omega_true_b.norm().into_buf();
    (err, rate)
}

/// The convergence sample against the **commanded** pointing-law target: the attitude
/// tracking error (rad) to the `law`'s target for this orbit state, and the body-rate
/// magnitude (rad/s). This is how the closed loop is judged once the controller points at a
/// moving (nadir/velocity-vector) target rather than a fixed identity attitude.
pub fn tracking_sample(truth: &Truth, orbit: &OrbitState, law: u8) -> (f64, f64) {
    let target = target_for(law, &orbit.pos_eci, &orbit.vel_eci);
    let err = truth.q_true_b_eci.angular_distance(&target).into_buf().abs();
    let rate = truth.omega_true_b.norm().into_buf();
    (err, rate)
}

// ---------------------------------------------------------------------------
// Per-system Params — the config each constructor needs (dl-open.md §6.3)
// ---------------------------------------------------------------------------
//
// Each derives `Schema` (so the dlopen host can encode it across `fsw_create`) AND
// `FromKdlNode` (so the same params parse when a system is linked statically and resolved
// from the same `mission.kdl` via a `Registry` — the parity test's static path).

/// Plant parameters: the initial attitude/rate offset, the sensor-noise sigma, the RNG seed
/// (so a run is reproducible), and whether the reaction wheels boot disarmed.
#[derive(Serialize, Deserialize, Schema, metor_fsw_2::wiring::FromKdlNode, Clone, Debug, PartialEq)]
pub struct PlantParams {
    /// Initial attitude offset from the target, radians about the [1,1,1] axis.
    pub init_angle: f64,
    /// Initial body-rate magnitude, rad/s about [1,1,1].
    pub init_rate: f64,
    /// 1-sigma sensor noise (rad/s for gyro, unitless for the normalized vectors).
    pub meas_sigma: f64,
    /// RNG seed, so a run is reproducible.
    pub seed: u64,
    /// Bring the spacecraft up with every reaction wheel offline (the `--disarmed` parity):
    /// no control torque is applied until the wheels are armed. Defaults to `false`.
    #[serde(default)]
    pub disarmed: bool,
}

/// Navigation-filter (MEKF) parameters.
#[derive(Serialize, Deserialize, Schema, metor_fsw_2::wiring::FromKdlNode, Clone, Debug, PartialEq)]
pub struct NavParams {
    /// MEKF measurement 1-sigma for the two vector observations.
    pub meas_sigma: f64,
}

/// Controller (Yang-LQR) parameters.
#[derive(Serialize, Deserialize, Schema, metor_fsw_2::wiring::FromKdlNode, Clone, Debug, PartialEq)]
pub struct CtrlParams {
    /// LQR attitude/rate state weight (q) and control weight (r).
    pub q_weight: f64,
    pub r_weight: f64,
}
