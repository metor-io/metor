//! The **commissioning** sequence of the `adcs-fsw2` mission, as a `#[sequence]`-authored
//! `dlopen`-loadable `cdylib` (sequences-slots.md §4). It is the `mode` slot's initial
//! occupant: a small re-entrant state machine the slot polls once per cycle, walking the
//! spacecraft through idle → settling → pointing as it brings the reaction wheels up.
//!
//! The body is the design's author shape verbatim (§4.1): typed owned ports as direct
//! parameters (the macro binds them once and moves them into the future — the ring-backing
//! generic is injected, review E7) and the free `now`/`wait`/`progress` API reaching the
//! ambient `SeqClock`. `wait` resolves against the **coordinator** clock — under the
//! mission's `Simulated` 1/120 s step, 100 ms ≈ 12 cycles and 150 ms ≈ 18 cycles, so the
//! whole sequence completes in ~30 cycles. On a cooperative `Abort` (the slot's cancel
//! signal latched at the next `wait`) it writes `ModeCmd::safe` and bails out `Aborted`.

use core::time::Duration;

use adcs_contracts::{AttitudeEstimate, ModeCmd};
use metor_fsw_2::sequence::{now, progress, wait};
use metor_fsw_2::{Input, Outcome, Output};

/// Walk idle → settling → pointing, reading the live attitude estimate along the way.
#[metor_fsw_2::sequence]
async fn commissioning(mut att: Input<AttitudeEstimate>, mut mode: Output<ModeCmd>) -> Outcome {
    progress("warming up");
    if wait(Duration::from_millis(100)).await.aborted() {
        mode.publish(&ModeCmd::safe().stamped(now()));
        return Outcome::Aborted;
    }
    // Read the live estimate (demonstrates owned-port input access from inside the future).
    let _ = att.latest();
    mode.publish(&ModeCmd::settling().stamped(now()));
    progress("reaction wheels enabled");
    if wait(Duration::from_millis(150)).await.aborted() {
        mode.publish(&ModeCmd::safe().stamped(now()));
        return Outcome::Aborted;
    }
    mode.publish(&ModeCmd::pointing().stamped(now()));
    progress("pointing");
    Outcome::Completed
}
