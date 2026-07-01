//! The rigid-body **plant** of the `adcs-fsw2` mission, as a `dlopen`-loadable `cdylib`
//! (dl-open.md §3, §8). It propagates a real 400 km orbit (gravity + orbital velocity) and
//! the attitude dynamics, driven by a three-wheel reaction-wheel actuator with friction and
//! momentum saturation — the cube-sat plant, ported onto the metor-fsw-2 cyclic-system shape.
//!
//! Each cycle it projects the commanded control torque onto the wheels, steps the wheels
//! (Euler) and the body (`six_dof_rk4` under gravity + the net wheel torque), and emits the
//! simulated sensor suite (gyro + sun + magnetometer, the field from the NOAA WMM), a noisy
//! `gps` measurement the flight software flies on (a Gauss-Markov position error + white
//! velocity noise), the wheel telemetry, the true `world` environment, and a `body` truth
//! frame the host taps to measure convergence.

// The `export_system!`-generated `extern "C" fn fsw_*` exports take raw pointers by ABI
// contract (the host owns their validity, dl-open.md §2.5); clippy's
// `not_unsafe_ptr_arg_deref` is inherent to that macro surface for any cdylib.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use adcs_contracts::{
    ALTITUDE, BodyState, DT, EARTH_RADIUS, G, GPS_POS_SIGMA, GPS_TAU, GPS_VEL_SIGMA, Gps, M,
    MASS, MagneticModel, PlantParams, ReactionWheel, Sensors, TorqueCmd, V3, Wheels, World,
    epoch_at, inertia_diag, mag_field_eci, sun_dir_eci,
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

/// The rigid-body plant: an orbiting spacecraft whose attitude is driven by three reaction
/// wheels, emitting a noisy sensor suite. The wheels are the shared [`ReactionWheel`] contract
/// type — the same struct the `wheels` telemetry frame carries.
pub struct PlantSystem {
    body: Body,
    wheels: [ReactionWheel; 3],
    bias: V3,
    meas_sigma: f64,
    rng: StdRng,
    /// Seconds of simulated mission time (a deterministic per-cycle counter, not wall time)
    /// — drives the epoch the real sun direction + WMM field are evaluated at.
    t_sim: f64,
    /// The NOAA WMM handle for the true magnetic field (built once — holds C-library state).
    mag_model: MagneticModel,
    /// The GPS position error: a first-order Gauss-Markov (exponentially-correlated) state,
    /// stepped each cycle and added to the true position to form the [`Gps`] measurement.
    gps_pos_err: V3,
}

#[derive(SystemInput)]
pub struct PlantIn<B: Backing = BoxBacking> {
    pub torque: Input<TorqueCmd, B>,
}

#[derive(SystemOutput)]
pub struct PlantOut<B: Backing = BoxBacking> {
    pub sensors: Output<Sensors, B>,
    pub gps: Output<Gps, B>,
    pub wheels: Output<Wheels, B>,
    pub body: Output<BodyState, B>,
    pub world: Output<World, B>,
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
            t_sim: 0.0,
            mag_model: MagneticModel::default(),
            gps_pos_err: V3::zeros(),
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
        // The true ECI environment at this epoch/position: the real sun direction (nox-frames
        // Vallado model) and the NOAA WMM magnetic field, both at the deterministic mission epoch.
        let epoch = epoch_at(self.t_sim);
        let pos_eci = self.body.pos.linear();
        let vel_eci = self.body.vel.linear();
        let sun_eci = sun_dir_eci(epoch);
        let mag_eci = mag_field_eci(&mut self.mag_model, epoch, &pos_eci);
        // The two normalized vector observations, those ECI references brought into the body
        // frame plus sensor noise.
        let sun_b = (q_eci_b * sun_eci).normalize() + self.noise(self.meas_sigma);
        let mag_b = (q_eci_b * mag_eci).normalize() + self.noise(self.meas_sigma);

        // The GPS measurement: the true orbit state corrupted by the GPS error model. Position
        // error is a first-order Gauss-Markov process (exponentially correlated); velocity error
        // is white. These RNG draws come AFTER the sensor draws above so the sensor noise stays
        // byte-identical regardless of the GPS model.
        let gps_phi = (-DT / GPS_TAU).exp();
        let gps_drive_sigma = GPS_POS_SIGMA * (1.0 - gps_phi * gps_phi).sqrt();
        self.gps_pos_err = gps_phi * self.gps_pos_err + self.noise(gps_drive_sigma);
        let gps_pos_eci = pos_eci + self.gps_pos_err;
        let gps_vel_eci = vel_eci + self.noise(GPS_VEL_SIGMA);

        let _ = o.sensors.write(&Sensors {
            timestamp: now,
            gyro_b,
            sun_b,
            mag_b,
        });
        let _ = o.gps.write(&Gps {
            timestamp: now,
            pos_eci: gps_pos_eci,
            vel_eci: gps_vel_eci,
        });
        let _ = o.world.write(&World {
            timestamp: now,
            sun_eci,
            mag_eci,
        });
        // Per-wheel telemetry — the wheels themselves, the same structs the plant integrates.
        let _ = o.wheels.write(&Wheels {
            timestamp: now,
            wheels: self.wheels.clone(),
        });
        // The ground-truth body state: attitude + rate (truth) and the orbit (GPS) together.
        let _ = o.body.write(&BodyState {
            timestamp: now,
            q_b_eci,
            omega_b: omega_b_true,
            pos_eci,
            vel_eci,
        });

        // Integrate the body forward one step under gravity + the net wheel torque.
        self.body = six_dof_rk4(DT, self.body.clone(), |b| {
            SpatialForce::from_torque(rw_torque_b) + SpatialForce::from_linear(gravity(b))
        });
        // Advance the deterministic mission clock (drives the sun epoch — never wall time).
        self.t_sim += DT;
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
