//! The Yang-LQR **controller** of the `adcs-fsw2` mission, as a `dlopen`-loadable `cdylib`
//! (dl-open.md §3, §8). It wraps [`metor_fsw_adcs::yang_lqr::YangLQR`] and produces the body
//! torque that drives the spacecraft toward its target attitude — the feedback back-edge
//! into the plant. The target is selected by the **pointing law** the `mode` slot commands
//! (`ModeCmd.law`): nadir or velocity-vector, computed from the orbit state (cube-sat's
//! `FSW::nadir_point` / `hil_point`). Before any `ModeCmd` arrives it holds the identity
//! reference.

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use adcs_contracts::{
    AttitudeEstimate, CtrlParams, ModeCmd, OrbitState, TorqueCmd, inertia_diag, target_for,
};
use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::ring::{Backing, BoxBacking};
use metor_fsw_2::{
    BuildSystem, CyclicSystem, Input, Out, Output, System, SystemInput, SystemOutput,
};
use nox::{Quaternion, tensor};

pub struct CtrlSystem {
    lqr: metor_fsw_adcs::yang_lqr::YangLQR,
    /// The latest commanded pointing law (`None` until the first `ModeCmd`).
    law: Option<u8>,
}

#[derive(SystemInput)]
pub struct CtrlIn<B: Backing = BoxBacking> {
    pub estimate: Input<AttitudeEstimate, B>,
    pub orbit: Input<OrbitState, B>,
    pub mode: Input<ModeCmd, B>,
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
        Self { lqr, law: None }
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
        let q_hat = e.q_hat;

        // Latch the commanded pointing law (the slot may not have written one yet).
        if let Ok(Some(m)) = input.mode.latest() {
            self.law = Some(m.get().law);
        }

        // Select the target attitude from the law + the current orbit state; identity until
        // a law and an orbit sample are both available.
        let target = match (self.law, input.orbit.latest()) {
            (Some(law), Ok(Some(orbit))) => {
                let orbit = orbit.get();
                target_for(law, &orbit.pos, &orbit.vel)
            }
            _ => Quaternion::identity(),
        };

        // Body rate rotated into the world frame, matching the cube-sat recipe.
        let ang_vel = q_hat * e.omega;
        let torque = self.lqr.control(q_hat, ang_vel, target);

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
