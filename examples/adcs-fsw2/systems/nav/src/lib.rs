//! The MEKF **navigation filter** of the `adcs-fsw2` mission, as a `dlopen`-loadable
//! `cdylib` (dl-open.md §3, §8). It wraps [`metor_fsw_adcs::mekf::State`] (a multiplicative
//! EKF) and estimates attitude from the two body-frame vector observations + gyro, modeling
//! the inertial **references** for those observations from the orbit state (cube-sat's
//! `Nav::from_sensors`): the sun is a fixed inertial direction and the magnetic field is the
//! tilted-dipole model evaluated at the GPS position.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use adcs_contracts::{AttitudeEstimate, BodyState, DT, NavParams, Sensors, V3, mag_field_eci};
use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::ring::{Backing, BoxBacking};
use metor_fsw_2::{
    BuildSystem, CyclicSystem, Input, Out, Output, System, SystemInput, SystemOutput,
};
use nox::tensor;

pub struct NavSystem {
    state: metor_fsw_adcs::mekf::State,
    sigma: f64,
}

#[derive(SystemInput)]
pub struct NavIn<B: Backing = BoxBacking> {
    pub sensors: Input<Sensors, B>,
    pub body: Input<BodyState, B>,
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
        Self { state, sigma: p.meas_sigma }
    }
}

impl<B: Backing> System<B> for NavSystem {
    type Input = NavIn<B>;
    type Output = Out<NavOut<B>, B>;
    const NAME: &'static str = "nav";
}

impl<B: Backing> CyclicSystem<B> for NavSystem {
    fn execute(&mut self, now: Timestamp, input: &mut NavIn<B>, o: &mut Self::Output) {
        let Ok(Some(s)) = input.sensors.latest() else {
            return; // no sample yet
        };
        let s = s.get().clone();

        // Model the inertial (ECI) references from the orbit state (cube-sat
        // `Nav::from_sensors`): a fixed sun direction and the dipole field at the position.
        let sun_eci: V3 = tensor![0.0, 0.0, 1.0];
        let mag_eci: V3 = match input.body.latest() {
            Ok(Some(body)) => mag_field_eci(&body.get().pos_eci).normalize(),
            _ => tensor![1.0, 0.0, 0.0],
        };

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
