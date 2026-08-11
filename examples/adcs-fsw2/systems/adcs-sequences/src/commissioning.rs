//! The **commissioning** sequence: the `mode` slot's initial occupant, a **condition-based**
//! state machine the slot polls once per cycle, walking the ladder
//!
//! ```text
//! estimator warm-up ──▶ [detumble]* ──▶ coarse pointing ──▶ fine pointing ──▶ Completed
//! ```
//!
//! Every transition is gated on what the FSW observes (the attitude estimate and the GPS
//! orbit state — never truth) through [`check`], which holds each phase's predicate for a
//! dwell and gives up on a budget. The gates and budgets ride [`CommissioningParams`] off
//! the target's `allow` line, which is also how tests patch them.
//!
//! [`ladder`] carries the phases and returns `Result<(), Outcome>`, so a timeout or a
//! cooperative `Abort` unwinds through `?` to the single safing site in
//! [`commissioning`] — the spacecraft is safed in exactly one place rather than at every
//! suspension point.
//!
//! *Detumble (magnetorquer-only rate damping, `LAW_DETUMBLE`) is entered only when the
//! estimated rate is beyond what the reaction wheels should capture — the boot tumble is
//! not, so target runs go straight from warm-up to coarse pointing.

use core::time::Duration;

use adcs_contracts::{AttitudeEstimate, CommissioningParams, Gps, ModeCmd, target_for};
use metor_fsw_2::sequence::{check, now, progress};
use metor_fsw_2::{Input, Outcome, Output, Params};

/// Walk the commissioning ladder, safing the spacecraft on any non-nominal exit.
pub(crate) async fn commissioning(
    mut att: Input<AttitudeEstimate>,
    mut gps: Input<Gps>,
    Params(params): Params<CommissioningParams>,
    mut mode: Output<ModeCmd>,
) -> Outcome {
    match ladder(&mut att, &mut gps, &params, &mut mode).await {
        Ok(()) => {
            progress("commissioned");
            tracing::info!("commissioned");
            Outcome::Completed
        }
        Err(outcome) => {
            mode.publish(&ModeCmd::safe().stamped(now()));
            tracing::warn!(?outcome, "commissioning ended early; safing");
            outcome
        }
    }
}

/// The phases, gated on the live estimate + GPS orbit state.
async fn ladder(
    att: &mut Input<AttitudeEstimate>,
    gps: &mut Input<Gps>,
    params: &CommissioningParams,
    mode: &mut Output<ModeCmd>,
) -> Result<(), Outcome> {
    // --- Phase 0: estimator warm-up ---------------------------------------------------
    // No mode commanded (ctrl holds its identity reference, as it always has before the
    // first ModeCmd); complete when successive q̂ deltas stay small for the dwell.
    progress("estimator warm-up");
    tracing::info!("commissioning started; estimator warm-up");
    let mut last_q = None;
    check(
        || {
            let Ok(Some(e)) = att.latest() else {
                return false;
            };
            let q = e.q_hat_b_eci;
            let settled = last_q.is_some_and(|prev| {
                q.angular_distance(&prev).into_buf().abs() < params.est_delta_rad
            });
            last_q = Some(q);
            settled
        },
        secs(params.est_dwell_s),
        secs(params.warmup_timeout_s),
    )
    .await
    .or_fail("warm-up")?;

    // --- Phase 1: detumble (only for tumbles the wheels shouldn't capture) -------------
    let boot_rate = rate(att);
    if boot_rate > params.rate_detumble_enter {
        mode.publish(&ModeCmd::detumble().stamped(now()));
        progress("detumbling");
        tracing::warn!(boot_rate, "boot tumble beyond wheel capture; detumbling");
        check(
            || rate(att) < params.rate_detumble_exit,
            Duration::ZERO,
            secs(params.detumble_timeout_s),
        )
        .await
        .or_fail("detumble")?;
    }

    // --- Phase 2: coarse pointing ------------------------------------------------------
    // Wheels up, slewing onto the velocity-vector target; advance once the tracking error
    // holds under the coarse gate for the dwell.
    mode.publish(&ModeCmd::settling().stamped(now()));
    progress("coarse pointing");
    tracing::info!(
        boot_rate,
        "estimator settled; slewing onto the velocity-vector target"
    );
    check(
        || tracking_error(att, gps).is_some_and(|err| err < params.coarse_err_rad),
        secs(params.coarse_dwell_s),
        secs(params.settle_timeout_s),
    )
    .await
    .or_fail("coarse pointing")?;

    // --- Phase 3: fine pointing ---------------------------------------------------------
    // Declare the spacecraft commissioned once the error HOLDS for the confirm dwell (a
    // breach resets the dwell; the phase budget catches a loop that cannot hold).
    mode.publish(&ModeCmd::pointing().stamped(now()));
    progress("pointing");
    tracing::info!(
        gate_rad = params.coarse_err_rad,
        "coarse gate held; confirming fine pointing"
    );
    check(
        || tracking_error(att, gps).is_some_and(|err| err < params.coarse_err_rad),
        secs(params.confirm_dwell_s),
        secs(params.confirm_timeout_s),
    )
    .await
    .or_fail("pointing confirm")
}

/// A params budget or dwell, which the target spells in seconds.
fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

/// The estimated body rate (rad/s), or zero before the first estimate.
fn rate(att: &mut Input<AttitudeEstimate>) -> f64 {
    att.latest()
        .ok()
        .flatten()
        .map_or(0.0, |e| e.omega_b.norm().into_buf())
}

/// The estimated tracking error (rad) to the velocity-vector target — `q̂` against
/// `target_for(LAW_HIL, gps)`, the same law/guard ctrl steers by. `None` until both an
/// estimate and a GPS fix have arrived.
fn tracking_error(att: &mut Input<AttitudeEstimate>, gps: &mut Input<Gps>) -> Option<f64> {
    let q_hat = att.latest().ok().flatten()?.q_hat_b_eci;
    let g = gps.latest().ok().flatten()?;
    let target = target_for(ModeCmd::LAW_HIL, &g.pos_eci, &g.vel_eci);
    Some(q_hat.angular_distance(&target).into_buf().abs())
}
