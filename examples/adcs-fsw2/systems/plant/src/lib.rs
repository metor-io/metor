//! The rigid-body **plant** of the `adcs-fsw2` mission, as a `dlopen`-loadable `cdylib`
//! (dl-open.md §3, §8). The `impl CyclicSystem` is **unchanged** from the monolith — only
//! the frame/param contracts moved out (to `adcs-contracts`) and a one-line
//! [`export_system!`](metor_fsw_2::export_system) was added.
//!
//! Each cycle it integrates one `rk4` step of rotational dynamics under the latest
//! commanded torque and emits simulated gyro + sun-sensor + magnetometer measurements,
//! plus a `truth` frame (ground-truth attitude + body rate) the host taps to measure
//! convergence.

// The `export_system!`-generated `extern "C" fn fsw_*` exports take raw pointers by ABI
// contract (the host owns their validity, dl-open.md §2.5); clippy's
// `not_unsafe_ptr_arg_deref` is inherent to that macro surface for any cdylib.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use adcs_contracts::{DT, PlantParams, Sensors, TorqueCmd, Truth, V3, inertia_diag};
use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::ring::{Backing, BoxBacking};
use metor_fsw_2::{
    BuildSystem, CyclicSystem, Input, Out, Output, System, SystemInput, SystemOutput,
};
use nox::{
    Body, Quaternion, SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform, six_dof_rk4,
    tensor,
};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, Normal};

/// The rigid-body plant: integrates rotational dynamics under the commanded torque
/// and emits noisy sensor measurements.
pub struct PlantSystem {
    body: Body,
    sun_ref: V3,
    mag_ref: V3,
    bias: V3,
    meas_sigma: f64,
    rng: StdRng,
}

#[derive(SystemInput)]
pub struct PlantIn<B: Backing = BoxBacking> {
    pub torque: Input<TorqueCmd, B>,
}

#[derive(SystemOutput)]
pub struct PlantOut<B: Backing = BoxBacking> {
    pub sensors: Output<Sensors, B>,
    pub truth: Output<Truth, B>,
}

impl PlantSystem {
    pub fn new(p: PlantParams) -> Self {
        // Start rotated `init_angle` about [1,1,1] from the (identity) target, with a
        // small initial tumble about the same axis.
        let axis: V3 = tensor![1.0, 1.0, 1.0];
        let q0 = Quaternion::from_axis_angle(axis, p.init_angle);
        let omega0_world = axis.normalize() * p.init_rate;
        let body = Body {
            pos: SpatialTransform::new(q0, tensor![1.0, 0.0, 0.0]),
            vel: SpatialMotion::new(omega0_world, tensor![0.0, 0.0, 0.0]),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::new(inertia_diag(), tensor![0.0, 0.0, 0.0], 1.0),
            force: SpatialForce::zero(),
        };
        Self {
            body,
            sun_ref: tensor![0.0, 0.0, 1.0],
            mag_ref: tensor![1.0, 0.0, 0.0],
            bias: V3::zeros(),
            meas_sigma: p.meas_sigma,
            rng: StdRng::seed_from_u64(p.seed),
        }
    }

    fn noise(&mut self, sigma: f64) -> V3 {
        let d = Normal::new(0.0, sigma).unwrap();
        tensor![
            d.sample(&mut self.rng),
            d.sample(&mut self.rng),
            d.sample(&mut self.rng)
        ]
    }
}

impl<B: Backing> System<B> for PlantSystem {
    type Input = PlantIn<B>;
    type Output = Out<PlantOut<B>, B>;
    const NAME: &'static str = "plant";
}

impl<B: Backing> CyclicSystem<B> for PlantSystem {
    fn execute(&mut self, now: Timestamp, input: &mut PlantIn<B>, o: &mut Self::Output) {
        // Latest commanded torque (zero on the first cycle / if none has arrived).
        let torque: V3 = match input.torque.latest() {
            Ok(Some(cmd)) => cmd.get().torque,
            _ => V3::zeros(),
        };

        // One rk4 step of rigid-body dynamics under that (constant-over-step) torque.
        self.body = six_dof_rk4(DT, self.body.clone(), |_| SpatialForce::from_torque(torque));

        let att = self.body.pos.angular();
        let att_inv = att.inverse();
        // Gyro: world-frame rate brought into the body frame, plus a slow bias walk.
        let gyro_body = &att_inv * self.body.vel.angular();
        self.bias = self.bias + self.noise(self.meas_sigma * 1e-2);
        let gyro = gyro_body.clone() + self.bias + self.noise(self.meas_sigma);
        // Two normalized vector observations in the body frame.
        let sun_body = (&att_inv * self.sun_ref).normalize() + self.noise(self.meas_sigma);
        let mag_body = (&att_inv * self.mag_ref).normalize() + self.noise(self.meas_sigma);

        let _ = o.sensors.write(&Sensors {
            timestamp: now,
            gyro,
            sun_body,
            mag_body,
            sun_ref: self.sun_ref,
            mag_ref: self.mag_ref,
        });
        let _ = o.truth.write(&Truth {
            timestamp: now,
            q_true: att.clone(),
            omega_true: gyro_body.clone(),
        });
    }
}

impl BuildSystem for PlantSystem {
    type Params = PlantParams;
    fn new(params: Self::Params) -> Self {
        PlantSystem::new(params)
    }
}

// The C-ABI surface this cdylib exports (the `fsw_*` symbols the host resolves). Gated so
// the rlib the test links statically carries no `fsw_*` symbols (dl-open.md §8 note).
#[cfg(feature = "export")]
metor_fsw_2::export_system!(PlantSystem);
