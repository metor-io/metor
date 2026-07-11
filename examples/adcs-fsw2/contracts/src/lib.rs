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

// --- GPS error model (first-order Gauss-Markov position, white velocity) -----
// The "good" basic GPS model (Brown & Hwang): position error is dominated by slowly-varying
// iono/ephemeris terms, so it is exponentially correlated (a first-order Gauss-Markov process)
// rather than white; velocity from Doppler/carrier is far less correlated, so it is white.
// Spaceborne single-frequency SPS figures. Used by the plant to corrupt the true orbit state
// into the `Gps` measurement the controller flies on.

/// GPS position error 1-sigma per axis (m) — the Gauss-Markov steady-state.
pub const GPS_POS_SIGMA: f64 = 5.0;
/// GPS position-error correlation time (s) — the Gauss-Markov time constant.
pub const GPS_TAU: f64 = 100.0;
/// GPS velocity error 1-sigma per axis (m/s), modeled as white noise.
pub const GPS_VEL_SIGMA: f64 = 0.05;

// --- Reaction wheels ----------------------------------------------------------
// Shared by the plant (the wheels are its actuator) and the controller (an FSW
// that knows its actuator's limits clamps what it commands).

/// Wheel momentum saturation limit (N·m·s) — at the limit the motor folds back to zero net
/// torque; unloading torque always flows.
pub const RW_MOMENTUM_MAX: f64 = 0.04;
/// Maximum motor torque per wheel (N·m).
pub const RW_TORQUE_MAX: f64 = 0.002;
/// Wheel rotor moment of inertia (kg·m²): a 185 g disc of 5 cm diameter.
pub const RW_MOI: f64 = 0.185 * (0.05 / 2.0) * (0.05 / 2.0) / 2.0;
/// Coulomb bearing friction (N·m). The cube-sat's 5e-4 was telemetry-only; fed into the
/// dynamics it (with its viscous partner) would cap the wheel near 40 rad/s — 17× below the
/// ~692 rad/s saturation speed — so both coefficients are retuned to bearing-realistic values.
pub const RW_COULOMB: f64 = 1.0e-5;
/// Viscous bearing friction (N·m·s/rad).
pub const RW_VISCOUS: f64 = 1.0e-7;
/// Stiction deadband (rad/s): below this speed friction holds the wheel at rest instead of
/// chattering across zero (`signum(0)` would otherwise apply a phantom torque at rest).
pub const RW_STICTION_OMEGA: f64 = 1e-3;

// --- Magnetorquers / magnetometer ----------------------------------------------

/// Magnetometer noise 1-sigma per axis (Tesla) — the sensor reads the physical field, so its
/// noise is physical too (~150 nT against a ~25–45 µT field at 400 km).
pub const MAG_SENSOR_SIGMA: f64 = 150e-9;
/// Maximum magnetorquer dipole per axis (A·m²) — at |B| ≈ 4e-5 T that is ~8e-6 N·m of
/// authority, plenty against ~1e-7 N·m secular disturbances but far below the wheels.
pub const MTQ_MAX_DIPOLE: f64 = 0.2;

// --- Disturbance environment ----------------------------------------------------

/// Solar radiation pressure at 1 AU (N/m²).
pub const P_SRP: f64 = 4.56e-6;

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

/// The mission start epoch (UTC) the environment models are evaluated against. A **fixed**
/// constant (not wall-clock) so a `Simulated` run is reproducible — the parity test relies on
/// the static and dlopen runs computing the identical sun direction.
pub fn mission_epoch() -> Epoch {
    Epoch::from_gregorian_utc(2024, 1, 1, 0, 0, 0, 0)
}

/// The epoch `t_sim_s` seconds into the mission (the plant advances this from a deterministic
/// per-cycle counter, never wall time).
pub fn epoch_at(t_sim_s: f64) -> Epoch {
    mission_epoch() + Duration::from_seconds(t_sim_s)
}

/// The unit vector pointing at the sun in ECI at `epoch` — the real-world sun direction from
/// nox-frames' Vallado model, replacing the fixed fake sun direction.
pub fn sun_dir_eci(epoch: Epoch) -> V3 {
    nox_frames::earth::sun_vec(epoch)
}

// ---------------------------------------------------------------------------
// Environmental disturbance torques
// ---------------------------------------------------------------------------
// The body-frame torques a small spacecraft actually lives on at 400 km, evaluated at the true
// state each cycle (all deterministic — no RNG). Owned by the contract so the plant applies
// them and the sanity tests can probe them without a coordinator.

/// The disturbance torques at one true state, plus the drag force (which also perturbs the
/// orbit). All torques in the body frame (N·m); the force in ECI (N).
pub struct DisturbanceTorques {
    /// Gravity-gradient torque.
    pub gg_b: V3,
    /// Aerodynamic drag torque about the CP–CG offset.
    pub aero_b: V3,
    /// Residual-magnetic-dipole torque `m_res × B`.
    pub mag_b: V3,
    /// Solar-radiation-pressure torque about the CP–CG offset.
    pub srp_b: V3,
    /// The drag force in ECI — fed into the translational dynamics.
    pub aero_force_eci: V3,
}

impl DisturbanceTorques {
    /// The summed disturbance torque (excludes actuation — the MTQ torque is control).
    pub fn total_b(&self) -> V3 {
        self.gg_b + self.aero_b + self.mag_b + self.srp_b
    }
}

/// Evaluate the disturbance environment at a true state: attitude `q_b_eci`, ECI
/// position/velocity, the sun direction, and the **un-normalized** magnetic field (Tesla).
///
/// - Gravity gradient: `τ = (3μ/|r|³) · r̂_b × (I ∘ r̂_b)`.
/// - Aero: `F = −½ρ·Cd·A·|v|·v` (co-rotating atmosphere ignored), `τ = r_cp × F_b`.
/// - Residual dipole: `τ = m_res × B_b`.
/// - SRP: `F = −P·A·Cr·ŝ_b`, `τ = r_cp × F_b` — always lit (no eclipse model yet).
pub fn disturbance_torques(
    p: &PlantParams,
    q_b_eci: &Quat,
    pos_eci: &V3,
    vel_eci: &V3,
    sun_eci: &V3,
    mag_eci: &V3,
) -> DisturbanceTorques {
    let q_eci_b = q_b_eci.inverse();
    let cp_offset_b = V3::from_buf(p.cp_offset_b);

    let r_mag: f64 = pos_eci.norm().into_buf();
    let r_hat_b = &q_eci_b * (pos_eci.normalize());
    let gg_b = r_hat_b.cross(&(inertia_diag() * r_hat_b)) * (3.0 * MU / r_mag.powi(3));

    let v_mag: f64 = vel_eci.norm().into_buf();
    let aero_force_eci = *vel_eci * (-0.5 * p.rho * p.cd * p.area_aero * v_mag);
    let aero_b = cp_offset_b.cross(&(&q_eci_b * aero_force_eci));

    let mag_b = V3::from_buf(p.m_res_b).cross(&(&q_eci_b * *mag_eci));

    let srp_force_b = (&q_eci_b * sun_eci.normalize()) * (-P_SRP * p.area_srp * p.cr);
    let srp_b = cp_offset_b.cross(&srp_force_b);

    DisturbanceTorques { gg_b, aero_b, mag_b, srp_b, aero_force_eci }
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

/// The **GPS** measurement: the spacecraft's inertial (ECI) position + velocity, corrupted by
/// the GPS error model (a first-order Gauss-Markov position error + white velocity noise —
/// [`GPS_POS_SIGMA`]/[`GPS_TAU`]/[`GPS_VEL_SIGMA`]). This is the noisy orbit state the flight
/// software actually flies on (cube-sat's `GPS` sensor, now with noise): the controller derives
/// its pointing-law target from it, and nav evaluates its sun/magnetic references at it.
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
/// torque delivered to the body is the reaction `−ḣ` — Coulomb/viscous bearing friction and
/// the momentum-saturation foldback are inside the dynamics, not telemetry-only. A disarmed
/// wheel applies no motor torque (the `--disarmed` / safing gate) but its bearings stay real.
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

    /// The same wheel preloaded with `h` N·m·s of stored momentum along its axis — how the
    /// desaturation tests/demos start with something to dump.
    pub fn with_momentum(mut self, h: f64) -> Self {
        self.ang_momentum = self.axis * h;
        self.speed = h / RW_MOI;
        self
    }

    /// Advance the wheel one step. The motor drives against the commanded body torque
    /// (`τm = −u`, clamped to ±[`RW_TORQUE_MAX`], zero when disarmed); bearing friction
    /// opposes the spin (Coulomb + viscous, a stiction deadband at rest, and it may stop the
    /// wheel within a step but never reverse it); the momentum-saturation foldback pins
    /// `|s| ≤ RW_MOMENTUM_MAX` exactly while always letting unloading torque flow. The
    /// reaction `−ḣ·axis` is what the body receives.
    pub fn update(&mut self) {
        let s: f64 = self.axis.dot(&self.ang_momentum).into_buf();
        let omega = s / RW_MOI;

        let motor = if self.armed() {
            let u: f64 = self.axis.dot(&self.torque_set_point).into_buf();
            (-u).clamp(-RW_TORQUE_MAX, RW_TORQUE_MAX)
        } else {
            0.0
        };

        let friction = if omega.abs() < RW_STICTION_OMEGA {
            0.0
        } else {
            let f = -RW_COULOMB * omega.signum() - RW_VISCOUS * omega;
            // Friction may stop the wheel within the step, never push it through zero.
            if omega > 0.0 { f.max(-s / DT) } else { f.min(-s / DT) }
        };

        let mut h_dot = motor + friction;
        let s_next = s + h_dot * DT;
        if s_next > RW_MOMENTUM_MAX {
            h_dot = (RW_MOMENTUM_MAX - s) / DT;
        } else if s_next < -RW_MOMENTUM_MAX {
            h_dot = (-RW_MOMENTUM_MAX - s) / DT;
        }

        let s = s + h_dot * DT;
        self.ang_momentum = self.axis * s;
        self.speed = s / RW_MOI;
        self.friction = friction;
        self.torque = self.axis * -h_dot;
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
// read the same params off `mission.kdl` (the parity test's static path).

/// Plant parameters: the initial attitude/rate offset, the sensor-noise sigma, the RNG seed
/// (so a run is reproducible), whether the reaction wheels boot disarmed, and the disturbance
/// environment. The disturbance defaults are physically honest for a ~3 kg spacecraft at
/// 400 km (secular torques ~1e-7 N·m — days to load a wheel); tests and demos crank them so
/// momentum management is visible in seconds.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq)]
pub struct PlantParams {
    /// Initial attitude offset from the target, radians about the [1,1,1] axis.
    pub init_angle: f64,
    /// Initial body-rate magnitude, rad/s about [1,1,1].
    pub init_rate: f64,
    /// 1-sigma sensor noise (rad/s for gyro, unitless for the normalized sun vector; the
    /// magnetometer's Tesla-valued noise is [`MAG_SENSOR_SIGMA`]).
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
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq)]
pub struct NavParams {
    /// MEKF measurement 1-sigma for the two vector observations.
    pub meas_sigma: f64,
}

/// Controller (Yang-LQR + magnetorquer laws) parameters.
#[derive(Serialize, Deserialize, Schema, Clone, Debug, PartialEq)]
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
            let b = mag_field_eci(&mut model, mission_epoch(), &pos);
            let mag = b.norm().into_buf();
            assert!(
                (1.0e-5..6.0e-5).contains(&mag),
                "WMM field magnitude at 400 km should be tens of µT, got {mag} T"
            );
        }
    }

    /// Driven at the full set point, the wheel pins **exactly** at `RW_MOMENTUM_MAX` (the
    /// foldback lands on the limit rather than snapping torque to zero early), never exceeds
    /// it, and an unloading command flows immediately from the pinned state.
    #[test]
    fn rw_saturation_clamps_at_limit_and_allows_unloading() {
        let mut w = ReactionWheel::new(tensor![1.0, 0.0, 0.0], true);
        // A constant −X body-torque command spins the wheel up along +X.
        w.torque_set_point = tensor![-RW_TORQUE_MAX, 0.0, 0.0];
        let mut pinned = 0;
        for _ in 0..5000 {
            w.update();
            let s: f64 = w.ang_momentum.into_buf()[0];
            assert!(s <= RW_MOMENTUM_MAX + 1e-12, "momentum exceeded the limit: {s}");
            if s >= RW_MOMENTUM_MAX - 1e-12 {
                pinned += 1;
            }
        }
        assert!(pinned > 0, "wheel never reached the momentum limit");
        let t: f64 = w.torque.into_buf()[0];
        assert!(t.abs() < 1e-12, "pinned wheel still delivered torque: {t}");
        // Reverse the command: unloading torque must flow on the very next step.
        w.torque_set_point = tensor![RW_TORQUE_MAX, 0.0, 0.0];
        w.update();
        let t: f64 = w.torque.into_buf()[0];
        assert!(t > 1e-3, "unloading torque did not flow from saturation: {t}");
        let s: f64 = w.ang_momentum.into_buf()[0];
        assert!(s < RW_MOMENTUM_MAX, "momentum did not unload");
    }

    /// With no motor command, bearing friction decays a preloaded wheel monotonically, stops
    /// it without ever pushing it through zero into counter-rotation, and holds it frozen
    /// inside the stiction deadband (regression for the unsigned-speed `signum` friction and
    /// the phantom torque at rest).
    #[test]
    fn rw_friction_decays_signed_speed() {
        let mut w = ReactionWheel::new(tensor![0.0, 1.0, 0.0], true).with_momentum(0.001);
        let mut prev = w.speed;
        assert!(prev > 0.0);
        let mut frozen = 0;
        for _ in 0..30_000 {
            w.update();
            assert!(w.speed <= prev + 1e-15, "friction sped the wheel up");
            assert!(w.speed >= 0.0, "friction counter-rotated the wheel: {}", w.speed);
            if w.speed == prev {
                frozen += 1;
            } else {
                frozen = 0;
            }
            prev = w.speed;
        }
        assert!(frozen > 100, "wheel never froze");
        assert!(prev < RW_STICTION_OMEGA, "froze outside the deadband: {prev}");
    }

    /// The disturbance model lands each source in its expected order-of-magnitude band at a
    /// 400 km state with honest params — a units guard (ρ, Tesla, P_SRP, torque arms), like
    /// the WMM band test above.
    #[test]
    fn disturbance_torques_sane_at_400km() {
        let p = PlantParams {
            init_angle: 0.0,
            init_rate: 0.0,
            meas_sigma: 0.0,
            seed: 0,
            disarmed: false,
            rho: default_rho(),
            cd: default_cd(),
            area_aero: default_area(),
            cp_offset_b: default_cp_offset(),
            m_res_b: default_m_res(),
            area_srp: default_area(),
            cr: default_cr(),
            mtq_max_dipole: default_mtq_max(),
            init_wheel_h: 0.0,
        };
        let radius = EARTH_RADIUS + ALTITUDE;
        let pos: V3 = tensor![radius, 0.0, 0.0];
        let v_orbit = (MU / radius).sqrt();
        let vel: V3 = tensor![0.0, v_orbit, 0.0];
        // A generic attitude so nothing sits on a principal axis or parallel to the field.
        let q = Quat::from_axis_angle(tensor![1.0, 1.0, 1.0], 0.5);
        let epoch = mission_epoch();
        let mut model = MagneticModel::default();
        let mag = mag_field_eci(&mut model, epoch, &pos);
        let d = disturbance_torques(&p, &q, &pos, &vel, &sun_dir_eci(epoch), &mag);

        let mag_of = |v: &V3| -> f64 { v.norm().into_buf() };
        let gg = mag_of(&d.gg_b);
        assert!((1e-10..1e-6).contains(&gg), "gravity-gradient torque out of band: {gg}");
        let aero = mag_of(&d.aero_b);
        assert!((1e-9..1e-5).contains(&aero), "aero torque out of band: {aero}");
        let res = mag_of(&d.mag_b);
        assert!((1e-10..1e-5).contains(&res), "residual-dipole torque out of band: {res}");
        let srp = mag_of(&d.srp_b);
        assert!((1e-11..1e-7).contains(&srp), "SRP torque out of band: {srp}");
        // Drag opposes the velocity.
        let drag_along_v: f64 = d.aero_force_eci.dot(&vel).into_buf();
        assert!(drag_along_v < 0.0, "drag force does not oppose velocity");
        let sum = d.gg_b + d.aero_b + d.mag_b + d.srp_b;
        assert!(mag_of(&(d.total_b() - sum)) == 0.0);
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
