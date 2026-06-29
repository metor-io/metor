//! Shared **compile-time contracts** for the `adcs-fsw2` dlopen mission (dl-open.md §8).
//!
//! This crate is the agreement every system `cdylib` is built against: the four frame
//! structs that flow over the rings (their `VTable`s must match byte-for-byte on both
//! sides of a wire — the `frame_id`/`compatible()` check enforces it, descriptor.rs:123)
//! and one `Params` struct per system (the config each constructor needs, crossing
//! `fsw_create` as canonical postcard bytes — dl-open.md §6.3).
//!
//! It is shared **among the cdylibs** (`adcs-plant`/`adcs-nav`/`adcs-ctrl`) and linked by
//! the convergence test (to decode outputs), but it is **not** linked by the mission host
//! at runtime: the host validates frames from the serialized `VTable`s and encodes params
//! from the exported `Params` schema, never linking a frame or param Rust type.

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

/// Principal moments of inertia (kg·m²) — the cube-sat values. Shared by the plant
/// (dynamics) and the controller (LQR gain synthesis), so it lives in the contract.
pub fn inertia_diag() -> V3 {
    tensor![15204079.70002e-9, 14621352.61765e-9, 6237758.3131e-9]
}

// ---------------------------------------------------------------------------
// Frames — the compile-time wire contracts (moved verbatim from the monolith)
// ---------------------------------------------------------------------------

/// Simulated sensor measurements produced by the plant each cycle: the gyro rate
/// (body frame) plus two normalized vector observations and their inertial
/// references (sun + magnetometer) feeding the MEKF.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "sensors")]
pub struct Sensors {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub gyro: V3,
    pub sun_body: V3,
    pub mag_body: V3,
    pub sun_ref: V3,
    pub mag_ref: V3,
}

/// The navigation filter's attitude estimate.
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "attitude_estimate")]
pub struct AttitudeEstimate {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub q_hat: Quat,
    pub omega: V3,
    pub b_hat: V3,
}

/// The mission-mode command a slot sequence emits each transition (sequences-slots.md §4):
/// the discrete ADCS mode the spacecraft should be in. Produced by the `mode` slot's occupant
/// (`adcs-commissioning` / `adcs-safe-mode`) and, in this graph, telemetered only — no system
/// consumes it, so it demonstrates the slot writing its own frame alongside the plant/nav/ctrl
/// loop. `_pad` keeps the `#[repr(C)]` layout padding-free (zerocopy `IntoBytes` requires it).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "mode_cmd")]
pub struct ModeCmd {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// The commanded mode: `0` idle, `1` settling, `2` pointing, `3` safe.
    pub mode: u8,
    _pad: [u8; 7],
}

impl ModeCmd {
    /// The mode byte values, mirrored by the example test's assertions.
    pub const IDLE: u8 = 0;
    pub const SETTLING: u8 = 1;
    pub const POINTING: u8 = 2;
    pub const SAFE: u8 = 3;

    /// A `ModeCmd` for `mode`, with a zero timestamp (the sequence has no per-cycle `now`
    /// handle; the field is telemetry ordering only — the slot's `SequenceStatus` carries the
    /// authoritative run state).
    const fn at(mode: u8) -> Self {
        Self { timestamp: Timestamp(0), mode, _pad: [0; 7] }
    }

    /// Idle — no active pointing.
    pub const fn idle() -> Self {
        Self::at(Self::IDLE)
    }
    /// Settling — reaction wheels enabled, damping toward the target.
    pub const fn settling() -> Self {
        Self::at(Self::SETTLING)
    }
    /// Pointing — converged, holding the target attitude.
    pub const fn pointing() -> Self {
        Self::at(Self::POINTING)
    }
    /// Safe — the safing branch (entered on abort).
    pub const fn safe() -> Self {
        Self::at(Self::SAFE)
    }
}

/// The commanded body-frame control torque (the feedback back-edge into the plant).
#[derive(metor_fsw_2::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone)]
#[repr(C)]
#[metor_fsw(name = "torque_cmd")]
pub struct TorqueCmd {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub torque: V3,
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
    pub q_true: Quat,
    pub omega_true: V3,
}

/// The convergence sample a [`Truth`] frame yields: `(attitude_error_rad, body_rate_rad_s)`
/// — the attitude error angle to the (identity) target and the body-rate magnitude. This is
/// exactly what the monolith logged to its process-global `SIM_LOG`; deriving it from the
/// tapped `truth` output instead lets the **dlopen** plant (whose statics live in the `.so`)
/// be observed by the host/test through the rings.
pub fn convergence_sample(t: &Truth) -> (f64, f64) {
    let target = Quat::identity();
    let err = t.q_true.angular_distance(&target).into_buf().abs();
    let rate = t.omega_true.norm().into_buf();
    (err, rate)
}

// ---------------------------------------------------------------------------
// Per-system Params — the config each constructor needs (dl-open.md §6.3)
// ---------------------------------------------------------------------------

/// Plant parameters: the initial attitude/rate offset, the sensor-noise sigma, and the
/// RNG seed (so a run is reproducible). Crosses `fsw_create` as canonical postcard bytes.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq)]
pub struct PlantParams {
    /// Initial attitude offset from the target, radians about the [1,1,1] axis.
    pub init_angle: f64,
    /// Initial body-rate magnitude, rad/s about [1,1,1].
    pub init_rate: f64,
    /// 1-sigma sensor noise (rad/s for gyro, unitless for the normalized vectors).
    pub meas_sigma: f64,
    /// RNG seed, so a run is reproducible.
    pub seed: u64,
}

/// Navigation-filter (MEKF) parameters.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq)]
pub struct NavParams {
    /// MEKF measurement 1-sigma for the two vector observations.
    pub meas_sigma: f64,
}

/// Controller (Yang-LQR) parameters.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq)]
pub struct CtrlParams {
    /// LQR attitude/rate state weight (q) and control weight (r).
    pub q_weight: f64,
    pub r_weight: f64,
}
