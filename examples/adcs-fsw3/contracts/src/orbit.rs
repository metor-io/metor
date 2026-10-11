//! The circular orbit the plant boots onto.

use nox::tensor;

use crate::{Epoch, MU, V3, sun_dir_eci};

/// Default orbit altitude (m).
pub const ALTITUDE: f64 = 400.0e3;
/// Default orbit inclination (rad), sun-synchronous at 400 km.
pub const SSO_INCLINATION: f64 = 97.03 * core::f64::consts::PI / 180.0;

/// Returns the RAAN (rad) that puts the ascending node at local mean solar time
/// `ltan_hours` at `epoch`.
pub fn raan_for_ltan(epoch: Epoch, ltan_hours: f64) -> f64 {
    let [x, y, _] = sun_dir_eci(epoch).into_buf();
    y.atan2(x) + (ltan_hours - 12.0) / 24.0 * core::f64::consts::TAU
}

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
