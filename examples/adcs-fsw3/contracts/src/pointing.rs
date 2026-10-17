//! Pointing laws, which give the controller its target attitude, and the
//! samples that score how well the spacecraft tracks it.

use crate::{BodyState, ModeCmd, Quat, V3};

/// The body axis both pointing laws aim.
const POINTING_AXIS: [f64; 3] = [0.0, -1.0, 0.0];

/// Bound on `1 + cos θ` below which the target is opposite the body axis.
const ANTIPARALLEL: f64 = 1e-9;

/// Returns the shortest rotation that aims body `-Y` along `dir_eci`.
///
/// A target opposite the axis has no unique shortest rotation, so it gets a
/// half turn about body `X`.
fn point_minus_y_at(dir_eci: V3) -> Quat {
    let r = dir_eci.normalize();
    let body_axis: V3 = V3::from_buf(POINTING_AXIS);
    let [x, y, z] = body_axis.cross(&r).into_buf();
    let w = 1.0 + body_axis.dot(&r).into_buf();
    if w.abs() < ANTIPARALLEL {
        return Quat::new(0.0, 1.0, 0.0, 0.0);
    }
    Quat::new(w, x, y, z).normalize()
}

/// Returns the attitude that aims body `-Y` at Earth's center.
pub fn nadir_point(pos_eci: &V3) -> Quat {
    point_minus_y_at(pos_eci.normalize() * -1.0)
}

/// Returns the attitude that aims body `-Y` along the orbital velocity.
pub fn hil_point(vel_eci: &V3) -> Quat {
    point_minus_y_at(vel_eci.normalize())
}

/// Returns the target attitude for `law` at an orbit state, or identity when the
/// law is unknown or the target is not finite.
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

impl BodyState {
    /// Returns the attitude error to `target` (rad) and the body rate (rad/s).
    pub fn tracking_error(&self, target: &Quat) -> (f64, f64) {
        let err = self.q_b_eci.angular_distance(target).into_buf().abs();
        let rate = self.omega_b.norm().into_buf();
        (err, rate)
    }
}

#[cfg(test)]
mod tests {
    use nox::tensor;

    use super::*;
    use crate::{ALTITUDE, EARTH_RADIUS};

    /// Nadir pointing aims body `-Y` along `-pos`, and velocity pointing aims it
    /// along `vel`.
    #[test]
    fn pointing_laws_aim_the_body_axis() {
        let axis: V3 = V3::from_buf(POINTING_AXIS);
        let radius = EARTH_RADIUS + ALTITUDE;
        for (pos, vel) in [
            (
                tensor![radius, 0.0, 0.0] as V3,
                tensor![0.0, 7.6e3, 0.0] as V3,
            ),
            (
                tensor![0.0, radius, 0.0] as V3,
                tensor![0.0, 0.0, 7.6e3] as V3,
            ),
            (
                (tensor![1.0, 1.0, 1.0] as V3).normalize() * radius,
                tensor![0.0, -7.6e3, 0.0] as V3,
            ),
        ] {
            let aimed = nadir_point(&pos) * axis;
            let want = pos.normalize() * -1.0;
            let err: f64 = (aimed - want).norm().into_buf();
            assert!(
                err < 1e-12,
                "nadir aims {:?}, want {:?}",
                aimed.into_buf(),
                want.into_buf()
            );

            let aimed = hil_point(&vel) * axis;
            let want = vel.normalize();
            let err: f64 = (aimed - want).norm().into_buf();
            assert!(
                err < 1e-12,
                "hil aims {:?}, want {:?}",
                aimed.into_buf(),
                want.into_buf()
            );
        }
    }

    /// A target opposite body `-Y` yields a unit half turn rather than NaN.
    #[test]
    fn antiparallel_target_is_a_finite_half_turn() {
        let axis: V3 = V3::from_buf(POINTING_AXIS);
        // Nadir from a position along `-Y` lies along `+Y`.
        let q = nadir_point(&(tensor![0.0, -7.0e6, 0.0] as V3));
        assert!(
            q.0.into_buf().iter().all(|f| f.is_finite()),
            "a degenerate target must stay finite"
        );
        let norm: f64 = q.0.norm().into_buf();
        assert!((norm - 1.0).abs() < 1e-12, "not a unit quaternion: {norm}");
        let aimed = q * axis;
        let want: V3 = tensor![0.0, 1.0, 0.0];
        let err: f64 = (aimed - want).norm().into_buf();
        assert!(err < 1e-12, "half turn aims {:?}", aimed.into_buf());
    }
}
