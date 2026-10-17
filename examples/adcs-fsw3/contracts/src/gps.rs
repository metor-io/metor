//! The GPS position error that the plant and the receiver model both add.
//!
//! Ionospheric and ephemeris errors vary slowly, so the error follows a
//! first-order Gauss-Markov process.

use crate::V3;

/// Steady-state GPS position error 1σ per axis (m).
pub const GPS_POS_SIGMA: f64 = 5.0;
/// The time constant of the GPS position error (s).
pub const GPS_POS_ERR_TIME_CONSTANT: f64 = 100.0;
/// GPS velocity error 1σ per axis (m/s).
pub const GPS_VEL_SIGMA: f64 = 0.05;

/// A GPS position error that drifts toward a steady-state deviation.
#[derive(Clone, Copy, Debug)]
pub struct GpsError {
    err: V3,
    /// The steady-state 1σ per axis (m).
    sigma: f64,
}

impl GpsError {
    /// A zero error that settles at `sigma` per axis.
    pub fn new(sigma: f64) -> Self {
        Self {
            err: V3::zeros(),
            sigma,
        }
    }

    /// Steps the error `dt` seconds, driven by `unit`, three unit-normal
    /// draws, and returns it.
    pub fn step(&mut self, dt: f64, unit: V3) -> V3 {
        let phi = (-dt / GPS_POS_ERR_TIME_CONSTANT).exp();
        self.err = phi * self.err + unit * (self.sigma * (1.0 - phi * phi).sqrt());
        self.err
    }
}
