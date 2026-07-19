//! The **commissioning** sequence: the `mode` slot's initial occupant, a **condition-based**
//! state machine the slot polls once per cycle, walking the ladder
//!
//! ```text
//! estimator warm-up ──▶ [detumble]* ──▶ coarse pointing ──▶ fine pointing ──▶ Completed
//! ```
//!
//! Every transition is gated on what the FSW observes (the attitude estimate and the GPS
//! orbit state — never truth), every phase has a timeout that safes the spacecraft and
//! fails the sequence, and a cooperative `Abort` (latched at any `wait`) safes and bails
//! out `Aborted`. The gates and budgets ride [`CommissioningParams`] off the mission's
//! `allow` line, which is also how tests patch them.
//!
//! *Detumble (magnetorquer-only rate damping, `LAW_DETUMBLE`) is entered only when the
//! estimated rate is beyond what the reaction wheels should capture — the boot tumble is
//! not, so mission runs go straight from warm-up to coarse pointing.

use core::time::Duration;

use adcs_contracts::{AttitudeEstimate, CommissioningParams, Gps, ModeCmd, target_for};
use metor_fsw_2::metor_proto::types::Timestamp;
use metor_fsw_2::sequence::{now, progress, wait};
use metor_fsw_2::{Input, Outcome, Output, Params};

/// One coordinator cycle: under the `Simulated` clock `now` advances ≥ one cycle's worth of
/// microseconds per poll, so a 1 µs wait is pending this poll and ready the next — the
/// per-cycle yield primitive (`wait(Duration::ZERO)` resolves within the same poll and
/// would spin).
const NEXT_CYCLE: Duration = Duration::from_micros(1);

/// Seconds of sequence-clock time since `t0`.
fn secs_since(t0: Timestamp) -> f64 {
    (now().0 - t0.0) as f64 / 1e6
}

/// Walk the commissioning ladder, gated on the live estimate + GPS orbit state.
pub(crate) async fn commissioning(
    mut att: Input<AttitudeEstimate>,
    mut gps: Input<Gps>,
    Params(params): Params<CommissioningParams>,
    mut mode: Output<ModeCmd>,
) -> Outcome {
    // --- Phase 0: estimator warm-up ---------------------------------------------------
    // No mode commanded (ctrl holds its identity reference, as it always has before the
    // first ModeCmd); complete when successive q̂ deltas stay small for the dwell.
    progress("estimator warm-up");
    tracing::info!("commissioning started; estimator warm-up");
    let t0 = now();
    let mut last_q = None;
    let mut settled_since: Option<Timestamp> = None;
    let mut rate;
    loop {
        if wait(NEXT_CYCLE).await.aborted() {
            mode.publish(&ModeCmd::safe().stamped(now()));
            return Outcome::Aborted;
        }
        if secs_since(t0) > params.warmup_timeout_s {
            mode.publish(&ModeCmd::safe().stamped(now()));
            progress("timeout in warm-up");
            tracing::warn!(budget_s = params.warmup_timeout_s, "warm-up timed out; safing");
            return Outcome::Failed;
        }
        let Ok(Some(e)) = att.latest() else { continue };
        let q = e.q_hat_b_eci;
        rate = e.omega_b.norm().into_buf();
        if let Some(prev) = last_q {
            let delta: f64 = q.angular_distance(&prev).into_buf().abs();
            if delta < params.est_delta_rad {
                let since = *settled_since.get_or_insert(now());
                if (now().0 - since.0) as f64 / 1e6 >= params.est_dwell_s {
                    break;
                }
            } else {
                settled_since = None;
            }
        }
        last_q = Some(q);
    }

    // --- Phase 1: detumble (only for tumbles the wheels shouldn't capture) -------------
    if rate > params.rate_detumble_enter {
        mode.publish(&ModeCmd::detumble().stamped(now()));
        progress("detumbling");
        tracing::warn!(rate, "boot tumble beyond wheel capture; detumbling");
        let t0 = now();
        loop {
            if wait(NEXT_CYCLE).await.aborted() {
                mode.publish(&ModeCmd::safe().stamped(now()));
                return Outcome::Aborted;
            }
            if secs_since(t0) > params.detumble_timeout_s {
                mode.publish(&ModeCmd::safe().stamped(now()));
                progress("timeout in detumble");
                tracing::warn!(budget_s = params.detumble_timeout_s, "detumble timed out; safing");
                return Outcome::Failed;
            }
            let Ok(Some(e)) = att.latest() else { continue };
            let rate: f64 = e.omega_b.norm().into_buf();
            if rate < params.rate_detumble_exit {
                break;
            }
        }
    }

    // --- Phase 2: coarse pointing ------------------------------------------------------
    // Wheels up, slewing onto the velocity-vector target; advance once the tracking error
    // holds under the coarse gate for the dwell.
    mode.publish(&ModeCmd::settling().stamped(now()));
    progress("coarse pointing");
    tracing::info!(rate, "estimator settled; slewing onto the velocity-vector target");
    let t0 = now();
    let mut ok_since: Option<Timestamp> = None;
    loop {
        if wait(NEXT_CYCLE).await.aborted() {
            mode.publish(&ModeCmd::safe().stamped(now()));
            return Outcome::Aborted;
        }
        if secs_since(t0) > params.settle_timeout_s {
            mode.publish(&ModeCmd::safe().stamped(now()));
            progress("timeout in coarse pointing");
            tracing::warn!(budget_s = params.settle_timeout_s, "coarse pointing timed out; safing");
            return Outcome::Failed;
        }
        match tracking_error(&mut att, &mut gps) {
            Some(err) if err < params.coarse_err_rad => {
                let since = *ok_since.get_or_insert(now());
                if (now().0 - since.0) as f64 / 1e6 >= params.coarse_dwell_s {
                    break;
                }
            }
            _ => ok_since = None,
        }
    }

    // --- Phase 3: fine pointing ---------------------------------------------------------
    // Declare the spacecraft commissioned once the error HOLDS for the confirm dwell (a
    // breach resets the dwell; the phase timeout catches a loop that cannot hold).
    mode.publish(&ModeCmd::pointing().stamped(now()));
    progress("pointing");
    tracing::info!(gate_rad = params.coarse_err_rad, "coarse gate held; confirming fine pointing");
    let t0 = now();
    let mut ok_since: Option<Timestamp> = None;
    loop {
        if wait(NEXT_CYCLE).await.aborted() {
            mode.publish(&ModeCmd::safe().stamped(now()));
            return Outcome::Aborted;
        }
        if secs_since(t0) > params.confirm_timeout_s {
            mode.publish(&ModeCmd::safe().stamped(now()));
            progress("timeout in pointing confirm");
            tracing::warn!(budget_s = params.confirm_timeout_s, "pointing confirm timed out; safing");
            return Outcome::Failed;
        }
        match tracking_error(&mut att, &mut gps) {
            Some(err) if err < params.coarse_err_rad => {
                let since = *ok_since.get_or_insert(now());
                if (now().0 - since.0) as f64 / 1e6 >= params.confirm_dwell_s {
                    break;
                }
            }
            _ => ok_since = None,
        }
    }
    progress("commissioned");
    tracing::info!("commissioned");
    Outcome::Completed
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
