//! Earth's constants and the circular orbit the plant boots onto.

use nox::tensor;

use crate::V3;

/// Gravitational constant (m³·kg⁻¹·s⁻²).
pub const G: f64 = 6.6743e-11;
/// Mass of Earth (kg).
pub const M: f64 = 5.972e24;
/// Earth's gravitational parameter `G·M` (m³·s⁻²).
pub const MU: f64 = G * M;
/// Earth radius (m).
pub const EARTH_RADIUS: f64 = 6378.1e3;

/// Default orbit altitude (m).
pub const ALTITUDE: f64 = 400.0e3;
/// Default orbit inclination (rad), sun-synchronous at 400 km.
pub const SSO_INCLINATION: f64 = 97.03 * core::f64::consts::PI / 180.0;

/// Returns the ECI position and velocity of a circular orbit at phase `theta`
/// (rad) past its ascending node.
pub fn orbit_pos_vel(radius: f64, inclination: f64, raan: f64, theta: f64) -> (V3, V3) {
    let (sin_o, cos_o) = raan.sin_cos();
    let (sin_i, cos_i) = inclination.sin_cos();
    // The node line and the in-plane direction 90° ahead of it.
    let node: V3 = tensor![cos_o, sin_o, 0.0];
    let climb: V3 = tensor![-sin_o * cos_i, cos_o * cos_i, sin_i];
    let (sin_th, cos_th) = theta.sin_cos();
    let v_orbit = (MU / radius).sqrt();
    (
        (node * cos_th + climb * sin_th) * radius,
        (climb * cos_th - node * sin_th) * v_orbit,
    )
}
