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
    pub fn from_body_force(body: &Body, force: SpatialForce<f64>) -> Self {
        DU {
            vel: body.vel.clone(),
            accel: force.clone() / body.inertia.clone(),
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
        assert_eq!(body.pos.linear(), tensor![0.0, 0.0, -19.6]);
    }
}
