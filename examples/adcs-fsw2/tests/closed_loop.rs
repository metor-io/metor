//! The behavioral assertion: the closed ADCS loop drives the spacecraft attitude
//! toward the target. Exercised through BOTH wiring paths — code-first
//! (`Coordinator::builder`) and a KDL document (`wiring::load`) — to prove frames,
//! the ring transport, the feedback-loop sampling, the MEKF, and the LQR all work
//! together.

use adcs_fsw2::{KDL, SIM_LOG, build_code_first, registry, reset_sim_log};
use metor_fsw_2::wiring::load;

/// Read (initial error, final error, initial rate, final rate) out of the shared log.
fn convergence() -> (f64, f64, f64, f64) {
    let log = SIM_LOG.lock().unwrap();
    assert!(log.err_angle.len() > 100, "loop produced samples");
    (
        log.err_angle[0],
        *log.err_angle.last().unwrap(),
        log.rate[0],
        *log.rate.last().unwrap(),
    )
}

/// Assert the loop converged: attitude error shrinks well below the start and below
/// a small absolute threshold, and the body rate damps toward zero.
fn assert_converged(label: &str) {
    let (e0, ef, r0, rf) = convergence();
    println!(
        "[{label}] attitude error {:.4} -> {:.5} rad ({:.2} deg); rate {:.4} -> {:.6} rad/s",
        e0,
        ef,
        ef.to_degrees(),
        r0,
        rf
    );
    assert!(e0 > 0.3, "[{label}] starts with a real offset (>0.3 rad), got {e0}");
    assert!(
        ef < e0 * 0.1,
        "[{label}] attitude error shrinks to <10% of start: {e0} -> {ef}"
    );
    assert!(
        ef < 0.05,
        "[{label}] final attitude error converges below 0.05 rad (~3 deg): {ef}"
    );
    assert!(rf < r0, "[{label}] body rate damps: {r0} -> {rf}");
    assert!(rf < 0.02, "[{label}] final body rate near zero (<0.02 rad/s): {rf}");
}

// Both paths drive the same process-global `SIM_LOG`, so they run sequentially in
// one test (cargo would otherwise run separate `#[test]` fns on parallel threads and
// clobber the shared log).
#[stellarator::test]
async fn closed_loop_converges_both_paths() {
    // Path 1 — code-first via `Coordinator::builder` + `add_cyclic` + `connect`.
    reset_sim_log();
    let mut coord = build_code_first().expect("code-first wiring builds");
    coord.run_for(4000).await;
    assert_converged("code-first");

    // Path 2 — the SAME systems wired from a KDL document via `Registry` + `load`.
    reset_sim_log();
    let mut coord = load(KDL, &registry()).expect("KDL document loads");
    coord.run_for(4000).await;
    assert_converged("kdl");
}
