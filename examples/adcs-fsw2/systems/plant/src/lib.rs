//! The rigid-body **plant** of the `adcs-fsw2` mission, as a `dlopen`-loadable `cdylib`
//! (dl-open.md §3, §8). It propagates a real 400 km orbit (gravity + drag) and the full
//! Euler attitude dynamics — including the gyroscopic coupling of the stored reaction-wheel
//! momentum — under the environmental disturbance torques a small spacecraft actually lives
//! on (gravity gradient, aero drag about the CP–CG offset, residual magnetic dipole, SRP),
//! driven by two actuators: three reaction wheels (bearing friction + momentum-saturation
//! foldback, both inside the dynamics) and three magnetorquers (`τ = m × B`, the desat /
//! detumble authority).
//!
//! Each cycle it steps the wheels under the commanded control torque, samples the simulated
//! sensor suite (gyro + sun + a Tesla-valued magnetometer reading the NOAA WMM field), emits
//! the noisy `gps` measurement the flight software flies on (a Gauss-Markov position error +
//! white velocity noise), the wheel and disturbance telemetry, the true `world` environment,
//! and the `body` truth frame the host taps to measure convergence — then integrates the
//! body one step (`six_dof_rk4`, body-fixed torques rotated per substep, the wheel-momentum
//! coupling `−ω_b × h_w` evaluated per substep).
//!
//! Authored with `#[system]` (`docs/design-system-macro.md`): the port set is the `execute`
//! signature, and the bundles/trait impls/`BuildSystem`/`fsw_*` exports are all generated.

use adcs_contracts::{
    ALTITUDE, BodyState, DT, Disturbances, EARTH_RADIUS, GPS_POS_SIGMA, GPS_TAU, GPS_VEL_SIGMA,
    Gps, MAG_SENSOR_SIGMA, MASS, MU, MagneticModel, MtqCmd, PlantParams, ReactionWheel, Sensors,
    TorqueCmd, V3, Wheels, World, clamp_dipole, disturbance_torques, epoch_at, inertia_diag,
    mag_field_eci, sun_dir_eci,
};
use metor_fsw_2::{Input, Output, Timestamp, system};
use nox::{
    Body, Quaternion, SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform, six_dof_rk4,
    tensor,
};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, Normal};

/// The rigid-body plant: an orbiting spacecraft whose attitude is driven by three reaction
/// wheels and three magnetorquers against the disturbance environment, emitting a noisy
/// sensor suite. The wheels are the shared [`ReactionWheel`] contract type — the same struct
/// the `wheels` telemetry frame carries.
pub struct PlantSystem {
    body: Body,
    wheels: [ReactionWheel; 3],
    bias: V3,
    rng: StdRng,
    /// Seconds of simulated mission time (a deterministic per-cycle counter, not wall time)
    /// — drives the epoch the real sun direction + WMM field are evaluated at.
    t_sim: f64,
    /// The NOAA WMM handle for the true magnetic field (built once — holds C-library state).
    mag_model: MagneticModel,
    /// The GPS position error: a first-order Gauss-Markov (exponentially-correlated) state,
    /// stepped each cycle and added to the true position to form the [`Gps`] measurement.
    gps_pos_err: V3,
    /// The full plant config — the sensor sigmas and the disturbance/actuator environment
    /// ([`disturbance_torques`] reads it every cycle).
    params: PlantParams,
}

/// Point-mass Earth gravity force on `body`: `-MU·m / |r|³ · r`.
fn gravity(body: &Body) -> V3 {
    let r = body.pos.linear();
    let r_mag = r.norm().into_buf();
    r * (-MU * MASS / r_mag.powi(3))
}

/// Advance `body` one `DT` step: per-substep point-mass gravity plus a constant drag force
/// on the orbit, and the body-fixed torque `tau_body_const` (wheel reaction + disturbances +
/// MTQ — computed once at the pre-step state, but *rotated per RK4 substep* since it rides
/// the body) with the wheel-momentum gyroscopic coupling `−ω_b × h_w_b` evaluated per
/// substep. The body's own `ω × Iω` Euler term lives inside `six_dof_rk4`.
///
/// `h_w_b` must be the wheel momentum at the **step midpoint** (the wheels update before the
/// body steps, so that is `h_post + rw_torque_b·DT/2`): the wheels' momentum changes linearly
/// across the step while the coupling holds it constant, and centering it cancels the
/// first-order error — one-sided bookkeeping loses total angular momentum secularly.
/// Free-standing so the plant's physics tests can drive it without the port harness.
pub fn propagate(body: Body, tau_body_const: V3, h_w_b: V3, f_drag_eci: V3) -> Body {
    six_dof_rk4(DT, body, move |b| {
        let q = b.pos.angular();
        let omega_b = q.inverse() * b.vel.angular();
        let tau_b = tau_body_const - omega_b.cross(&h_w_b);
        SpatialForce::from_torque(q * tau_b)
            + SpatialForce::from_linear(gravity(b) + f_drag_eci)
    })
}

#[system(name = "plant", export = "export")]
impl PlantSystem {
    pub fn new(p: PlantParams) -> Self {
        // A 400 km circular orbit: position along +X at orbital radius, velocity along +Y at
        // the circular-orbit speed (cube-sat `CubeSat::default`).
        let radius = EARTH_RADIUS + ALTITUDE;
        let v_orbit = (MU / radius).sqrt();

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
                ReactionWheel::new(tensor![1.0, 0.0, 0.0], arm).with_momentum(p.init_wheel_h),
                ReactionWheel::new(tensor![0.0, 1.0, 0.0], arm).with_momentum(p.init_wheel_h),
                ReactionWheel::new(tensor![0.0, 0.0, 1.0], arm).with_momentum(p.init_wheel_h),
            ],
            bias: V3::zeros(),
            rng: StdRng::seed_from_u64(p.seed),
            t_sim: 0.0,
            mag_model: MagneticModel::default(),
            gps_pos_err: V3::zeros(),
            params: p,
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

    #[allow(clippy::too_many_arguments)]
    fn execute(
        &mut self,
        now: Timestamp,
        torque: &mut Input<TorqueCmd>,
        mtq: &mut Input<MtqCmd>,
        sensors: &mut Output<Sensors>,
        gps: &mut Output<Gps>,
        wheels: &mut Output<Wheels>,
        body: &mut Output<BodyState>,
        world: &mut Output<World>,
        disturb: &mut Output<Disturbances>,
    ) {
        // Latest commanded body torque / magnetorquer dipole (zero on the first cycle / if
        // none has arrived). Project the torque onto each wheel's axis to form that wheel's
        // set point, then step them; the wheel `torque` fields come back as the reaction
        // delivered to the body.
        let torque_b: V3 = match torque.latest() {
            Some(cmd) => cmd.get().torque_b,
            None => V3::zeros(),
        };
        let dipole_b: V3 = match mtq.latest() {
            Some(cmd) => cmd.get().dipole_b,
            None => V3::zeros(),
        };
        for wheel in &mut self.wheels {
            wheel.torque_set_point = wheel.axis.dot(&torque_b) * wheel.axis;
            wheel.update();
        }
        let rw_torque_b: V3 = self
            .wheels
            .iter()
            .fold(V3::zeros(), |acc, w| acc + w.torque);
        let h_w_b: V3 = self
            .wheels
            .iter()
            .fold(V3::zeros(), |acc, w| acc + w.ang_momentum);

        // Sample the sensors off the CURRENT body (cube-sat samples before integrating).
        let q_b_eci = self.body.pos.angular();
        let q_eci_b = q_b_eci.inverse();
        // Gyro: ECI rate brought into the body frame, plus a slow bias walk.
        let omega_b_true = q_eci_b * self.body.vel.angular();
        let meas_sigma = self.params.meas_sigma;
        self.bias = self.bias + self.noise(meas_sigma * 1e-2);
        let gyro_b = omega_b_true + self.bias + self.noise(meas_sigma);
        // The true ECI environment at this epoch/position: the real sun direction (nox-frames
        // Vallado model) and the NOAA WMM magnetic field, both at the deterministic mission epoch.
        let epoch = epoch_at(self.t_sim);
        let pos_eci = self.body.pos.linear();
        let vel_eci = self.body.vel.linear();
        let sun_eci = sun_dir_eci(epoch);
        let mag_eci = mag_field_eci(&mut self.mag_model, epoch, &pos_eci);
        // The sun observation is a normalized unit vector; the magnetometer reads the
        // physical field (Tesla) — real magnetometers measure magnitude, and the desat law
        // downstream needs |B|. Both plus sensor noise.
        let sun_b = (q_eci_b * sun_eci).normalize() + self.noise(meas_sigma);
        let mag_b_true = q_eci_b * mag_eci;
        let mag_b = mag_b_true + self.noise(MAG_SENSOR_SIGMA);

        // The GPS measurement: the true orbit state corrupted by the GPS error model. Position
        // error is a first-order Gauss-Markov process (exponentially correlated); velocity error
        // is white. These RNG draws come AFTER the sensor draws above so the sensor noise stays
        // byte-identical regardless of the GPS model.
        let gps_phi = (-DT / GPS_TAU).exp();
        let gps_drive_sigma = GPS_POS_SIGMA * (1.0 - gps_phi * gps_phi).sqrt();
        self.gps_pos_err = gps_phi * self.gps_pos_err + self.noise(gps_drive_sigma);
        let gps_pos_eci = pos_eci + self.gps_pos_err;
        let gps_vel_eci = vel_eci + self.noise(GPS_VEL_SIGMA);

        // The disturbance environment at the pre-step true state, and the magnetorquer
        // torque (the commanded dipole clamped to the torquer's authority, crossed with the
        // true field). All deterministic — no RNG draws.
        let d = disturbance_torques(&self.params, &q_b_eci, &pos_eci, &vel_eci, &sun_eci, &mag_eci);
        let mtq_torque_b = clamp_dipole(dipole_b, self.params.mtq_max_dipole).cross(&mag_b_true);

        sensors.publish(&Sensors {
            timestamp: now,
            gyro_b,
            sun_b,
            mag_b,
        });
        gps.publish(&Gps {
            timestamp: now,
            pos_eci: gps_pos_eci,
            vel_eci: gps_vel_eci,
        });
        world.publish(&World {
            timestamp: now,
            sun_eci,
            mag_eci,
        });
        // Per-wheel telemetry — the wheels themselves, the same structs the plant integrates.
        wheels.publish(&Wheels {
            timestamp: now,
            wheels: self.wheels.clone(),
        });
        // The ground-truth body state: attitude + rate (truth) and the orbit (GPS) together.
        body.publish(&BodyState {
            timestamp: now,
            q_b_eci,
            omega_b: omega_b_true,
            pos_eci,
            vel_eci,
        });
        disturb.publish(&Disturbances {
            timestamp: now,
            gg_b: d.gg_b,
            aero_b: d.aero_b,
            mag_b: d.mag_b,
            srp_b: d.srp_b,
            mtq_b: mtq_torque_b,
            total_b: d.total_b(),
        });

        // Integrate the body forward one step under everything applied this cycle. The
        // coupling momentum is centered on the step midpoint (see `propagate`).
        let tau_body = rw_torque_b + d.total_b() + mtq_torque_b;
        let h_w_mid = h_w_b + rw_torque_b * (DT / 2.0);
        self.body = propagate(self.body.clone(), tau_body, h_w_mid, d.aero_force_eci);
        // Advance the deterministic mission clock (drives the sun epoch — never wall time).
        self.t_sim += DT;
    }
}

#[cfg(test)]
mod tests {
    use adcs_contracts::{MTQ_MAX_DIPOLE, RW_TORQUE_MAX, detumble_dipole};

    use super::*;

    fn orbit_body(incline: f64, omega0: V3) -> Body {
        let radius = EARTH_RADIUS + ALTITUDE;
        let v_orbit = (MU / radius).sqrt();
        let vel: V3 = tensor![0.0, incline.cos(), incline.sin()] * v_orbit;
        Body {
            pos: SpatialTransform::new(
                Quaternion::from_axis_angle(tensor![1.0, 1.0, 1.0], 0.5),
                tensor![1.0, 0.0, 0.0] * radius,
            ),
            vel: SpatialMotion::new(omega0, vel),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::new(inertia_diag(), tensor![0.0, 0.0, 0.0], MASS),
            force: SpatialForce::zero(),
        }
    }

    /// The spin angular momentum `L_eci = q ⊛ (I∘ω_b + h_w)` with the wheels actively
    /// slewing but disturbances/MTQ off: gravity is central (torque-free about the CoM), and
    /// motor/friction torques are internal, so `L_eci` must be conserved. The drift bound is
    /// set by the integrator (first-order quaternion update inside RK4) and by holding the
    /// wheel exchange constant over each step — not by the dynamics.
    #[test]
    fn total_angular_momentum_conserved_without_external_torque() {
        let mut body = orbit_body(0.0, tensor![0.05, -0.08, 0.06]);
        let mut wheels = [
            ReactionWheel::new(tensor![1.0, 0.0, 0.0], true),
            ReactionWheel::new(tensor![0.0, 1.0, 0.0], true),
            ReactionWheel::new(tensor![0.0, 0.0, 1.0], true),
        ];
        let l_eci = |body: &Body, wheels: &[ReactionWheel; 3]| -> V3 {
            let q = body.pos.angular();
            let omega_b = q.inverse() * body.vel.angular();
            let h_w = wheels
                .iter()
                .fold(V3::zeros(), |acc, w| acc + w.ang_momentum);
            q * (inertia_diag() * omega_b + h_w)
        };
        let l0 = l_eci(&body, &wheels);
        for k in 0..5000 {
            let t = k as f64 * DT;
            for (i, w) in wheels.iter_mut().enumerate() {
                // Sinusoidal set points, out of phase per wheel — constant momentum exchange.
                let u = RW_TORQUE_MAX * (0.5 * t + i as f64).sin();
                w.torque_set_point = w.axis * u;
                w.update();
            }
            let rw_torque = wheels.iter().fold(V3::zeros(), |acc, w| acc + w.torque);
            let h_w = wheels
                .iter()
                .fold(V3::zeros(), |acc, w| acc + w.ang_momentum);
            let h_w_mid = h_w + rw_torque * (DT / 2.0);
            body = propagate(body, rw_torque, h_w_mid, V3::zeros());
        }
        let l = l_eci(&body, &wheels);
        let drift: f64 = (l - l0).norm().into_buf();
        let l_mag: f64 = l0.norm().into_buf();
        // Measured integrator truncation is ~1e-5 relative over this run (an equivalent
        // parasitic torque of ~4e-10 N·m, three orders below the modeled disturbances); the
        // bookkeeping errors this test guards against show up at percent level.
        assert!(drift / l_mag < 5e-5, "L_eci drifted: {}", drift / l_mag);
    }

    /// The B-cross detumble law closed on the true dynamics damps the field-perpendicular
    /// rate component. Gain and torquer authority are cranked well above the mission values
    /// so the decay fits a test budget — the asserted property is the law's, not the gain's.
    /// (The field-parallel component is untouchable per instant; an inclined orbit rotates
    /// B̂ too slowly to matter over this window, so only ω_⊥ is asserted.)
    #[test]
    fn detumble_law_damps_tumble() {
        let omega0: V3 = (tensor![1.0, 1.0, 1.0] as V3).normalize() * 0.1;
        let mut body = orbit_body(0.9, omega0);
        let mut mag_model = MagneticModel::default();
        let k_detumble = 5e-4; // mission default ×10
        let mtq_max = 10.0 * MTQ_MAX_DIPOLE;
        let mut t_sim = 0.0;
        let omega_perp = |body: &Body, mag_b: &V3| -> f64 {
            let q = body.pos.angular();
            let omega_b = q.inverse() * body.vel.angular();
            let b_hat = mag_b.normalize();
            let along: f64 = omega_b.dot(&b_hat).into_buf();
            (omega_b - b_hat * along).norm().into_buf()
        };
        let mut perp0 = None;
        let mut perp = f64::MAX;
        // The WMM field varies on the orbit timescale, so re-evaluating it every cycle only
        // slows the test — refresh it once per 100-step block (0.83 s of sim time).
        let mut mag_eci = V3::zeros();
        for k in 0..15_000 {
            let q = body.pos.angular();
            let pos = body.pos.linear();
            if k % 100 == 0 {
                mag_eci = mag_field_eci(&mut mag_model, epoch_at(t_sim), &pos);
            }
            let mag_b = q.inverse() * mag_eci;
            let omega_b = q.inverse() * body.vel.angular();
            let m = clamp_dipole(detumble_dipole(k_detumble, &omega_b, &mag_b), mtq_max);
            let tau_mtq_b = m.cross(&mag_b);
            body = propagate(body, tau_mtq_b, V3::zeros(), V3::zeros());
            t_sim += DT;
            perp = omega_perp(&body, &mag_b);
            perp0.get_or_insert(perp);
        }
        let perp0 = perp0.unwrap();
        assert!(perp0 > 0.05, "tumble started perpendicular-degenerate: {perp0}");
        assert!(perp < 5e-3, "detumble failed to damp ω_⊥: {perp0} → {perp}");
    }
}
