//! The attitude controller.
//!
//! An LQR drives the reaction wheels toward the target attitude of the commanded
//! pointing law, while the magnetorquers unload the wheels' momentum. Under the
//! detumble law the wheels idle and the magnetorquers damp the body rate.

use adcs_contracts::{
    AttitudeEstimate, CtrlParams, Gps, Law, MTQ_MAX_DIPOLE, ModeCmd, MtqCmd, RW_TORQUE_MAX,
    Sensors, TorqueCmd, V3, Wheels, clamp_dipole, desat_dipole, detumble_dipole, inertia_diag,
    target_for,
};
use metor_fsw_3::{Input, Output, Timestamp, system};
use nox::{Quaternion, tensor};

/// The attitude controller and its magnetorquer gains.
pub struct CtrlSystem {
    lqr: metor_adcs::yang_lqr::YangLQR,
    /// The last commanded pointing law.
    law: Option<Law>,
    k_desat: f64,
    k_detumble: f64,
}

impl CtrlSystem {
    pub fn new(p: CtrlParams) -> Self {
        let q = tensor![p.q_weight, p.q_weight, p.q_weight];
        let r = tensor![p.r_weight, p.r_weight, p.r_weight];
        let lqr = metor_adcs::yang_lqr::YangLQR::new(inertia_diag(), q, q, r);
        Self {
            lqr,
            law: None,
            k_desat: p.k_desat,
            k_detumble: p.k_detumble,
        }
    }
}

#[system]
/// Commands wheel torque toward the pointing law's target and a dipole that
/// unloads the wheels.
#[allow(clippy::too_many_arguments)]
pub fn ctrl(
    #[state] state: &mut CtrlSystem,
    now: Timestamp,
    estimate: &mut Input<AttitudeEstimate>,
    gps: &mut Input<Gps>,
    mode: &mut Input<ModeCmd>,
    sensors: &mut Input<Sensors>,
    wheels: &mut Input<Wheels>,
    torque_cmd: &mut Output<TorqueCmd>,
    mtq_cmd: &mut Output<MtqCmd>,
) {
    let Ok(Some(e)) = estimate.latest() else {
        return;
    };
    let q_hat_b_eci = e.q_hat_b_eci;

    if let Ok(Some(m)) = mode.latest()
        && let Ok(law) = m.mode().map(|mode| mode.law())
        && state.law != Some(law)
    {
        tracing::info!(
            from = state.law.map_or("none", Law::name),
            to = law.name(),
            "pointing law changed"
        );
        state.law = Some(law);
    }

    let mag_b = sensors.latest().ok().flatten().map(|s| s.mag_b);
    let h_w_b = wheels.latest().ok().flatten().map(|w| {
        w.wheels
            .iter()
            .fold(V3::zeros(), |acc, wheel| acc + wheel.ang_momentum)
    });

    // The wheels idle while the magnetorquers damp the rate.
    if state.law == Some(Law::Detumble) {
        let dipole_b = match &mag_b {
            Some(b) => clamp_dipole(
                detumble_dipole(state.k_detumble, &e.omega_b, b),
                MTQ_MAX_DIPOLE,
            ),
            None => V3::zeros(),
        };
        let _ = torque_cmd.write(&TorqueCmd {
            timestamp: now,
            torque_b: V3::zeros(),
        });
        let _ = mtq_cmd.write(&MtqCmd {
            timestamp: now,
            dipole_b,
        });
        return;
    }

    // Identity until both a law and a GPS fix arrive.
    let target = match (state.law, gps.latest().ok().flatten()) {
        (Some(law), Some(gps)) => target_for(law, &gps.pos_eci, &gps.vel_eci),
        _ => Quaternion::identity(),
    };

    // The LQR works in the inertial frame, on the error `goal ⊗ q̂⁻¹` and the ECI
    // rate, so its torque comes back in ECI. The wheels sit on the body axes, so
    // clamping each body axis clamps each wheel.
    let ang_vel = q_hat_b_eci * e.omega_b;
    let torque_eci = state.lqr.control(q_hat_b_eci, ang_vel, target);
    let torque_b = q_hat_b_eci.inverse() * torque_eci;
    let torque_b = V3::from_buf(
        torque_b
            .into_buf()
            .map(|t| t.clamp(-RW_TORQUE_MAX, RW_TORQUE_MAX)),
    );

    let dipole_b = match (&mag_b, &h_w_b) {
        (Some(b), Some(h)) => clamp_dipole(desat_dipole(state.k_desat, h, b), MTQ_MAX_DIPOLE),
        _ => V3::zeros(),
    };

    let _ = torque_cmd.write(&TorqueCmd {
        timestamp: now,
        torque_b,
    });
    let _ = mtq_cmd.write(&MtqCmd {
        timestamp: now,
        dipole_b,
    });
}
