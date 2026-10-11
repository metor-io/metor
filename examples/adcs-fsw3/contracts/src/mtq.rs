//! Magnetorquer laws, which turn a measured field into a commanded dipole.

use crate::V3;

/// Returns `k·(x × B)/|B|²`, or zero when the field vanishes.
///
/// The resulting torque `m × B` is `−k·x⊥`, which opposes the part of `x`
/// perpendicular to the field. The parallel part stays out of reach until the
/// orbit turns the field.
fn cross_dipole(k: f64, x_b: &V3, mag_b: &V3) -> V3 {
    let b2: f64 = mag_b.dot(mag_b).into_buf();
    if b2 < 1e-18 {
        return V3::zeros();
    }
    x_b.cross(mag_b) * (k / b2)
}

/// Returns the dipole that unloads the wheel momentum `h_wheels_b`.
pub fn desat_dipole(k_desat: f64, h_wheels_b: &V3, mag_b: &V3) -> V3 {
    cross_dipole(k_desat, h_wheels_b, mag_b)
}

/// Returns the B-cross dipole that damps the body rate `omega_b`.
///
/// Classic B-dot gives the same torque from the field's derivative, which is
/// mostly noise at 120 Hz. The gyro rate is clean.
pub fn detumble_dipole(k_detumble: f64, omega_b: &V3, mag_b: &V3) -> V3 {
    cross_dipole(k_detumble, omega_b, mag_b)
}

/// Clamps each axis of `m` to ±`max`.
pub fn clamp_dipole(m: V3, max: f64) -> V3 {
    V3::from_buf(m.into_buf().map(|x| x.clamp(-max, max)))
}

#[cfg(test)]
mod tests {
    use nox::tensor;

    use super::*;

    /// Both magnetorquer laws make torque that opposes their input, make none
    /// along the field, and return zero when the field vanishes.
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
            assert!(
                along < 0.0,
                "desat torque does not oppose momentum: {along}"
            );
            let tau = detumble_dipole(0.1, &x, &b).cross(&b);
            let along: f64 = tau.dot(&x).into_buf();
            assert!(along < 0.0, "detumble torque does not oppose rate: {along}");
        }
        let parallel = b * 2.0e3;
        let tau = desat_dipole(0.1, &parallel, &b).cross(&b);
        let t: f64 = tau.norm().into_buf();
        assert!(t < 1e-12, "cross law claimed authority along B: {t}");
        let zero: f64 = desat_dipole(0.1, &parallel, &V3::zeros()).norm().into_buf();
        assert_eq!(zero, 0.0);
    }
}
