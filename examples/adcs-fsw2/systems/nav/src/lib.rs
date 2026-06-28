//! The MEKF **navigation filter** of the `adcs-fsw2` mission, as a `dlopen`-loadable
//! `cdylib` (dl-open.md §3, §8). The `impl CyclicSystem` is unchanged from the monolith;
//! it wraps [`metor_fsw_adcs::mekf::State`] (a multiplicative EKF) and estimates attitude
//! from the two vector observations + gyro.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use adcs_contracts::{AttitudeEstimate, DT, NavParams, Sensors};
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

        self.state.omega = s.gyro;
        self.state = self.state.clone().estimate_attitude(
            [s.sun_body, s.mag_body],
            [s.sun_ref, s.mag_ref],
            [self.sigma, self.sigma],
        );
        self.state.reset_if_invalid();

        let _ = o.estimate.write(&AttitudeEstimate {
            timestamp: now,
            q_hat: self.state.q_hat.clone(),
            omega: s.gyro, // pass the measured body rate through to the controller
            b_hat: self.state.b_hat,
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
