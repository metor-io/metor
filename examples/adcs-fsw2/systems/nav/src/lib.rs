//! The MEKF **navigation filter** of the `adcs-fsw2` mission, as a `dlopen`-loadable
//! `cdylib` (dl-open.md §3, §8). It wraps [`metor_fsw_adcs::mekf::State`] (a multiplicative
//! EKF) and estimates attitude from the two body-frame vector observations + gyro, modeling
//! the inertial **references** for those observations itself (cube-sat's `Nav::from_sensors`):
//! the sun direction from the Vallado ephemeris at its own sim-time epoch, and the magnetic
//! field from the NOAA WMM evaluated at the **GPS** position. It knows only what the flight
//! software knows — the noisy GPS measurement — not the plant's truth `world` frame.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use adcs_contracts::{
    AttitudeEstimate, DT, Gps, MagneticModel, NavParams, Sensors, V3, epoch_at, mag_field_eci,
    sun_dir_eci,
};
use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::ring::{Backing, BoxBacking};
use metor_fsw_2::{
    BuildSystem, CyclicSystem, Input, Out, Output, System, SystemInput, SystemOutput,
};
use nox::tensor;

pub struct NavSystem {
    state: metor_fsw_adcs::mekf::State,
    sigma: f64,
    /// The NOAA WMM handle for the reference magnetic field (built once — holds C-library state).
    mag_model: MagneticModel,
    /// Seconds of simulated mission time — a deterministic per-cycle counter kept in lockstep
    /// with the plant's (both start at 0 and advance by `DT` each cycle), so nav evaluates its
    /// sun/field references at the same epoch the plant generated the measurement at.
    t_sim: f64,
}

#[derive(SystemInput)]
pub struct NavIn<B: Backing = BoxBacking> {
    pub sensors: Input<Sensors, B>,
    pub gps: Input<Gps, B>,
}

#[derive(SystemOutput)]
pub struct NavOut<B: Backing = BoxBacking> {
    pub estimate: Output<AttitudeEstimate, B>,
}

impl NavSystem {
    pub fn new(p: NavParams) -> Self {
        let state = metor_fsw_adcs::mekf::State::new(
            tensor![0.01, 0.01, 0.01],
            tensor![0.01, 0.01, 0.01],
            DT,
        );
        Self {
            state,
            sigma: p.meas_sigma,
            mag_model: MagneticModel::default(),
            t_sim: 0.0,
        }
    }
}

impl<B: Backing> System<B> for NavSystem {
    type Input = NavIn<B>;
    type Output = Out<NavOut<B>, B>;
    const NAME: &'static str = "nav";
}

impl<B: Backing> CyclicSystem<B> for NavSystem {
    fn execute(&mut self, now: Timestamp, input: &mut NavIn<B>, o: &mut Self::Output) {
        // Advance the deterministic mission clock in lockstep with the plant (every cycle,
        // regardless of whether a sample is ready), so the reference epoch matches the epoch the
        // plant stamped this cycle's measurement at.
        let epoch = epoch_at(self.t_sim);
        self.t_sim += DT;

        let Ok(Some(s)) = input.sensors.latest() else {
            return; // no sensor sample yet
        };
        let s = s.get().clone();
        let Ok(Some(g)) = input.gps.latest() else {
            return; // no GPS fix yet
        };
        let gps_pos: V3 = g.get().pos_eci;

        // The inertial (ECI) references nav models itself from what the flight software knows:
        // the sun direction from the ephemeris at this epoch (a unit vector), and the WMM
        // magnetic field at the GPS position (normalized), NOT the plant's truth `world` frame.
        let sun_eci = sun_dir_eci(epoch);
        let mag_eci = mag_field_eci(&mut self.mag_model, epoch, &gps_pos).normalize();

        self.state.omega = s.gyro_b;
        self.state = self.state.clone().estimate_attitude(
            [s.sun_b, s.mag_b],
            [sun_eci, mag_eci],
            [self.sigma, self.sigma],
        );
        self.state.reset_if_invalid();

        let _ = o.estimate.write(&AttitudeEstimate {
            timestamp: now,
            q_hat_b_eci: self.state.q_hat,
            omega_b: s.gyro_b, // pass the measured body rate through to the controller
            b_hat_b: self.state.b_hat,
        });
    }
}

impl BuildSystem for NavSystem {
    type Params = NavParams;
    fn new(params: Self::Params) -> Self {
        NavSystem::new(params)
    }
}

#[cfg(feature = "export")]
metor_fsw_2::export_system!(NavSystem);
