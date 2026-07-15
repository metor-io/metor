use core::ops::{Add, Mul};

use zerocopy::{Immutable, IntoBytes, KnownLayout};

use crate::{
    Scalar, SpatialInertia, SpatialTransform,
    array::{SpatialForce, SpatialMotion},
    rk4,
};

#[derive(Debug, Clone, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
pub struct Body {
    pub pos: SpatialTransform<f64>,
    pub vel: SpatialMotion<f64>,
    pub accel: SpatialMotion<f64>,
    pub inertia: SpatialInertia<f64>,
    pub force: SpatialForce<f64>,
}

pub struct DU {
    vel: SpatialMotion<f64>,
    accel: SpatialMotion<f64>,
    force: SpatialForce<f64>,
}

impl DU {
    /// The state derivative of `body` under a **world-frame** `force` (both the linear force
    /// and the torque), for a body whose `inertia_diag` is expressed along its principal axes.
    ///
    /// The rotational dynamics are the full Euler equations: the torque and the world-frame
    /// angular velocity are brought into the body frame, the gyroscopic term is subtracted
    /// (`I ω̇_b = τ_b − ω_b × I ω_b`), and the resulting acceleration is rotated back into the
    /// world frame (`ω_w = R ω_b ⇒ ω̇_w = R ω̇_b`, since `Ṙ ω_b = ω_w × ω_w = 0`).
    pub fn from_body_force(body: &Body, force: SpatialForce<f64>) -> Self {
        let q = body.pos.angular();
        let q_inv = q.inverse();
        let inertia = body.inertia.inertia_diag();
        let omega_b = &q_inv * body.vel.angular();
        let torque_b = &q_inv * force.torque();
        let ang_accel_b = (torque_b - omega_b.cross(&(inertia * omega_b))) / inertia;
        let accel = SpatialMotion::new(q * ang_accel_b, force.force() / body.inertia.mass());
        DU {
            vel: body.vel.clone(),
            accel,
            force,
        }
    }
}

impl Add<DU> for &'_ Body {
    type Output = Body;

    fn add(self, du: DU) -> Body {
        Body {
            pos: self.pos.clone() + du.vel,
            vel: self.vel.clone() + du.accel.clone(),
            accel: du.accel,
            inertia: self.inertia.clone(),
            force: du.force.clone(),
        }
    }
}

impl Add<DU> for DU {
    type Output = DU;

    fn add(self, du: DU) -> DU {
        DU {
            vel: self.vel + du.vel,
            accel: self.accel + du.accel,
            force: self.force,
        }
    }
}

impl Mul<&'_ DU> for Scalar<f64> {
    type Output = DU;

    fn mul(self, du: &DU) -> DU {
        DU {
            vel: du.vel.clone() * &self,
            accel: du.accel.clone() * &self,
            force: du.force.clone(),
        }
    }
}

pub fn six_dof_rk4(dt: f64, body: Body, effector: impl Fn(&Body) -> SpatialForce<f64>) -> Body {
    rk4::<f64, Body, DU, _>(dt, &body, |body: &Body| -> DU {
        let force = effector(body);
        DU::from_body_force(body, force)
    })
}

#[cfg(test)]
mod tests {
    use crate::{Quaternion, SpatialForce, tensor};

    use super::*;

    #[test]
    fn test_hookes_law() {
        let mut body = Body {
            pos: SpatialTransform::new(Quaternion::identity(), tensor![1.0, 0.0, 0.0]),
            vel: SpatialMotion::zero(),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::from_mass(1.0),
            force: SpatialForce::zero(),
        };
        let k = 1.0;
        for _ in 0..10 {
            body = six_dof_rk4(0.1, body, |body| {
                let force = body.pos.linear() * -k;
                SpatialForce::from_linear(force)
            });
        }
        assert_eq!(body.pos.linear(), tensor![0.540302967116884, 0.0, 0.0]);
    }

    /// Torque-free tumbling of an anisotropic body off its principal axes: the world-frame
    /// angular momentum `L_w = q ⊛ (I ∘ ω_b)` and the rotational kinetic energy
    /// `½ ω_b · (I ∘ ω_b)` are both invariants of the Euler equations. The pre-fix dynamics
    /// (`ω̇ = τ/I`, no gyroscopic term) hold ω constant in the world frame instead, which
    /// drifts both — this pins the fix.
    #[test]
    fn torque_free_tumble_conserves_momentum_and_energy() {
        let inertia = tensor![3.0, 2.0, 1.0];
        let mut body = Body {
            pos: SpatialTransform::new(Quaternion::identity(), tensor![0.0, 0.0, 0.0]),
            vel: SpatialMotion::new(tensor![0.3, 0.2, 0.5], tensor![0.0, 0.0, 0.0]),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::new(inertia, tensor![0.0, 0.0, 0.0], 1.0),
            force: SpatialForce::zero(),
        };
        let invariants = |body: &Body| {
            let q = body.pos.angular();
            let omega_b = q.inverse() * body.vel.angular();
            let l_b = body.inertia.inertia_diag() * omega_b;
            let ke: f64 = (omega_b.dot(&l_b) * 0.5).into_buf();
            let l_w = q * l_b;
            (l_w, ke)
        };
        let (l0, ke0) = invariants(&body);
        let mut omega_w_max_delta = 0.0f64;
        for _ in 0..2000 {
            body = six_dof_rk4(0.01, body, |_| SpatialForce::zero());
            let d: f64 = (body.vel.angular() - tensor![0.3, 0.2, 0.5]).norm().into_buf();
            omega_w_max_delta = omega_w_max_delta.max(d);
        }
        // Tolerance is set by the integrator, not the dynamics: the `SpatialTransform +
        // SpatialMotion` quaternion update is first-order inside the RK4 combination, so the
        // invariants drift at ~1e-7 relative over this run. The pre-fix dynamics drift at O(1).
        let (l, ke) = invariants(&body);
        let l_err: f64 = (l - l0.clone()).norm().into_buf();
        let l_mag: f64 = l0.norm().into_buf();
        assert!(l_err / l_mag < 1e-5, "L_w drifted: {}", l_err / l_mag);
        assert!((ke - ke0).abs() / ke0 < 1e-5, "KE drifted: {}", (ke - ke0).abs() / ke0);
        // The body genuinely precesses (ω is NOT constant in the world frame) — guards against
        // reintroducing the no-coupling dynamics, for which both invariants trivially hold at
        // zero torque.
        assert!(omega_w_max_delta > 0.05, "no precession: {omega_w_max_delta}");
    }

    #[test]
    fn test_gravity() {
        let mut body = Body {
            pos: SpatialTransform::new(Quaternion::identity(), tensor![0.0, 0.0, 0.0]),
            vel: SpatialMotion::zero(),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::from_mass(1.0),
            force: SpatialForce::zero(),
        };
        let g = 9.81;
        for _ in 0..20 {
            body = six_dof_rk4(0.1, body, |_| {
                let force = tensor![0.0, 0.0, -g];
                SpatialForce::from_linear(force)
            });
        }
        // ½gt² at t = 2 s — RK4 is exact for constant acceleration.
        assert_eq!(body.pos.linear(), tensor![0.0, 0.0, -19.62]);
    }
}
