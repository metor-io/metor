//! The rigid-body **plant** of the `adcs-fsw2` mission, as a `dlopen`-loadable `cdylib`
//! (dl-open.md §3, §8). It propagates a real 400 km orbit (gravity + orbital velocity) and
//! the attitude dynamics, driven by a three-wheel reaction-wheel actuator with friction and
//! momentum saturation — the cube-sat plant, ported onto the metor-fsw-2 cyclic-system shape.
//!
//! Each cycle it projects the commanded control torque onto the wheels, steps the wheels
//! (Euler) and the body (`six_dof_rk4` under gravity + the net wheel torque), and emits the
//! simulated sensor suite (gyro + sun + magnetometer), the orbit state (GPS), the wheel
//! telemetry, and a `truth` frame the host taps to measure convergence.

// The `export_system!`-generated `extern "C" fn fsw_*` exports take raw pointers by ABI
// contract (the host owns their validity, dl-open.md §2.5); clippy's
// `not_unsafe_ptr_arg_deref` is inherent to that macro surface for any cdylib.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use adcs_contracts::{
    ALTITUDE, DT, EARTH_RADIUS, G, M, MASS, OrbitState, PlantParams, Sensors, TorqueCmd, Truth, V3,
    Wheels, inertia_diag, mag_field_eci,
};
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

/// One body-axis-aligned reaction wheel: integrates its stored angular momentum under the
/// commanded set point, with Stribeck/Coulomb/viscous friction and a momentum-saturation
/// clamp. A disarmed wheel applies no torque (the `--disarmed` / safing gate). Ported from
/// cube-sat `ReactionWheel`.
struct ReactionWheel {
    axis: V3,
    speed: f64,
    ang_momentum: V3,
    torque_set_point: V3,
    torque: V3,
    arm: bool,
}

impl ReactionWheel {
    fn new(axis: V3, arm: bool) -> Self {
        Self {
            axis,
            speed: 0.0,
            ang_momentum: V3::zeros(),
            torque_set_point: V3::zeros(),
            torque: V3::zeros(),
            arm,
        }
    }

    fn moment_of_inertia(&self) -> f64 {
        0.185 * (0.05 / 2.0_f64).powi(2) / 2.0
    }

    fn update_speed(&mut self) {
        let i = self.moment_of_inertia();
        let momentum_norm: f64 = self.ang_momentum.norm().into_buf();
        self.speed = momentum_norm / i;
    }

    /// Friction torque (the cube-sat `rw_drag`): Stribeck near zero speed, Coulomb + viscous
    /// otherwise. Kept for fidelity with the cube-sat wheel model, which computes friction but
    /// (per its `// TODO: add friction`) does not yet feed it back into the dynamics either.
    #[allow(dead_code)]
    fn friction_torque(&self) -> f64 {
        let static_fric = 0.0005;
        let columb_fric = 0.0005;
        let stribeck_coef = 0.0005;
        let cv = 0.00005;
        let omega_limit = 0.1;
        let speed = self.speed;

        let stribeck_torque = -(2.0 * std::f64::consts::E).sqrt()
            * (static_fric - columb_fric)
            * (-((speed / stribeck_coef).powi(2))).exp()
            - columb_fric * (10.0 * speed / stribeck_coef).tanh()
            - cv * speed;

        let torque_norm: f64 = self.torque_set_point.norm().into_buf();
        let use_stribeck =
            speed.abs() < 0.01 * omega_limit && speed.signum() == torque_norm.signum();

        if use_stribeck {
            stribeck_torque
        } else {
            -columb_fric * speed.signum() - cv * speed
        }
    }

    /// Advance the wheel one step: integrate momentum under the set point (gated by arm and
    /// by the 0.04 momentum-saturation limit), clamp the applied torque, and update speed.
    fn update(&mut self) {
        if !self.arm {
            self.torque = V3::zeros();
            self.update_speed();
            return;
        }

        let rw_force_clamp = 0.002;

        let new_ang_momentum = self.ang_momentum + self.torque_set_point * DT;
        let new_momentum_norm: f64 = new_ang_momentum.norm().into_buf();
        let torque = if new_momentum_norm < 0.04 {
            self.torque_set_point
        } else {
            V3::zeros()
        };

        let clamped_torque = V3::from_buf(
            torque
                .into_buf()
                .map(|t| t.clamp(-rw_force_clamp, rw_force_clamp)),
        );

        self.ang_momentum = self.ang_momentum + clamped_torque * DT;
        self.torque = clamped_torque;
        self.update_speed();
    }
}

/// The rigid-body plant: an orbiting spacecraft whose attitude is driven by three reaction
/// wheels, emitting a noisy sensor suite.
pub struct PlantSystem {
    body: Body,
    wheels: [ReactionWheel; 3],
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
    pub orbit: Output<OrbitState, B>,
    pub wheels: Output<Wheels, B>,
    pub truth: Output<Truth, B>,
}

impl PlantSystem {
    pub fn new(p: PlantParams) -> Self {
        // A 400 km circular orbit: position along +X at orbital radius, velocity along +Y at
        // the circular-orbit speed (cube-sat `CubeSat::default`).
        let radius = EARTH_RADIUS + ALTITUDE;
        let v_orbit = (G * M / radius).sqrt();

        // Start rotated `init_angle` about [1,1,1] from the (identity) reference, with a
        // small initial tumble about the same axis.
        let axis: V3 = tensor![1.0, 1.0, 1.0];
        let q0 = Quaternion::from_axis_angle(axis, p.init_angle);
        let omega0_world = axis.normalize() * p.init_rate;
        let body = Body {
            pos: SpatialTransform::new(q0, tensor![1.0, 0.0, 0.0] * radius),
            vel: SpatialMotion::new(omega0_world, tensor![0.0, v_orbit, 0.0]),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::new(inertia_diag(), tensor![0.0, 0.0, 0.0], MASS),
            force: SpatialForce::zero(),
        };
        let arm = !p.disarmed;
        Self {
            body,
            wheels: [
                ReactionWheel::new(tensor![1.0, 0.0, 0.0], arm),
                ReactionWheel::new(tensor![0.0, 1.0, 0.0], arm),
                ReactionWheel::new(tensor![0.0, 0.0, 1.0], arm),
            ],
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

/// Point-mass Earth gravity force on `body`: `-G·M·m / |r|³ · r`.
fn gravity(body: &Body) -> V3 {
    let r = body.pos.linear();
    let r_mag = r.norm().into_buf();
    r * (-G * M * MASS / r_mag.powi(3))
}

impl<B: Backing> System<B> for PlantSystem {
    type Input = PlantIn<B>;
    type Output = Out<PlantOut<B>, B>;
    const NAME: &'static str = "plant";
}

impl<B: Backing> CyclicSystem<B> for PlantSystem {
    fn execute(&mut self, now: Timestamp, input: &mut PlantIn<B>, o: &mut Self::Output) {
        // Latest commanded body torque (zero on the first cycle / if none has arrived).
        // Project it onto each wheel's axis to form that wheel's set point, then step them.
        let torque_b: V3 = match input.torque.latest() {
            Ok(Some(cmd)) => cmd.get().torque_b,
            _ => V3::zeros(),
        };
        for wheel in &mut self.wheels {
            wheel.torque_set_point = wheel.axis.dot(&torque_b) * wheel.axis;
            wheel.update();
        }
        let rw_torque_b: V3 = self
            .wheels
            .iter()
            .fold(V3::zeros(), |acc, w| acc + w.torque);

        // Sample the sensors off the CURRENT body (cube-sat samples before integrating).
        let q_b_eci = self.body.pos.angular();
        let q_eci_b = q_b_eci.inverse();
        // Gyro: ECI rate brought into the body frame, plus a slow bias walk.
        let omega_b_true = q_eci_b * self.body.vel.angular();
        self.bias = self.bias + self.noise(self.meas_sigma * 1e-2);
        let gyro_b = omega_b_true + self.bias + self.noise(self.meas_sigma);
        // Two normalized vector observations in the body frame: the sun (a fixed inertial
        // direction) and the modeled magnetic field at the true position.
        let pos_eci = self.body.pos.linear();
        let vel_eci = self.body.vel.linear();
        let sun_eci: V3 = tensor![0.0, 0.0, 1.0];
        let sun_b = (q_eci_b * sun_eci).normalize() + self.noise(self.meas_sigma);
        let mag_b = (q_eci_b * mag_field_eci(&pos_eci)).normalize() + self.noise(self.meas_sigma);

        let _ = o.sensors.write(&Sensors {
            timestamp: now,
            gyro_b,
            sun_b,
            mag_b,
        });
        let _ = o.orbit.write(&OrbitState {
            timestamp: now,
            pos_eci,
            vel_eci,
        });
        // Per-wheel telemetry (each wheel is axis-aligned, so its scalar value sits on the
        // wheel's own body axis).
        let proj = |f: &dyn Fn(&ReactionWheel) -> V3| -> V3 {
            let mut v = [0.0; 3];
            for (i, w) in self.wheels.iter().enumerate() {
                v[i] = w.axis.dot(&f(w)).into_buf();
            }
            V3::from_buf(v)
        };
        let _ = o.wheels.write(&Wheels {
            timestamp: now,
            speed: V3::from_buf([self.wheels[0].speed, self.wheels[1].speed, self.wheels[2].speed]),
            momentum_b: proj(&|w| w.ang_momentum),
            torque_b: proj(&|w| w.torque),
        });
        let _ = o.truth.write(&Truth {
            timestamp: now,
            q_true_b_eci: q_b_eci,
            omega_true_b: omega_b_true,
        });

        // Integrate the body forward one step under gravity + the net wheel torque.
        self.body = six_dof_rk4(DT, self.body.clone(), |b| {
            SpatialForce::from_torque(rw_torque_b) + SpatialForce::from_linear(gravity(b))
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
