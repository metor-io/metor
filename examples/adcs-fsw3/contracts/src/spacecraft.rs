//! Mass properties and actuator and sensor limits of the spacecraft.

use nox::tensor;

use crate::V3;

/// Spacecraft mass (kg).
pub const MASS: f64 = 2825.2 / 1000.0;

/// Returns the spacecraft's principal moments of inertia (kg·m²).
pub fn inertia_diag() -> V3 {
    tensor![15204079.70002e-9, 14621352.61765e-9, 6237758.3131e-9]
}

/// Stored momentum limit per wheel (N·m·s). A wheel at the limit accepts only
/// torque that unloads it.
pub const RW_MOMENTUM_MAX: f64 = 0.04;
/// Motor torque limit per wheel (N·m).
pub const RW_TORQUE_MAX: f64 = 0.002;
/// Magnetorquer dipole limit per axis (A·m²). At |B| ≈ 4e-5 T this gives about
/// 8e-6 N·m, far above the secular disturbances and far below the wheels.
pub const MTQ_MAX_DIPOLE: f64 = 0.2;

/// Sun sensor reading below which a head counts as dark. With every head dark,
/// the sun is lost.
///
/// While the sun is visible the brightest of the six heads reads at least
/// 1/√3 ≈ 0.577, so 0.1 sits about 50σ of head noise from both bands.
pub const CSS_THRESHOLD: f64 = 0.1;
