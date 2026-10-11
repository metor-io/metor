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
