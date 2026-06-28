//! The behavioral acceptance gate (dl-open.md §8): the closed ADCS loop drives the
//! spacecraft attitude toward the target — **through the dlopen path**, and matching the
//! statically-linked path bit-for-bit.
//!
//! Two missions run, both measured the **same** way (tapping the plant's `truth` output
//! from the coordinator's registry, since a `.so`'s statics never reach the host):
//!
//! 1. **static** — `PlantSystem`/`NavSystem`/`CtrlSystem` linked as rlibs and wired with
//!    `Coordinator::builder` (the old `build_code_first`).
//! 2. **dlopen** — the SAME systems, each built as a `cdylib`, `dlopen`'d and resolved from
//!    a `Wiring` (`adcs_fsw2::build_sim_coordinator`, which runs the build driver first).
//!
//! Both must converge, and the dlopen run must land on the **same** attitude-error / body-
//! rate envelope as the static one (identical within a tight tolerance — same systems, same
//! params, just loaded vs linked). The dlopen build runs inside the test (slower, like
//! `metor-fsw-2`'s `dl_integration`/`wiring_resolve`), gated off `miri`.

#![cfg(not(miri))]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adcs_contracts::{
    AttitudeEstimate, CtrlParams, DT, NavParams, PlantParams, Sensors, TorqueCmd, Truth,
    convergence_sample,
};
use adcs_ctrl::CtrlSystem;
use adcs_nav::NavSystem;
use adcs_plant::PlantSystem;
use metor_fsw_2::metor_proto::types::ComponentId;
use metor_fsw_2::{ClockMode, Coordinator, CoordinatorConfig, Input, PortRef};

/// Cycles each mission runs (≈33 s of simulated time at 120 Hz) — enough to converge.
const CYCLES: usize = 4000;

/// Static ≡ dlopen tolerance. The two paths compile the *same* source (rlib vs cdylib) and
/// share the seeded RNG, so the converged state is expected bit-identical; this small slack
/// absorbs any codegen difference between the two builds.
const PARITY_TOL: f64 = 1e-9;

/// The convergence envelope of one run: initial + final attitude error (rad) and body rate
/// (rad/s), read from the tapped `truth` output.
#[derive(Clone, Copy, Debug)]
struct Measure {
    e0: f64,
    r0: f64,
    ef: f64,
    rf: f64,
}

/// Wire the static (rlib-linked) closed loop with the coordinator builder — the parity
/// reference. Registration order Plant → Nav → Ctrl resolves the forward chain in one
/// cycle; the Ctrl → Plant torque edge is `connect_delayed` (the one-cycle feedback
/// back-edge). Free-running `Simulated` clock at `DT`, no telemetry.
fn build_static() -> Coordinator {
    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 120.0,
        default_depth: metor_fsw_2::DEFAULT_DEPTH,
        clock: ClockMode::Simulated {
            dt: Duration::from_secs_f64(DT),
        },
    });
    let plant = b.add_cyclic(PlantSystem::new(PlantParams {
        init_angle: 0.5,
        init_rate: 0.15,
        meas_sigma: 0.002,
        seed: 42,
    }));
    let nav = b.add_cyclic(NavSystem::new(NavParams { meas_sigma: 0.02 }));
    let ctrl = b.add_cyclic(CtrlSystem::new(CtrlParams { q_weight: 5.0, r_weight: 8.0 }));

    b.connect(PortRef::new::<Sensors>(plant), PortRef::new::<Sensors>(nav))
        .expect("plant → nav sensors edge");
    b.connect(
        PortRef::new::<AttitudeEstimate>(nav),
        PortRef::new::<AttitudeEstimate>(ctrl),
    )
    .expect("nav → ctrl estimate edge");
    b.connect_delayed(
        PortRef::new::<TorqueCmd>(ctrl),
        PortRef::new::<TorqueCmd>(plant),
    )
    .expect("ctrl → plant torque feedback back-edge");
    b.build().expect("static wiring builds")
}

/// Run `coord` for `cycles`, measuring convergence by tapping the plant's `truth` output.
///
/// A spawned sampler reads `truth` **every** cycle (the loop yields once per cycle under the
/// `Simulated` clock, so the sampler interleaves 1:1 — the same pattern as the live mission's
/// printer), capturing the whole trajectory: the first sample is the initial state, the last
/// is the final cycle's. (A single view read only at the end would lap after thousands of
/// cycles.) This works for **both** paths: a dlopen'd plant's host-owned `truth` ring is
/// tapped exactly like a statically-linked one's.
async fn run_and_measure(mut coord: Coordinator, cycles: usize) -> Measure {
    let sample_view: Input<Truth> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("plant.truth"))
            .expect("plant.truth is registered")
            .expect("a reader slot is available"),
    );

    let samples = Rc::new(RefCell::new(Vec::<(f64, f64)>::new()));
    let captured = samples.clone();
    let sampler = stellarator::spawn(async move {
        let mut input = sample_view;
        loop {
            stellarator::yield_now().await;
            if let Ok(Some(r)) = input.latest() {
                captured.borrow_mut().push(convergence_sample(r.get()));
            }
        }
    })
    .drop_guard();

    coord.run_for(cycles).await;
    drop(sampler); // abort the sampler before reading the captured trajectory

    let trajectory = samples.borrow();
    let (e0, r0) = *trajectory.first().expect("the sampler captured the initial truth");
    let (ef, rf) = *trajectory.last().expect("the sampler captured the final truth");
    Measure { e0, r0, ef, rf }
}

/// Assert the loop converged: attitude error shrinks well below the start and below a small
/// absolute threshold, and the body rate damps toward zero.
fn assert_converged(label: &str, m: &Measure) {
    println!(
        "[{label}] attitude error {:.4} -> {:.5} rad ({:.2} deg); rate {:.4} -> {:.6} rad/s",
        m.e0,
        m.ef,
        m.ef.to_degrees(),
        m.r0,
        m.rf
    );
    assert!(m.e0 > 0.3, "[{label}] starts with a real offset (>0.3 rad), got {}", m.e0);
    assert!(
        m.ef < m.e0 * 0.1,
        "[{label}] attitude error shrinks to <10% of start: {} -> {}",
        m.e0,
        m.ef
    );
    assert!(
        m.ef < 0.05,
        "[{label}] final attitude error converges below 0.05 rad (~3 deg): {}",
        m.ef
    );
    assert!(m.rf < m.r0, "[{label}] body rate damps: {} -> {}", m.r0, m.rf);
    assert!(m.rf < 0.02, "[{label}] final body rate near zero (<0.02 rad/s): {}", m.rf);
}

#[stellarator::test]
async fn closed_loop_converges_static_and_dlopen() {
    // Path 1 — the statically-linked reference.
    let static_run = run_and_measure(build_static(), CYCLES).await;
    assert_converged("static", &static_run);

    // Path 2 — the SAME systems built as cdylibs, dlopen'd + resolved from a `Wiring`.
    let dl_coord = adcs_fsw2::build_sim_coordinator()
        .expect("build the three system cdylibs, dlopen + resolve them");
    let dl_run = run_and_measure(dl_coord, CYCLES).await;
    assert_converged("dlopen", &dl_run);

    // The hard gate's payoff: the dlopen run lands on the SAME converged envelope as the
    // static run (same systems + params + seed, just loaded vs linked).
    let d_err = (static_run.ef - dl_run.ef).abs();
    let d_rate = (static_run.rf - dl_run.rf).abs();
    println!("static ≡ dlopen: |Δerror| = {d_err:.3e} rad, |Δrate| = {d_rate:.3e} rad/s");
    assert!(
        d_err < PARITY_TOL,
        "dlopen final attitude error matches static within {PARITY_TOL:e}: \
         static {} vs dlopen {} (Δ {d_err:e})",
        static_run.ef,
        dl_run.ef
    );
    assert!(
        d_rate < PARITY_TOL,
        "dlopen final body rate matches static within {PARITY_TOL:e}: \
         static {} vs dlopen {} (Δ {d_rate:e})",
        static_run.rf,
        dl_run.rf
    );
}
