//! The simulated spacecraft.
//!
//! Each cycle the plant steps the wheels under the last torque command, samples
//! its sensors from the state at the start of the cycle, writes telemetry, and
//! integrates the body one step. All noise comes from one seeded RNG, so a seed
//! reproduces a run exactly.
//!
//! The GPS position error follows a first-order Gauss-Markov process because
//! ionospheric and ephemeris errors vary slowly. Velocity error from Doppler
//! tracking is nearly uncorrelated, so it is white.

use adcs_contracts::{
    BodyState, DT, Disturbances, EARTH_RADIUS, Epoch, GPS_POS_SIGMA, GPS_VEL_SIGMA, Gps, GpsError,
    MASS, MU, MagneticModel, MtqCmd, PlantParams, Quat, RW_MOMENTUM_MAX, RW_TORQUE_MAX,
    ReactionWheel, Sensors, TorqueCmd, V3, Wheels, World, clamp_dipole, eci_to_geodetic, epoch_at,
    in_earth_shadow, inertia_diag, mag_field_eci, orbit_pos_vel, raan_for_ltan, sun_dir_eci,
    target_epoch,
};
use metor_fsw_3::{Input, Output, Timestamp, system};
use nox::{
    Body, Quaternion, SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform, six_dof_rk4,
    tensor,
};
use rand::{SeedableRng, rngs::StdRng};
use rand_distr::{Distribution, Normal};

/// Magnetometer noise 1σ per axis (T), against a field of 25 to 45 µT at 400 km.
pub const MAG_SENSOR_SIGMA: f64 = 150e-9;

/// Solar radiation pressure at 1 AU (N/m²).
pub const P_SRP: f64 = 4.56e-6;

/// Sun sensor head normals, in [`Sensors::css`] order. Six orthogonal 90° cones
/// cover the sphere, so some head sees the sun whenever the spacecraft is lit.
const CSS_NORMALS: [[f64; 3]; 6] = [
    [1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 0.0, 1.0],
    [-1.0, 0.0, 0.0],
    [0.0, -1.0, 0.0],
    [0.0, 0.0, -1.0],
];

/// Returns the noiseless sun sensor readings for a body-frame sun direction.
pub fn css_readings(sun_b: &V3, illuminated: bool) -> [f64; 6] {
    let lit = if illuminated { 1.0 } else { 0.0 };
    CSS_NORMALS.map(|n| {
        let cos: f64 = V3::from_buf(n).dot(sun_b).into_buf();
        cos.max(0.0) * lit
    })
}

/// Wheel rotor inertia (kg·m²), a 185 g disc 5 cm across.
pub const RW_MOI: f64 = 0.185 * (0.05 / 2.0) * (0.05 / 2.0) / 2.0;
/// Coulomb bearing friction (N·m).
pub const RW_COULOMB: f64 = 1.0e-5;
/// Viscous bearing friction (N·m·s/rad).
pub const RW_VISCOUS: f64 = 1.0e-7;
/// Speed (rad/s) below which friction holds the wheel still instead of chattering
/// across zero.
pub const RW_STICTION_OMEGA: f64 = 1e-3;

/// Reaction wheel physics.
pub trait WheelDynamics {
    /// Returns the wheel preloaded with `h` N·m·s along its axis.
    fn with_momentum(self, h: f64) -> Self;

    /// Advances the wheel one step.
    ///
    /// The motor torque is the negated set point, clamped to [`RW_TORQUE_MAX`] and
    /// zero when disarmed. Friction can stop the wheel within a step but never
    /// reverse it. Near [`RW_MOMENTUM_MAX`] the motor torque folds back so the
    /// momentum lands exactly on the limit, while torque that unloads the wheel
    /// always passes.
    fn update(&mut self);
}

impl WheelDynamics for ReactionWheel {
    fn with_momentum(mut self, h: f64) -> Self {
        self.ang_momentum = self.axis * h;
        self.speed = h / RW_MOI;
        self
    }

    fn update(&mut self) {
        let s: f64 = self.axis.dot(&self.ang_momentum).into_buf();
        let omega = s / RW_MOI;

        let motor = if self.armed() {
            let u: f64 = self.axis.dot(&self.torque_set_point).into_buf();
            (-u).clamp(-RW_TORQUE_MAX, RW_TORQUE_MAX)
        } else {
            0.0
        };

        let friction = if omega.abs() < RW_STICTION_OMEGA {
            0.0
        } else {
            let f = -RW_COULOMB * omega.signum() - RW_VISCOUS * omega;
            // Stop at zero rather than reversing.
            if omega > 0.0 {
                f.max(-s / DT)
            } else {
                f.min(-s / DT)
            }
        };

        let mut h_dot = motor + friction;
        let s_next = s + h_dot * DT;
        if s_next > RW_MOMENTUM_MAX {
            h_dot = (RW_MOMENTUM_MAX - s) / DT;
        } else if s_next < -RW_MOMENTUM_MAX {
            h_dot = (-RW_MOMENTUM_MAX - s) / DT;
        }

        let s = s + h_dot * DT;
        self.ang_momentum = self.axis * s;
        self.speed = s / RW_MOI;
        self.friction = friction;
        self.torque = self.axis * -h_dot;
    }
}

/// The disturbance torques at one state, with the drag force that also slows the orbit.
pub struct DisturbanceTorques {
    /// Gravity-gradient torque (N·m).
    pub gg_b: V3,
    /// Drag torque (N·m).
    pub aero_b: V3,
    /// Residual dipole torque `m_res × B` (N·m).
    pub mag_b: V3,
    /// Solar radiation pressure torque (N·m).
    pub srp_b: V3,
    /// Drag force (N).
    pub aero_force_eci: V3,
}

/// The true state the disturbance torques act on.
pub struct TrueState {
    pub q_b_eci: Quat,
    pub pos_eci: V3,
    pub vel_eci: V3,
    pub sun_eci: V3,
    /// True field (T), unnormalized.
    pub mag_eci: V3,
    pub illuminated: bool,
}

impl DisturbanceTorques {
    /// Evaluates the disturbance torques at the true state `s`.
    ///
    /// - Gravity gradient: `τ = (3μ/|r|³) · r̂_b × (I ∘ r̂_b)`.
    /// - Drag: `F = −½ρ·Cd·A·|v|·v` against a still atmosphere, `τ = r_cp × F_b`.
    /// - Residual dipole: `τ = m_res × B_b`.
    /// - Solar radiation pressure: `F = −P·A·Cr·ŝ_b`, `τ = r_cp × F_b`, zero in shadow.
    pub fn at(p: &PlantParams, s: &TrueState) -> Self {
        let q_eci_b = s.q_b_eci.inverse();
        let cp_offset_b = V3::from_buf(p.cp_offset_b);

        let r_mag: f64 = s.pos_eci.norm().into_buf();
        let r_hat_b = q_eci_b * (s.pos_eci.normalize());
        let gg_b = r_hat_b.cross(&(inertia_diag() * r_hat_b)) * (3.0 * MU / r_mag.powi(3));

        let v_mag: f64 = s.vel_eci.norm().into_buf();
        let aero_force_eci = s.vel_eci * (-0.5 * p.rho * p.cd * p.area_aero * v_mag);
        let aero_b = cp_offset_b.cross(&(q_eci_b * aero_force_eci));

        let mag_b = V3::from_buf(p.m_res_b).cross(&(q_eci_b * s.mag_eci));

        let srp_force_b = if s.illuminated {
            (q_eci_b * s.sun_eci.normalize()) * (-P_SRP * p.area_srp * p.cr)
        } else {
            V3::zeros()
        };
        let srp_b = cp_offset_b.cross(&srp_force_b);

        DisturbanceTorques {
            gg_b,
            aero_b,
            mag_b,
            srp_b,
            aero_force_eci,
        }
    }

    /// Returns the sum of the torques.
    pub fn total_b(&self) -> V3 {
        self.gg_b + self.aero_b + self.mag_b + self.srp_b
    }
}

/// The simulated spacecraft, its wheels, and its sensor models.
pub struct PlantState {
    body: Body,
    wheels: [ReactionWheel; 3],
    /// Gyro bias, a random walk.
    bias: V3,
    rng: StdRng,
    /// Simulated seconds since boot, advanced one [`DT`] per cycle.
    t_sim: f64,
    mag_model: MagneticModel,
    /// GPS position error, stepped each cycle.
    gps_pos_err: GpsError,
    params: PlantParams,
    /// Illumination last cycle, so each shadow edge logs once.
    was_illuminated: Option<bool>,
    /// The unit normal that every noise draw scales.
    unit_normal: Normal<f64>,
}

/// The ports one cycle's telemetry goes to.
struct Ports<'a> {
    sensors: &'a mut Output<Sensors>,
    gps: &'a mut Output<Gps>,
    wheels: &'a mut Output<Wheels>,
    body: &'a mut Output<BodyState>,
    world: &'a mut Output<World>,
    disturb: &'a mut Output<Disturbances>,
}

/// The true state at the start of a cycle and what the sensors measured.
struct Sample {
    truth: TrueState,
    omega_b_true: V3,
    /// True field in the body frame, which the magnetorquers act against.
    mag_b_true: V3,
    sensors: Sensors,
    gps: Gps,
}

/// The wheels' reaction torque and momentum after a step, and the commanded
/// dipole.
struct Actuation {
    rw_torque_b: V3,
    h_w_b: V3,
    dipole_b: V3,
}

/// Returns point-mass gravity on `body`, `-MU·m / |r|³ · r`.
fn gravity(body: &Body) -> V3 {
    let r = body.pos.linear();
    let r_mag = r.norm().into_buf();
    r * (-MU * MASS / r_mag.powi(3))
}

/// Advances `body` one step under the body torque `tau_body_const`, gravity, and drag.
///
/// The torque rides with the body, so each RK4 substep rotates it by that
/// substep's attitude and adds the wheel coupling `−ω_b × h_w_b`.
///
/// Pass `h_w_b` at the step midpoint. Wheel momentum changes linearly across the
/// step while the coupling holds it constant, and centering it cancels the
/// first-order error that would otherwise leak total angular momentum.
pub fn propagate(body: Body, tau_body_const: V3, h_w_b: V3, f_drag_eci: V3) -> Body {
    six_dof_rk4(DT, body, move |b| {
        let q = b.pos.angular();
        let omega_b = q.inverse() * b.vel.angular();
        let tau_b = tau_body_const - omega_b.cross(&h_w_b);
        SpatialForce::from_torque(q * tau_b) + SpatialForce::from_linear(gravity(b) + f_drag_eci)
    })
}

impl PlantState {
    /// Builds the plant from its params.
    pub fn new(p: PlantParams) -> PlantState {
        let radius = EARTH_RADIUS + p.altitude;
        let raan = raan_for_ltan(target_epoch(), p.ltan_hours);
        let (pos0, vel0) = orbit_pos_vel(radius, p.inclination, raan, p.init_orbit_phase);

        let axis: V3 = tensor![1.0, 1.0, 1.0];
        let q0 = Quaternion::from_axis_angle(axis, p.init_angle);
        let omega0_world = axis.normalize() * p.init_rate;
        let body = Body {
            pos: SpatialTransform::new(q0, pos0),
            vel: SpatialMotion::new(omega0_world, vel0),
            accel: SpatialMotion::zero(),
            inertia: SpatialInertia::new(inertia_diag(), tensor![0.0, 0.0, 0.0], MASS),
            force: SpatialForce::zero(),
        };
        let arm = !p.disarmed;
        tracing::info!(
            wheels_armed = arm,
            init_rate = p.init_rate,
            orbit_phase = p.init_orbit_phase,
            "plant booted"
        );
        PlantState {
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
            gps_pos_err: GpsError::new(GPS_POS_SIGMA),
            params: p,
            was_illuminated: None,
            // PANIC Safety: a unit standard deviation is a valid normal.
            unit_normal: Normal::new(0.0, 1.0).expect("a unit normal"),
        }
    }

    /// Steps the wheels under the torque command. A missing command counts as
    /// zero.
    fn actuate(
        &mut self,
        torque_cmd: &mut Input<TorqueCmd>,
        mtq_cmd: &mut Input<MtqCmd>,
    ) -> Actuation {
        let torque_b: V3 = match torque_cmd.latest() {
            Ok(Some(cmd)) => cmd.torque_b,
            _ => V3::zeros(),
        };
        let dipole_b: V3 = match mtq_cmd.latest() {
            Ok(Some(cmd)) => cmd.dipole_b,
            _ => V3::zeros(),
        };
        // Each wheel takes the command's component along its axis.
        for wheel in &mut self.wheels {
            wheel.torque_set_point = wheel.axis.dot(&torque_b) * wheel.axis;
            wheel.update();
        }
        let rw_torque_b = self
            .wheels
            .iter()
            .fold(V3::zeros(), |acc, w| acc + w.torque);
        let h_w_b = self
            .wheels
            .iter()
            .fold(V3::zeros(), |acc, w| acc + w.ang_momentum);
        Actuation {
            rw_torque_b,
            h_w_b,
            dipole_b,
        }
    }

    /// Samples the sensors and the GPS receiver from the state at the start of the step.
    fn sense(&mut self, now: Timestamp) -> Sample {
        let q_b_eci = self.body.pos.angular();
        let q_eci_b = q_b_eci.inverse();
        let omega_b_true = q_eci_b * self.body.vel.angular();
        let gyro_b = self.gyro(omega_b_true);

        let epoch = epoch_at(self.t_sim);
        let pos_eci = self.body.pos.linear();
        let vel_eci = self.body.vel.linear();
        let sun_eci = sun_dir_eci(epoch);
        let mag_eci = mag_field_eci(&mut self.mag_model, epoch, &pos_eci);
        let illuminated = self.log_shadow_edges(!in_earth_shadow(&pos_eci, &sun_eci));

        let css = self.css((q_eci_b * sun_eci).normalize(), illuminated);
        // Unnormalized, since the unloading law needs |B|.
        let mag_b_true = q_eci_b * mag_eci;
        let mag_b = mag_b_true + self.noise(MAG_SENSOR_SIGMA);

        Sample {
            truth: TrueState {
                q_b_eci,
                pos_eci,
                vel_eci,
                sun_eci,
                mag_eci,
                illuminated,
            },
            omega_b_true,
            mag_b_true,
            sensors: Sensors {
                timestamp: now,
                gyro_b,
                css,
                mag_b,
            },
            gps: self.gps_fix(now, epoch, &pos_eci, &vel_eci),
        }
    }

    /// Returns the true body rate plus the bias and white noise.
    fn gyro(&mut self, omega_b_true: V3) -> V3 {
        let sigma = self.params.meas_sigma;
        self.bias = self.bias + self.noise(sigma * 1e-2);
        omega_b_true + self.bias + self.noise(sigma)
    }

    /// Returns the sun sensor readings with each head's noise added.
    fn css(&mut self, sun_b_true: V3, illuminated: bool) -> [f64; 6] {
        let sigma = self.params.meas_sigma;
        let mut css = css_readings(&sun_b_true, illuminated);
        for reading in &mut css {
            *reading += self.noise1(sigma);
        }
        css
    }

    /// Returns a GPS fix of the true orbit state.
    fn gps_fix(&mut self, now: Timestamp, epoch: Epoch, pos_eci: &V3, vel_eci: &V3) -> Gps {
        let unit = self.noise(1.0);
        let pos = *pos_eci + self.gps_pos_err.step(DT, unit);
        let vel = *vel_eci + self.noise(GPS_VEL_SIGMA);
        let (lat, lon, alt) = eci_to_geodetic(epoch, &pos);
        Gps {
            timestamp: now,
            pos_eci: pos,
            vel_eci: vel,
            lla: V3::from_buf([lat.to_degrees(), lon.to_degrees(), alt]),
        }
    }

    /// Logs each entry into and exit from Earth shadow, passing `illuminated` through.
    fn log_shadow_edges(&mut self, illuminated: bool) -> bool {
        match (self.was_illuminated, illuminated) {
            (Some(true), false) => tracing::info!(t_sim = self.t_sim, "entering Earth shadow"),
            (Some(false), true) => tracing::info!(t_sim = self.t_sim, "leaving Earth shadow"),
            _ => {}
        }
        self.was_illuminated = Some(illuminated);
        illuminated
    }

    /// Returns the magnetorquer torque at the start of the step.
    fn mtq_torque(&self, s: &Sample, dipole_b: V3) -> V3 {
        clamp_dipole(dipole_b, self.params.mtq_max_dipole).cross(&s.mag_b_true)
    }

    /// Writes the cycle's measurements and truth telemetry.
    fn telemeter(
        &self,
        now: Timestamp,
        s: &Sample,
        d: &DisturbanceTorques,
        mtq_torque_b: V3,
        ports: Ports<'_>,
    ) {
        let Ports {
            sensors,
            gps,
            wheels,
            body,
            world,
            disturb,
        } = ports;
        let _ = sensors.write(&s.sensors);
        let _ = gps.write(&s.gps);
        let t = &s.truth;
        let _ = world.write(&World::new(now, t.sun_eci, t.mag_eci, t.illuminated));
        let _ = wheels.write(&Wheels {
            timestamp: now,
            wheels: self.wheels,
        });
        let _ = body.write(&BodyState {
            timestamp: now,
            q_b_eci: t.q_b_eci,
            omega_b: s.omega_b_true,
            pos_eci: t.pos_eci,
            vel_eci: t.vel_eci,
        });
        let _ = disturb.write(&Disturbances {
            timestamp: now,
            gg_b: d.gg_b,
            aero_b: d.aero_b,
            mag_b: d.mag_b,
            srp_b: d.srp_b,
            mtq_b: mtq_torque_b,
            total_b: d.total_b(),
        });
    }

    /// Integrates the body one step under every applied torque.
    fn integrate(&mut self, act: &Actuation, d: &DisturbanceTorques, mtq_torque_b: V3) {
        let tau_body = act.rw_torque_b + d.total_b() + mtq_torque_b;
        let h_w_mid = act.h_w_b + act.rw_torque_b * (DT / 2.0);
        self.body = propagate(self.body.clone(), tau_body, h_w_mid, d.aero_force_eci);
        self.t_sim += DT;
    }

    fn noise(&mut self, sigma: f64) -> V3 {
        let (x, y, z) = (self.noise1(sigma), self.noise1(sigma), self.noise1(sigma));
        tensor![x, y, z]
    }

    /// Draws zero-mean noise with deviation `sigma`.
    fn noise1(&mut self, sigma: f64) -> f64 {
        sigma * self.unit_normal.sample(&mut self.rng)
    }
}

#[system]
/// Steps the wheels, samples the sensors, writes telemetry, and integrates the body.
#[allow(clippy::too_many_arguments)]
pub fn plant(
    #[state] state: &mut PlantState,
    now: Timestamp,
    torque_cmd: &mut Input<TorqueCmd>,
    mtq_cmd: &mut Input<MtqCmd>,
    sensors: &mut Output<Sensors>,
    gps: &mut Output<Gps>,
    wheels: &mut Output<Wheels>,
    body: &mut Output<BodyState>,
    world: &mut Output<World>,
    disturb: &mut Output<Disturbances>,
) {
    let act = state.actuate(torque_cmd, mtq_cmd);
    let sample = state.sense(now);
    let d = DisturbanceTorques::at(&state.params, &sample.truth);
    let mtq_torque_b = state.mtq_torque(&sample, act.dipole_b);
    let ports = Ports {
        sensors,
        gps,
        wheels,
        body,
        world,
        disturb,
    };
    state.telemeter(now, &sample, &d, mtq_torque_b, ports);
    state.integrate(&act, &d, mtq_torque_b);
}

#[cfg(test)]
mod tests {
    use adcs_contracts::{ALTITUDE, MTQ_MAX_DIPOLE, RW_TORQUE_MAX, detumble_dipole};

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

    /// Total angular momentum `q ⊛ (I∘ω_b + h_w)` holds while the wheels trade
    /// momentum with the body. Gravity is central and wheel torques are internal, so
    /// only integrator error moves it.
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
                // Out-of-phase sinusoids keep every wheel trading momentum.
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
        // Truncation drifts about 1e-5 relative over this run, like a 4e-10 N·m
        // parasitic torque. Bookkeeping mistakes drift at the percent level.
        assert!(drift / l_mag < 5e-5, "L_eci drifted: {}", drift / l_mag);
    }

    /// A wheel driven at full torque pins exactly at the momentum limit, and
    /// unloads on the next step once the command reverses.
    #[test]
    fn rw_saturation_clamps_at_limit_and_allows_unloading() {
        let mut w = ReactionWheel::new(tensor![1.0, 0.0, 0.0], true);
        // A −X body torque spins the wheel up along +X.
        w.torque_set_point = tensor![-RW_TORQUE_MAX, 0.0, 0.0];
        let mut pinned = 0;
        for _ in 0..5000 {
            w.update();
            let s: f64 = w.ang_momentum.into_buf()[0];
            assert!(
                s <= RW_MOMENTUM_MAX + 1e-12,
                "momentum exceeded the limit: {s}"
            );
            if s >= RW_MOMENTUM_MAX - 1e-12 {
                pinned += 1;
            }
        }
        assert!(pinned > 0, "wheel never reached the momentum limit");
        let t: f64 = w.torque.into_buf()[0];
        assert!(t.abs() < 1e-12, "pinned wheel still delivered torque: {t}");
        w.torque_set_point = tensor![RW_TORQUE_MAX, 0.0, 0.0];
        w.update();
        let t: f64 = w.torque.into_buf()[0];
        assert!(
            t > 1e-3,
            "unloading torque did not flow from saturation: {t}"
        );
        let s: f64 = w.ang_momentum.into_buf()[0];
        assert!(s < RW_MOMENTUM_MAX, "momentum did not unload");
    }

    /// Friction alone slows a spinning wheel to a stop without reversing it, then
    /// holds it inside the stiction deadband.
    #[test]
    fn rw_friction_decays_signed_speed() {
        let mut w = ReactionWheel::new(tensor![0.0, 1.0, 0.0], true).with_momentum(0.001);
        let mut prev = w.speed;
        assert!(prev > 0.0);
        let mut frozen = 0;
        for _ in 0..30_000 {
            w.update();
            assert!(w.speed <= prev + 1e-15, "friction sped the wheel up");
            assert!(
                w.speed >= 0.0,
                "friction counter-rotated the wheel: {}",
                w.speed
            );
            if w.speed == prev {
                frozen += 1;
            } else {
                frozen = 0;
            }
            prev = w.speed;
        }
        assert!(frozen > 100, "wheel never froze");
        assert!(
            prev < RW_STICTION_OMEGA,
            "froze outside the deadband: {prev}"
        );
    }

    /// Each disturbance at 400 km falls in its expected order of magnitude, which
    /// catches unit mistakes.
    #[test]
    fn disturbance_torques_sane_at_400km() {
        let p = PlantParams {
            init_angle: 0.0,
            init_rate: 0.0,
            meas_sigma: 0.0,
            seed: 0,
            disarmed: false,
            rho: 3e-12,
            cd: 2.2,
            area_aero: 0.03,
            cp_offset_b: [0.02, 0.0, 0.0],
            m_res_b: [0.002, 0.002, 0.002],
            area_srp: 0.03,
            cr: 1.5,
            mtq_max_dipole: MTQ_MAX_DIPOLE,
            init_wheel_h: 0.0,
            altitude: ALTITUDE,
            inclination: 0.0,
            ltan_hours: 12.0,
            init_orbit_phase: 0.0,
        };
        let radius = EARTH_RADIUS + ALTITUDE;
        let pos: V3 = tensor![radius, 0.0, 0.0];
        let v_orbit = (MU / radius).sqrt();
        let vel: V3 = tensor![0.0, v_orbit, 0.0];
        // Off every principal axis and not parallel to the field.
        let q = Quat::from_axis_angle(tensor![1.0, 1.0, 1.0], 0.5);
        let epoch = adcs_contracts::target_epoch();
        let mut model = MagneticModel::default();
        let mag = mag_field_eci(&mut model, epoch, &pos);
        let lit = TrueState {
            q_b_eci: q,
            pos_eci: pos,
            vel_eci: vel,
            sun_eci: sun_dir_eci(epoch),
            mag_eci: mag,
            illuminated: true,
        };
        let d = DisturbanceTorques::at(&p, &lit);

        let mag_of = |v: &V3| -> f64 { v.norm().into_buf() };
        let gg = mag_of(&d.gg_b);
        assert!(
            (1e-10..1e-6).contains(&gg),
            "gravity-gradient torque out of band: {gg}"
        );
        let aero = mag_of(&d.aero_b);
        assert!(
            (1e-9..1e-5).contains(&aero),
            "aero torque out of band: {aero}"
        );
        let res = mag_of(&d.mag_b);
        assert!(
            (1e-10..1e-5).contains(&res),
            "residual-dipole torque out of band: {res}"
        );
        let srp = mag_of(&d.srp_b);
        assert!(
            (1e-11..1e-7).contains(&srp),
            "SRP torque out of band: {srp}"
        );
        let drag_along_v: f64 = d.aero_force_eci.dot(&vel).into_buf();
        assert!(drag_along_v < 0.0, "drag force does not oppose velocity");
        let sum = d.gg_b + d.aero_b + d.mag_b + d.srp_b;
        assert!(mag_of(&(d.total_b() - sum)) == 0.0);

        // Shadow switches off solar radiation pressure and nothing else.
        let shadow = TrueState {
            illuminated: false,
            ..lit
        };
        let dark = DisturbanceTorques::at(&p, &shadow);
        assert!(mag_of(&dark.srp_b) == 0.0, "SRP must vanish in shadow");
        assert!(mag_of(&(dark.gg_b - d.gg_b)) == 0.0);
        assert!(mag_of(&(dark.aero_b - d.aero_b)) == 0.0);
        assert!(mag_of(&(dark.mag_b - d.mag_b)) == 0.0);
    }

    /// A head square to the sun reads 1, heads facing away read 0, an oblique sun
    /// reads its direction cosines, and eclipse darkens every head.
    #[test]
    fn css_heads_read_cosine_fov_and_eclipse() {
        let sun_x: V3 = tensor![1.0, 0.0, 0.0];
        let r = css_readings(&sun_x, true);
        assert!(
            (r[0] - 1.0).abs() < 1e-12,
            "+X head square to the sun: {}",
            r[0]
        );
        assert_eq!(r[3], 0.0, "−X head is behind its FOV");
        assert_eq!(
            [r[1], r[2], r[4], r[5]],
            [0.0; 4],
            "perpendicular heads read zero"
        );

        let sun: V3 = (tensor![1.0, 1.0, 1.0] as V3).normalize();
        let r = css_readings(&sun, true);
        let c = 1.0 / 3f64.sqrt();
        for (i, reading) in r.iter().enumerate().take(3) {
            assert!((reading - c).abs() < 1e-12, "head {i} cosine: {reading}");
        }
        assert_eq!([r[3], r[4], r[5]], [0.0; 3]);

        assert_eq!(css_readings(&sun, false), [0.0; 6]);
    }

    /// The detumble law damps the body rate perpendicular to the field.
    ///
    /// The gain and dipole limit are ten times the defaults to keep the test short.
    /// The orbit turns the field too slowly to reach the parallel component within
    /// this window, so the test checks only the perpendicular one.
    #[test]
    fn detumble_law_damps_tumble() {
        let omega0: V3 = (tensor![1.0, 1.0, 1.0] as V3).normalize() * 0.1;
        let mut body = orbit_body(0.9, omega0);
        let mut mag_model = MagneticModel::default();
        let k_detumble = 5e-4; // ten times the default
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
        // The field changes on the orbit timescale, so evaluate it every 100 steps.
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
        assert!(
            perp0 > 0.05,
            "tumble started perpendicular-degenerate: {perp0}"
        );
        assert!(perp < 5e-3, "detumble failed to damp ω_⊥: {perp0} → {perp}");
    }
}
