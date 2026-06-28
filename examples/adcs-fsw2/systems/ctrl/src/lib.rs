//! The Yang-LQR **controller** of the `adcs-fsw2` mission, as a `dlopen`-loadable `cdylib`
//! (dl-open.md §3, §8). The `impl CyclicSystem` is unchanged from the monolith; it wraps
//! [`metor_fsw_adcs::yang_lqr::YangLQR`] and produces the body torque that drives the
//! spacecraft toward the target attitude — the feedback back-edge into the plant.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use adcs_contracts::{AttitudeEstimate, CtrlParams, Quat, TorqueCmd, inertia_diag};
use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::ring::{Backing, BoxBacking};
use metor_fsw_2::{
    BuildSystem, CyclicSystem, Input, Out, Output, System, SystemInput, SystemOutput,
};
use nox::{Quaternion, tensor};

pub struct CtrlSystem {
    lqr: metor_fsw_adcs::yang_lqr::YangLQR,
    target: Quat,
}

#[derive(SystemInput)]
pub struct CtrlIn<B: Backing = BoxBacking> {
    pub estimate: Input<AttitudeEstimate, B>,
}

#[derive(SystemOutput)]
pub struct CtrlOut<B: Backing = BoxBacking> {
    pub torque: Output<TorqueCmd, B>,
}

impl CtrlSystem {
    pub fn new(p: CtrlParams) -> Self {
        let q = tensor![p.q_weight, p.q_weight, p.q_weight];
        let r = tensor![p.r_weight, p.r_weight, p.r_weight];
        let lqr = metor_fsw_adcs::yang_lqr::YangLQR::new(inertia_diag(), q, q, r);
        Self { lqr, target: Quaternion::identity() }
    }
}

impl<B: Backing> System<B> for CtrlSystem {
    type Input = CtrlIn<B>;
    type Output = Out<CtrlOut<B>, B>;
    const NAME: &'static str = "ctrl";
}

impl<B: Backing> CyclicSystem<B> for CtrlSystem {
    fn execute(&mut self, now: Timestamp, input: &mut CtrlIn<B>, o: &mut Self::Output) {
        let Ok(Some(e)) = input.estimate.latest() else {
            return;
        };
        let e = e.get();
        let q_hat = e.q_hat.clone();
        // Body rate rotated into the world frame, matching the cube-sat recipe.
        let ang_vel = &q_hat * e.omega;
        let torque = self.lqr.control(q_hat, ang_vel, self.target.clone());

        let _ = o.torque.write(&TorqueCmd {
            timestamp: now,
            torque,
        });
    }
}

impl BuildSystem for CtrlSystem {
    type Params = CtrlParams;
    fn new(params: Self::Params) -> Self {
        CtrlSystem::new(params)
    }
}

#[cfg(feature = "export")]
metor_fsw_2::export_system!(CtrlSystem);
