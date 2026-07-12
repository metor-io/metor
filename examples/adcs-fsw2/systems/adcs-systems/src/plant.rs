//! The rigid-body **plant** of the `adcs-fsw2` mission. It propagates a real 400 km orbit
//! (gravity + drag) and the full Euler attitude dynamics — including the gyroscopic coupling
//! of the stored reaction-wheel momentum — under the environmental disturbance torques a
//! small spacecraft actually lives on (gravity gradient, aero drag about the CP–CG offset,
//! residual magnetic dipole, SRP), driven by two actuators: three reaction wheels (bearing
//! friction + momentum-saturation foldback, both inside the dynamics) and three
//! magnetorquers (`τ = m × B`, the desat / detumble authority).
//!
//! Each cycle it steps the wheels under the commanded control torque, samples the simulated
//! sensor suite (gyro, six coarse-sun-sensor heads — dark in Earth shadow — and a
//! Tesla-valued magnetometer reading the NOAA WMM field), emits the noisy `gps` measurement
//! the flight software flies on (a Gauss-Markov position error + white velocity noise), the
//! wheel and disturbance telemetry, the true `world` environment (including the
//! illumination truth flag), and the `body` truth frame the host taps to measure
//! convergence — then integrates the body one step (`six_dof_rk4`, body-fixed torques
//! rotated per substep, the wheel-momentum coupling `−ω_b × h_w` evaluated per substep).
//!
//! Fn-authored (docs/design-packs-authoring.md §4): [`PlantState`] is a plain struct,
//! [`plant_init`] the `PlantParams -> PlantState` constructor, and [`plant_execute`]'s
//! signature the port set — the crate's [`pack`](crate::pack) registers all three.

use adcs_contracts::{
    ALTITUDE, BodyState, DT, Disturbances, EARTH_RADIUS, Gps, MASS, MU, MagneticModel, MtqCmd,
    PlantParams, Quat, RW_MOMENTUM_MAX, RW_TORQUE_MAX, ReactionWheel, Sensors, TorqueCmd, V3,
    Wheels, World, clamp_dipole, epoch_at, in_earth_shadow, inertia_diag, mag_field_eci,
    sun_dir_eci,
};
use metor_fsw_2::{Input, Output, Timestamp};
use nox::{
    Body, Quaternion, SpatialForce, SpatialInertia, SpatialMotion, SpatialTransform, six_dof_rk4,
    tensor,
};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, Normal};

// --- The plant's own physics --------------------------------------------------
// Simulation math and its tuning live here, with the system that integrates them; the
// contract carries only the wire shapes and the actuator envelope both sides agree on
// (RW_TORQUE_MAX / RW_MOMENTUM_MAX / MTQ_MAX_DIPOLE).

// GPS error model (first-order Gauss-Markov position, white velocity): the "good" basic GPS
// model (Brown & Hwang) — position error is dominated by slowly-varying iono/ephemeris terms,
// so it is exponentially correlated rather than white; velocity from Doppler/carrier is far
// less correlated, so it is white. Spaceborne single-frequency SPS figures.

/// GPS position error 1-sigma per axis (m) — the Gauss-Markov steady-state.
pub const GPS_POS_SIGMA: f64 = 5.0;
/// GPS position-error correlation time (s) — the Gauss-Markov time constant.
pub const GPS_TAU: f64 = 100.0;
/// GPS velocity error 1-sigma per axis (m/s), modeled as white noise.
pub const GPS_VEL_SIGMA: f64 = 0.05;

/// Magnetometer noise 1-sigma per axis (Tesla) — the sensor reads the physical field, so its
/// noise is physical too (~150 nT against a ~25–45 µT field at 400 km).
pub const MAG_SENSOR_SIGMA: f64 = 150e-9;

/// Solar radiation pressure at 1 AU (N/m²).
pub const P_SRP: f64 = 4.56e-6;

/// The six CSS head normals, one photodiode per body face in the frame's documented order
/// `[+X, +Y, +Z, −X, −Y, −Z]` (cube-sat's arrangement). Six orthogonal 90° cones cover the
/// full sphere, so sun availability reduces to illumination.
const CSS_NORMALS: [[f64; 3]; 6] = [
    [1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, 0.0, 1.0],
    [-1.0, 0.0, 0.0],
    [0.0, -1.0, 0.0],
    [0.0, 0.0, -1.0],
];

/// The noiseless CSS head readings for a true body-frame sun direction: per head the cosine
/// response `n̂·ŝ_b`, clamped at zero (the 90° half-angle FOV) and gated by illumination —
/// an eclipsed head reads nothing. The plant adds each head's noise floor on top.
pub fn css_readings(sun_b: &V3, illuminated: bool) -> [f64; 6] {
    let lit = if illuminated { 1.0 } else { 0.0 };
    CSS_NORMALS.map(|n| {
        let cos: f64 = V3::from_buf(n).dot(sun_b).into_buf();
        cos.max(0.0) * lit
    })
}

/// Wheel rotor moment of inertia (kg·m²): a 185 g disc of 5 cm diameter.
pub const RW_MOI: f64 = 0.185 * (0.05 / 2.0) * (0.05 / 2.0) / 2.0;
/// Coulomb bearing friction (N·m). The cube-sat's 5e-4 was telemetry-only; fed into the
/// dynamics it (with its viscous partner) would cap the wheel near 40 rad/s — 17× below the
/// ~692 rad/s saturation speed — so both coefficients are retuned to bearing-realistic values.
pub const RW_COULOMB: f64 = 1.0e-5;
/// Viscous bearing friction (N·m·s/rad).
pub const RW_VISCOUS: f64 = 1.0e-7;
/// Stiction deadband (rad/s): below this speed friction holds the wheel at rest instead of
/// chattering across zero (`signum(0)` would otherwise apply a phantom torque at rest).
pub const RW_STICTION_OMEGA: f64 = 1e-3;

/// The wheel dynamics over the shared [`ReactionWheel`] telemetry struct (the struct is the
/// wire contract and lives in `adcs-contracts`; how it evolves is this plant's physics).
pub trait WheelDynamics {
    /// The same wheel preloaded with `h` N·m·s of stored momentum along its axis — how the
    /// desaturation tests/demos start with something to dump.
    fn with_momentum(self, h: f64) -> Self;

    /// Advance the wheel one step. The motor drives against the commanded body torque
    /// (`τm = −u`, clamped to ±[`RW_TORQUE_MAX`], zero when disarmed); bearing friction
    /// opposes the spin (Coulomb + viscous, a stiction deadband at rest, and it may stop the
    /// wheel within a step but never reverse it); the momentum-saturation foldback pins
    /// `|s| ≤ RW_MOMENTUM_MAX` exactly while always letting unloading torque flow. The
    /// reaction `−ḣ·axis` is what the body receives.
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
            // Friction may stop the wheel within the step, never push it through zero.
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

/// The disturbance torques at one true state, plus the drag force (which also perturbs the
/// orbit). All torques in the body frame (N·m); the force in ECI (N).
pub struct DisturbanceTorques {
    /// Gravity-gradient torque.
    pub gg_b: V3,
    /// Aerodynamic drag torque about the CP–CG offset.
    pub aero_b: V3,
    /// Residual-magnetic-dipole torque `m_res × B`.
    pub mag_b: V3,
    /// Solar-radiation-pressure torque about the CP–CG offset.
    pub srp_b: V3,
    /// The drag force in ECI — fed into the translational dynamics.
    pub aero_force_eci: V3,
}

impl DisturbanceTorques {
    /// The summed disturbance torque (excludes actuation — the MTQ torque is control).
    pub fn total_b(&self) -> V3 {
        self.gg_b + self.aero_b + self.mag_b + self.srp_b
    }
}

/// Evaluate the disturbance environment at a true state: attitude `q_b_eci`, ECI
/// position/velocity, the sun direction, the **un-normalized** magnetic field (Tesla), and
/// whether the spacecraft is illuminated. All deterministic — no RNG.
///
/// - Gravity gradient: `τ = (3μ/|r|³) · r̂_b × (I ∘ r̂_b)`.
/// - Aero: `F = −½ρ·Cd·A·|v|·v` (co-rotating atmosphere ignored), `τ = r_cp × F_b`.
/// - Residual dipole: `τ = m_res × B_b`.
/// - SRP: `F = −P·A·Cr·ŝ_b`, `τ = r_cp × F_b` — zero in Earth shadow.
pub fn disturbance_torques(
    p: &PlantParams,
    q_b_eci: &Quat,
    pos_eci: &V3,
    vel_eci: &V3,
    sun_eci: &V3,
    mag_eci: &V3,
    illuminated: bool,
) -> DisturbanceTorques {
    let q_eci_b = q_b_eci.inverse();
    let cp_offset_b = V3::from_buf(p.cp_offset_b);

    let r_mag: f64 = pos_eci.norm().into_buf();
    let r_hat_b = &q_eci_b * (pos_eci.normalize());
    let gg_b = r_hat_b.cross(&(inertia_diag() * r_hat_b)) * (3.0 * MU / r_mag.powi(3));

    let v_mag: f64 = vel_eci.norm().into_buf();
    let aero_force_eci = *vel_eci * (-0.5 * p.rho * p.cd * p.area_aero * v_mag);
    let aero_b = cp_offset_b.cross(&(&q_eci_b * aero_force_eci));

    let mag_b = V3::from_buf(p.m_res_b).cross(&(&q_eci_b * *mag_eci));

    let srp_force_b = if illuminated {
        (&q_eci_b * sun_eci.normalize()) * (-P_SRP * p.area_srp * p.cr)
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

/// The rigid-body plant's state: an orbiting spacecraft whose attitude is driven by three
/// reaction wheels and three magnetorquers against the disturbance environment, emitting a
/// noisy sensor suite. The wheels are the shared [`ReactionWheel`] contract type — the same
/// struct the `wheels` telemetry frame carries.
pub struct PlantState {
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

impl PlantState {
    fn noise(&mut self, sigma: f64) -> V3 {
        let d = Normal::new(0.0, sigma).unwrap();
        tensor![
            d.sample(&mut self.rng),
            d.sample(&mut self.rng),
            d.sample(&mut self.rng)
        ]
    }

    fn noise1(&mut self, sigma: f64) -> f64 {
        Normal::new(0.0, sigma).unwrap().sample(&mut self.rng)
    }
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
        SpatialForce::from_torque(q * tau_b) + SpatialForce::from_linear(gravity(b) + f_drag_eci)
    })
}

impl PlantState {
    /// Build the plant's state from its params: a 400 km circular orbit, booted
    /// `init_orbit_phase` radians around it — phase zero is the classic +X position / +Y
    /// velocity (cube-sat `CubeSat::default`), and the eclipse tests crank the phase to start in
    /// Earth shadow — with the body rotated `init_angle` about [1,1,1] from the (identity)
    /// reference and a small initial tumble about the same axis.
    pub fn new(p: PlantParams) -> PlantState {
        let radius = EARTH_RADIUS + ALTITUDE;
        let v_orbit = (MU / radius).sqrt();
        let (sin_th, cos_th) = p.init_orbit_phase.sin_cos();
        let pos0 = tensor![cos_th, sin_th, 0.0] * radius;
        let vel0 = tensor![-sin_th, cos_th, 0.0] * v_orbit;

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
            gps_pos_err: V3::zeros(),
            params: p,
        }
    }
}

/// One plant cycle: step the wheels, sample the sensors, publish the telemetry, integrate.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    state: &mut PlantState,
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
        Some(cmd) => cmd.torque_b,
        None => V3::zeros(),
    };
    let dipole_b: V3 = match mtq.latest() {
        Some(cmd) => cmd.dipole_b,
        None => V3::zeros(),
    };
    for wheel in &mut state.wheels {
        wheel.torque_set_point = wheel.axis.dot(&torque_b) * wheel.axis;
        wheel.update();
    }
    let rw_torque_b: V3 = state
        .wheels
        .iter()
        .fold(V3::zeros(), |acc, w| acc + w.torque);
    let h_w_b: V3 = state
        .wheels
        .iter()
        .fold(V3::zeros(), |acc, w| acc + w.ang_momentum);

    // Sample the sensors off the CURRENT body (cube-sat samples before integrating).
    let q_b_eci = state.body.pos.angular();
    let q_eci_b = q_b_eci.inverse();
    // Gyro: ECI rate brought into the body frame, plus a slow bias walk.
    let omega_b_true = q_eci_b * state.body.vel.angular();
    let meas_sigma = state.params.meas_sigma;
    state.bias = state.bias + state.noise(meas_sigma * 1e-2);
    let gyro_b = omega_b_true + state.bias + state.noise(meas_sigma);
    // The true ECI environment at this epoch/position: the real sun direction (nox-frames
    // Vallado model) and the NOAA WMM magnetic field, both at the deterministic mission epoch.
    let epoch = epoch_at(state.t_sim);
    let pos_eci = state.body.pos.linear();
    let vel_eci = state.body.vel.linear();
    let sun_eci = sun_dir_eci(epoch);
    let mag_eci = mag_field_eci(&mut state.mag_model, epoch, &pos_eci);
    let illuminated = !in_earth_shadow(&pos_eci, &sun_eci);
    // The six CSS heads (cosine response, FOV-clamped, dark in eclipse) each read their
    // noise floor on top; the magnetometer reads the physical field (Tesla) — real
    // magnetometers measure magnitude, and the desat law downstream needs |B|.
    let sun_b_true = (q_eci_b * sun_eci).normalize();
    let mut css = css_readings(&sun_b_true, illuminated);
    for reading in &mut css {
        *reading += state.noise1(meas_sigma);
    }
    let mag_b_true = q_eci_b * mag_eci;
    let mag_b = mag_b_true + state.noise(MAG_SENSOR_SIGMA);

    // The GPS measurement: the true orbit state corrupted by the GPS error model. Position
    // error is a first-order Gauss-Markov process (exponentially correlated); velocity error
    // is white. These RNG draws come AFTER the sensor draws above so the sensor noise stays
    // byte-identical regardless of the GPS model.
    let gps_phi = (-DT / GPS_TAU).exp();
    let gps_drive_sigma = GPS_POS_SIGMA * (1.0 - gps_phi * gps_phi).sqrt();
    state.gps_pos_err = gps_phi * state.gps_pos_err + state.noise(gps_drive_sigma);
    let gps_pos_eci = pos_eci + state.gps_pos_err;
    let gps_vel_eci = vel_eci + state.noise(GPS_VEL_SIGMA);

    // The disturbance environment at the pre-step true state, and the magnetorquer
    // torque (the commanded dipole clamped to the torquer's authority, crossed with the
    // true field). All deterministic — no RNG draws.
    let d = disturbance_torques(
        &state.params,
        &q_b_eci,
        &pos_eci,
        &vel_eci,
        &sun_eci,
        &mag_eci,
        illuminated,
    );
    let mtq_torque_b = clamp_dipole(dipole_b, state.params.mtq_max_dipole).cross(&mag_b_true);

    sensors.publish(&Sensors {
        timestamp: now,
        gyro_b,
        css,
        mag_b,
    });
    gps.publish(&Gps {
        timestamp: now,
        pos_eci: gps_pos_eci,
        vel_eci: gps_vel_eci,
    });
    world.publish(&World::new(now, sun_eci, mag_eci, illuminated));
    // Per-wheel telemetry — the wheels themselves, the same structs the plant integrates.
    wheels.publish(&Wheels {
        timestamp: now,
        wheels: state.wheels.clone(),
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
    state.body = propagate(state.body.clone(), tau_body, h_w_mid, d.aero_force_eci);
    // Advance the deterministic mission clock (drives the sun epoch — never wall time).
    state.t_sim += DT;
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

    /// Driven at the full set point, the wheel pins **exactly** at `RW_MOMENTUM_MAX` (the
    /// foldback lands on the limit rather than snapping torque to zero early), never exceeds
    /// it, and an unloading command flows immediately from the pinned state.
    #[test]
    fn rw_saturation_clamps_at_limit_and_allows_unloading() {
        let mut w = ReactionWheel::new(tensor![1.0, 0.0, 0.0], true);
        // A constant −X body-torque command spins the wheel up along +X.
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
        // Reverse the command: unloading torque must flow on the very next step.
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

    /// With no motor command, bearing friction decays a preloaded wheel monotonically, stops
    /// it without ever pushing it through zero into counter-rotation, and holds it frozen
    /// inside the stiction deadband (regression for the unsigned-speed `signum` friction and
    /// the phantom torque at rest).
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

    /// The disturbance model lands each source in its expected order-of-magnitude band at a
    /// 400 km state with honest params — a units guard (ρ, Tesla, P_SRP, torque arms), like
    /// contracts' WMM band test.
    #[test]
    fn disturbance_torques_sane_at_400km() {
        // The mission's honest environment (`mission.kdl` spells the same values out).
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
            init_orbit_phase: 0.0,
        };
        let radius = EARTH_RADIUS + ALTITUDE;
        let pos: V3 = tensor![radius, 0.0, 0.0];
        let v_orbit = (MU / radius).sqrt();
        let vel: V3 = tensor![0.0, v_orbit, 0.0];
        // A generic attitude so nothing sits on a principal axis or parallel to the field.
        let q = Quat::from_axis_angle(tensor![1.0, 1.0, 1.0], 0.5);
        let epoch = adcs_contracts::mission_epoch();
        let mut model = MagneticModel::default();
        let mag = mag_field_eci(&mut model, epoch, &pos);
        let d = disturbance_torques(&p, &q, &pos, &vel, &sun_dir_eci(epoch), &mag, true);

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
        // Drag opposes the velocity.
        let drag_along_v: f64 = d.aero_force_eci.dot(&vel).into_buf();
        assert!(drag_along_v < 0.0, "drag force does not oppose velocity");
        let sum = d.gg_b + d.aero_b + d.mag_b + d.srp_b;
        assert!(mag_of(&(d.total_b() - sum)) == 0.0);

        // In Earth shadow only SRP switches off; everything else is identical.
        let dark = disturbance_torques(&p, &q, &pos, &vel, &sun_dir_eci(epoch), &mag, false);
        assert!(mag_of(&dark.srp_b) == 0.0, "SRP must vanish in shadow");
        assert!(mag_of(&(dark.gg_b - d.gg_b)) == 0.0);
        assert!(mag_of(&(dark.aero_b - d.aero_b)) == 0.0);
        assert!(mag_of(&(dark.mag_b - d.mag_b)) == 0.0);
    }

    /// The CSS head model: a face square to the sun reads ~1, the opposed face reads zero
    /// (FOV clamp), an oblique sun spreads its cosine across the facing heads, and eclipse
    /// darkens all six.
    #[test]
    fn css_heads_read_cosine_fov_and_eclipse() {
        // Sun along +X: head 0 (+X) reads 1, head 3 (−X) is FOV-clamped to 0.
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

        // An oblique sun: the three facing heads read its direction cosines.
        let sun: V3 = (tensor![1.0, 1.0, 1.0] as V3).normalize();
        let r = css_readings(&sun, true);
        let c = 1.0 / 3f64.sqrt();
        for (i, reading) in r.iter().enumerate().take(3) {
            assert!((reading - c).abs() < 1e-12, "head {i} cosine: {reading}");
        }
        assert_eq!([r[3], r[4], r[5]], [0.0; 3]);

        // Eclipse: every head is dark regardless of geometry.
        assert_eq!(css_readings(&sun, false), [0.0; 6]);
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
        assert!(
            perp0 > 0.05,
            "tumble started perpendicular-degenerate: {perp0}"
        );
        assert!(perp < 5e-3, "detumble failed to damp ω_⊥: {perp0} → {perp}");
    }
}
