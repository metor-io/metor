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
/// the same struct. It integrates its stored angular momentum under the commanded set point,
/// with Stribeck/Coulomb/viscous friction and a momentum-saturation clamp; a disarmed wheel
/// applies no torque (the `--disarmed` / safing gate). Ported from cube-sat `ReactionWheel`.
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
    /// Stored angular momentum about the spin axis (N·m·s).
    pub ang_momentum: V3,
    /// The commanded torque set point (the projection of the control torque onto `axis`).
    pub torque_set_point: V3,
    /// The torque actually applied this step (after saturation/clamp), N·m.
    pub torque: V3,
    /// Wheel speed (rad/s).
    pub speed: f64,
    /// Modeled friction torque (N·m) — telemetered, not fed into the dynamics (cube-sat parity).
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

    fn moment_of_inertia(&self) -> f64 {
        0.185 * (0.05 / 2.0_f64).powi(2) / 2.0
    }

    fn update_speed(&mut self) {
        let i = self.moment_of_inertia();
        let momentum_norm: f64 = self.ang_momentum.norm().into_buf();
        self.speed = momentum_norm / i;
    }

    /// Friction torque (the cube-sat `rw_drag`): Stribeck near zero speed, Coulomb + viscous
    /// otherwise.
    fn friction_torque(&self) -> f64 {
        let static_fric = 0.0005;
        let columb_fric = 0.0005;
        let stribeck_coef = 0.0005;
        let cv = 0.00005;
        let omega_limit = 0.1;
        let speed = self.speed;

        let stribeck_torque = -(2.0 * std::f64::consts::E).sqrt()
            * (static_fric - columb_fric)
            * (-((speed / stribeck_coef).powi(2))).exp()
            - columb_fric * (10.0 * speed / stribeck_coef).tanh()
            - cv * speed;

        let torque_norm: f64 = self.torque_set_point.norm().into_buf();
        let use_stribeck =
            speed.abs() < 0.01 * omega_limit && speed.signum() == torque_norm.signum();

        if use_stribeck {
            stribeck_torque
        } else {
            -columb_fric * speed.signum() - cv * speed
        }
    }

    /// Advance the wheel one step: integrate momentum under the set point (gated by arm and
    /// by the 0.04 momentum-saturation limit), clamp the applied torque, and update speed.
    pub fn update(&mut self) {
        if !self.armed() {
            self.torque = V3::zeros();
            self.friction = self.friction_torque();
            self.update_speed();
            return;
        }

        let rw_force_clamp = 0.002;

        let new_ang_momentum = self.ang_momentum + self.torque_set_point * DT;
        let new_momentum_norm: f64 = new_ang_momentum.norm().into_buf();
        let torque = if new_momentum_norm < 0.04 {
            self.torque_set_point
        } else {
            V3::zeros()
        };

        let clamped_torque =
            V3::from_buf(torque.into_buf().map(|t| t.clamp(-rw_force_clamp, rw_force_clamp)));

        self.ang_momentum = self.ang_momentum + clamped_torque * DT;
        self.friction = self.friction_torque();
        self.torque = clamped_torque;
        self.update_speed();
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
/// (so a run is reproducible), and whether the reaction wheels boot disarmed.
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
    /// Bring the spacecraft up with every reaction wheel offline (the `--disarmed` parity):
    /// no control torque is applied until the wheels are armed. Defaults to `false`.
    #[serde(default)]
    pub disarmed: bool,
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
