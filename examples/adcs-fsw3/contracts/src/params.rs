//! The configuration each system's constructor takes.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ALTITUDE, MTQ_MAX_DIPOLE, SSO_INCLINATION};

/// Configuration for the plant.
///
/// The disturbance defaults match a 3 kg spacecraft at 400 km, where the
/// torques take days to fill a wheel. Raise them to see momentum management
/// within a short run.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct PlantParams {
    /// Initial attitude offset about `[1, 1, 1]` (rad).
    pub init_angle: f64,
    /// Initial body rate about `[1, 1, 1]` (rad/s).
    pub init_rate: f64,
    /// Gyro noise 1σ (rad/s), also used for each sun sensor head.
    pub meas_sigma: f64,
    /// Seed for the sensor noise.
    pub seed: u64,
    /// Boots with every reaction wheel disarmed.
    #[serde(default)]
    pub disarmed: bool,
    /// Atmospheric density (kg/m³).
    #[serde(default = "default_rho")]
    pub rho: f64,
    /// Drag coefficient.
    #[serde(default = "default_cd")]
    pub cd: f64,
    /// Drag reference area (m²).
    #[serde(default = "default_area")]
    pub area_aero: f64,
    /// Center of pressure relative to the center of mass (m), the lever arm for
    /// drag and solar radiation pressure.
    #[serde(default = "default_cp_offset")]
    pub cp_offset_b: [f64; 3],
    /// Residual magnetic dipole (A·m²).
    #[serde(default = "default_m_res")]
    pub m_res_b: [f64; 3],
    /// Solar radiation pressure reference area (m²).
    #[serde(default = "default_area")]
    pub area_srp: f64,
    /// Reflectivity coefficient, from 1 for an absorber to 2 for a mirror.
    #[serde(default = "default_cr")]
    pub cr: f64,
    /// Magnetorquer dipole limit per axis (A·m²).
    #[serde(default = "default_mtq_max")]
    pub mtq_max_dipole: f64,
    /// Initial stored momentum per wheel (N·m·s).
    #[serde(default)]
    pub init_wheel_h: f64,
    /// Orbit altitude (m).
    #[serde(default = "default_altitude")]
    pub altitude: f64,
    /// Orbit inclination (rad).
    #[serde(default = "default_inclination")]
    pub inclination: f64,
    /// Local mean solar time of the ascending node at boot (hours). Gravity is a
    /// point mass, so the node stays fixed in inertial space and its local time
    /// drifts over days.
    #[serde(default = "default_ltan")]
    pub ltan_hours: f64,
    /// Initial orbit phase past the ascending node (rad).
    #[serde(default)]
    pub init_orbit_phase: f64,
}

fn default_altitude() -> f64 {
    ALTITUDE
}
fn default_inclination() -> f64 {
    SSO_INCLINATION
}
fn default_ltan() -> f64 {
    10.5
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

/// Configuration for the navigation filter.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct NavParams {
    /// Measurement 1σ for the sun and magnetic field observations.
    pub meas_sigma: f64,
    /// Squared Mahalanobis distance above which a star vector is rejected.
    /// 13.8 is χ²₂ at p = 0.001.
    #[serde(default = "default_star_gate")]
    pub star_gate: f64,
    /// Fuses the sun and magnetometer vectors. Off leaves star vectors alone.
    #[serde(default = "default_true")]
    pub sun_mag: bool,
    /// Gyro white noise the filter models (rad/s/√Hz). The default is twice
    /// the plant's 2 mrad/s per sample at 120 Hz, as margin for an
    /// unmodelled gyro.
    #[serde(default = "default_gyro_noise")]
    pub gyro_noise: f64,
    /// Gyro bias random walk the filter models (rad/s/√s). The default is
    /// twice the plant's 20 µrad/s per sample at 120 Hz.
    #[serde(default = "default_bias_walk")]
    pub bias_walk: f64,
    /// Initial gyro bias 1σ (rad/s).
    #[serde(default = "default_bias_sigma")]
    pub bias_sigma: f64,
}

fn default_gyro_noise() -> f64 {
    3.65e-4
}
fn default_bias_walk() -> f64 {
    4.38e-4
}
fn default_bias_sigma() -> f64 {
    1e-3
}

fn default_star_gate() -> f64 {
    13.8
}
fn default_true() -> bool {
    true
}

/// Configuration for the image star tracker. The camera defaults match the
/// star simulator's: 16 mm, 5.5 µm pixels, 1024², about 20°.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct StarTrackerParams {
    #[serde(default = "default_f_m")]
    pub f_m: f64,
    #[serde(default = "default_pitch_m")]
    pub pitch_m: f64,
    /// Principal point (px).
    #[serde(default = "default_centre_px")]
    pub cx: f64,
    #[serde(default = "default_centre_px")]
    pub cy: f64,
    /// Brown–Conrady radial (1/m², 1/m⁴) and tangential (1/m) terms.
    #[serde(default)]
    pub k1: f64,
    #[serde(default)]
    pub k2: f64,
    #[serde(default)]
    pub p1: f64,
    #[serde(default)]
    pub p2: f64,
    /// Mount, `[x, y, z, w]`: `b_body = A(q_b_st) b_st`.
    #[serde(default = "default_q_b_st")]
    pub q_b_st: [f64; 4],
    /// A faint star's centroid 1σ (px), the most any star gets; it sizes
    /// the tracking gate and lost-in-space's residual test.
    #[serde(default = "default_sigma_px")]
    pub sigma_px: f64,
    /// Shot-noise term (px·√flux): a centroid of flux `F` has 1σ
    /// `sigma_flux_px / √F`, between `sigma_floor_px` and `sigma_px`.
    #[serde(default = "default_sigma_flux_px")]
    pub sigma_flux_px: f64,
    /// The least centroid 1σ (px), for bright stars.
    #[serde(default = "default_sigma_floor_px")]
    pub sigma_floor_px: f64,
    /// Lost-in-space triplets per cycle.
    #[serde(default = "default_budget")]
    pub budget: u32,
}

fn default_f_m() -> f64 {
    16e-3
}

fn default_pitch_m() -> f64 {
    5.5e-6
}

fn default_centre_px() -> f64 {
    512.0
}

fn default_sigma_px() -> f64 {
    0.1
}

// metor-star's `SIGMA_FLUX_PX` and `SIGMA_FLOOR_PX`, calibrated on the
// simulator.
fn default_sigma_flux_px() -> f64 {
    1.2
}

fn default_sigma_floor_px() -> f64 {
    0.02
}

fn default_budget() -> u32 {
    1000
}

fn default_q_b_st() -> [f64; 4] {
    [0.0, 0.0, 0.0, 1.0]
}

/// Configuration for the controller.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct CtrlParams {
    /// LQR state weight.
    #[serde(default = "default_q_weight")]
    pub q_weight: f64,
    /// LQR control weight.
    #[serde(default = "default_r_weight")]
    pub r_weight: f64,
    /// Momentum unloading gain (1/s), zero to disable. At |B| ≈ 4e-5 T the
    /// default unloads with a time constant near 2000 s.
    #[serde(default = "default_k_desat")]
    pub k_desat: f64,
    /// Detumble gain (N·m·s/rad). The default saturates the magnetorquers near
    /// 0.16 rad/s and damps with a time constant near 300 s.
    #[serde(default = "default_k_detumble")]
    pub k_detumble: f64,
}

fn default_q_weight() -> f64 {
    5.0
}
fn default_r_weight() -> f64 {
    8.0
}
fn default_k_desat() -> f64 {
    5e-4
}
fn default_k_detumble() -> f64 {
    5e-5
}
