//! The **controller** of the `adcs-fsw2` mission. It commands both actuators:
//!
//! - **Reaction wheels** ([`TorqueCmd`]): the Yang-LQR body torque toward the pointing-law
//!   target the `mode` slot commands (`ModeCmd.law` — nadir or velocity-vector/HIL), computed
//!   from the **GPS** orbit measurement and clamped per axis to the wheel motor-torque limit
//!   an FSW knows its actuator has. Under `LAW_DETUMBLE` the wheels idle.
//! - **Magnetorquers** ([`MtqCmd`]): momentum desaturation (`k·(h_w × B)/|B|²`, always on
//!   outside detumble — negligible when the wheels are empty) or B-cross rate damping in
//!   detumble, both from the **measured** magnetometer field (Tesla) and the telemetered
//!   wheel momentum — the controller never sees the plant's truth.
//!
//! Before any `ModeCmd` arrives it holds the identity reference.
//!
//! Deliberately `#[system]` struct-authored (docs/design-system-macro.md) while its
//! packmates are fn-authored: the port set is the `execute` signature, the bundles/trait
//! impls/`BuildSystem` are generated, and the crate's [`pack`](crate::pack) lowers it in
//! via `system_type` — the example suite keeps both authoring styles exercised.

use adcs_contracts::{
    AttitudeEstimate, CtrlParams, Gps, MTQ_MAX_DIPOLE, ModeCmd, MtqCmd, RW_TORQUE_MAX, Sensors,
    TorqueCmd, V3, Wheels, clamp_dipole, desat_dipole, detumble_dipole, inertia_diag, target_for,
};
use metor_fsw_2::{Input, Output, Timestamp, system};
use nox::{Quaternion, tensor};

pub struct CtrlSystem {
    lqr: metor_fsw_adcs::yang_lqr::YangLQR,
    /// The latest commanded pointing law (`None` until the first `ModeCmd`).
    law: Option<u8>,
    k_desat: f64,
    k_detumble: f64,
}

#[system(name = "ctrl")]
impl CtrlSystem {
    pub fn new(p: CtrlParams) -> Self {
        let q = tensor![p.q_weight, p.q_weight, p.q_weight];
        let r = tensor![p.r_weight, p.r_weight, p.r_weight];
        let lqr = metor_fsw_adcs::yang_lqr::YangLQR::new(inertia_diag(), q, q, r);
        Self {
            lqr,
            law: None,
            k_desat: p.k_desat,
            k_detumble: p.k_detumble,
        }
    }

    fn execute(
        &mut self,
        now: Timestamp,
        estimate: &mut Input<AttitudeEstimate>,
        gps: &mut Input<Gps>,
        mode: &mut Input<ModeCmd>,
        sensors: &mut Input<Sensors>,
        wheels: &mut Input<Wheels>,
        torque: &mut Output<TorqueCmd>,
        mtq: &mut Output<MtqCmd>,
    ) {
        let Some(e) = estimate.latest() else {
            return;
        };
        let q_hat_b_eci = e.q_hat_b_eci;

        // Latch the commanded pointing law (the slot may not have written one yet).
        if let Some(m) = mode.latest() {
            self.law = Some(m.law);
        }

        // What the magnetorquer laws run on: the measured field (Tesla) and the telemetered
        // wheel momentum. Either not yet arrived ⇒ zero dipole.
        let mag_b = sensors.latest().map(|s| s.mag_b);
        let h_w_b = wheels.latest().map(|w| {
            w.wheels
                .iter()
                .fold(V3::zeros(), |acc, wheel| acc + wheel.ang_momentum)
        });

        // Detumble: the wheels idle while B-cross bleeds the measured body rate.
        if self.law == Some(ModeCmd::LAW_DETUMBLE) {
            let dipole_b = match &mag_b {
                Some(b) => {
                    clamp_dipole(detumble_dipole(self.k_detumble, &e.omega_b, b), MTQ_MAX_DIPOLE)
                }
                None => V3::zeros(),
            };
            torque.publish(&TorqueCmd { timestamp: now, torque_b: V3::zeros() });
            mtq.publish(&MtqCmd { timestamp: now, dipole_b });
            return;
        }

        // Select the target attitude from the law + the current GPS orbit measurement; identity
        // until a law and a GPS fix are both available.
        let target = match (self.law, gps.latest()) {
            (Some(law), Some(gps)) => target_for(law, &gps.pos_eci, &gps.vel_eci),
            _ => Quaternion::identity(),
        };

        // The Yang-LQR recipe lives in the inertial frame — a left error quaternion
        // (`goal ⊗ q̂⁻¹`) and the ECI-frame rate — so its torque comes back in ECI and must be
        // rotated into the body frame the wheels actually torque in. Then clamp per axis to
        // the wheel motor limit (the wheels are axis-aligned, so per-axis is per-wheel).
        let ang_vel = q_hat_b_eci * e.omega_b;
        let torque_eci = self.lqr.control(q_hat_b_eci, ang_vel, target);
        let torque_b = q_hat_b_eci.inverse() * torque_eci;
        let torque_b =
            V3::from_buf(torque_b.into_buf().map(|t| t.clamp(-RW_TORQUE_MAX, RW_TORQUE_MAX)));

        // Desaturation rides along whenever the wheels have momentum to dump.
        let dipole_b = match (&mag_b, &h_w_b) {
            (Some(b), Some(h)) => clamp_dipole(desat_dipole(self.k_desat, h, b), MTQ_MAX_DIPOLE),
            _ => V3::zeros(),
        };

        torque.publish(&TorqueCmd {
            timestamp: now,
            torque_b,
        });
        mtq.publish(&MtqCmd { timestamp: now, dipole_b });
    }
}
