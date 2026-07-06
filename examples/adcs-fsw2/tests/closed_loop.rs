//! The behavioral acceptance gate (dl-open.md §8): the closed ADCS loop drives the
//! spacecraft to track its commanded pointing law — **through the dlopen path**, and
//! matching the statically-linked path bit-for-bit.
//!
//! Both runs resolve the **same** `mission.kdl` (so the `mode` slot's `#[sequence]`
//! occupant — always a dlopen cdylib — commissions the spacecraft identically in both), and
//! differ in exactly one thing: whether the `plant`/`nav`/`ctrl` systems are linked
//! statically or `dlopen`'d.
//!
//! 1. **static** — `mission.kdl` resolved with `plant`/`nav`/`ctrl` linked as rlibs via a
//!    [`Registry`] (their `artifact` refs nulled so `resolve` takes the static factory);
//!    the `mode` slot's sequences stay dlopen.
//! 2. **dlopen** — the SAME mission, every system `dlopen`'d
//!    (`adcs_fsw2::build_sim_coordinator`).
//!
//! Both must converge (the controller tracks the velocity-vector target the auto-run
//! `commissioning` commands), and the dlopen run must land on the **same** tracking-error /
//! body-rate envelope as the static one. The dlopen build runs inside the test (slower),
//! gated off `miri`.

#![cfg(not(miri))]

use std::cell::RefCell;
use std::rc::Rc;

use adcs_contracts::{BodyState, ModeCmd, tracking_sample};
use adcs_ctrl::CtrlSystem;
use adcs_nav::NavSystem;
use adcs_plant::PlantSystem;
use metor_fsw_2::metor_proto::types::ComponentId;
use metor_fsw_2::wiring::Registry;
use metor_fsw_2::wiring::{build_artifacts, parse, resolve};
use metor_fsw_2::{BuildOptions, Coordinator, Input, register_system};

/// The mission wiring document — the same file the CLI runner and the other tests read.
const MISSION_KDL: &str = include_str!("../mission.kdl");

/// Cycles each mission runs (≈33 s of simulated time at 120 Hz) — enough to commission and
/// converge onto the (slowly orbit-rotating) pointing target.
const CYCLES: usize = 4000;

/// Static ≡ dlopen tolerance. The two paths compile the *same* source (rlib vs cdylib) and
/// share the seeded RNG, so the converged state is expected bit-identical; this small slack
/// absorbs any codegen difference between the two builds.
const PARITY_TOL: f64 = 1e-9;

/// The pointing law the auto-run `commissioning` settles on (velocity-vector), against which
/// the tracking error is measured (`ModeCmd::settling`/`pointing` ⇒ `LAW_HIL`).
const CONVERGED_LAW: u8 = ModeCmd::LAW_HIL;

/// The convergence envelope of one run: initial + final attitude tracking error (rad, to the
/// commanded pointing target) and body rate (rad/s), read from the tapped `truth`/`orbit`.
#[derive(Clone, Copy, Debug)]
struct Measure {
    e0: f64,
    r0: f64,
    ef: f64,
    rf: f64,
}

/// Resolve the mission with `plant`/`nav`/`ctrl` linked **statically** (the parity
/// reference): build every artifact (the `mode` slot's sequence cdylibs are still loaded),
/// then null the three systems' `artifact` refs so `resolve` instantiates them through the
/// [`Registry`] factory instead of `dlopen`. The slot/sequences remain dlopen in both paths,
/// so only the systems' linkage differs.
fn build_static() -> Coordinator {
    let mut wiring = parse(MISSION_KDL).expect("parse mission.kdl");
    build_artifacts(&mut wiring, &BuildOptions::default()).expect("build the cdylib artifacts");
    for spec in &mut wiring.systems {
        // plant/nav/ctrl: link statically via the Registry rather than dlopen.
        spec.artifact = None;
    }
    let mut registry = Registry::with_builtins();
    register_system!(&mut registry, PlantSystem => "Plant");
    register_system!(&mut registry, NavSystem => "Nav");
    register_system!(&mut registry, CtrlSystem => "Ctrl");
    resolve(&wiring, &registry).expect("resolve the mission with static systems")
}

/// Run `coord` for `cycles`, measuring convergence by tapping the plant's `body` output (the
/// ground-truth attitude/rate + orbit) and scoring the attitude error against the commanded
/// pointing-law target.
///
/// A spawned sampler reads it **every** cycle (the loop yields once per cycle under the
/// `Simulated` clock, so the sampler interleaves 1:1), capturing the whole trajectory. This
/// works for **both** paths: a dlopen'd plant's host-owned rings are tapped exactly like a
/// statically-linked one's.
async fn run_and_measure(mut coord: Coordinator, cycles: usize) -> Measure {
    let body_view: Input<BodyState> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("plant.body"))
            .expect("plant.body is registered")
            .expect("a reader slot is available"),
    );

    let samples = Rc::new(RefCell::new(Vec::<(f64, f64)>::new()));
    let captured = samples.clone();
    let sampler = stellarator::spawn(async move {
        let mut body = body_view;
        loop {
            stellarator::yield_now().await;
            if let Some(b) = body.latest() {
                captured
                    .borrow_mut()
                    .push(tracking_sample(b.get(), CONVERGED_LAW));
            }
        }
    })
    .drop_guard();

    coord.run_for(cycles).await;
    drop(sampler); // abort the sampler before reading the captured trajectory

    let trajectory = samples.borrow();
    let (e0, r0) = *trajectory.first().expect("the sampler captured the initial state");
    let (ef, rf) = *trajectory.last().expect("the sampler captured the final state");
    Measure { e0, r0, ef, rf }
}

/// Assert the loop converged: the attitude tracking error shrinks well below the start and
/// below a small absolute threshold, and the body rate damps toward the (small) orbital rate.
fn assert_converged(label: &str, m: &Measure) {
    println!(
        "[{label}] tracking error {:.4} -> {:.5} rad ({:.2} deg); rate {:.4} -> {:.6} rad/s",
        m.e0,
        m.ef,
        m.ef.to_degrees(),
        m.r0,
        m.rf
    );
    assert!(m.e0 > 0.3, "[{label}] starts with a real offset (>0.3 rad), got {}", m.e0);
    assert!(
        m.ef < m.e0 * 0.25,
        "[{label}] tracking error shrinks to <25% of start: {} -> {}",
        m.e0,
        m.ef
    );
    assert!(
        m.ef < 0.1,
        "[{label}] final tracking error converges below 0.1 rad (~6 deg): {}",
        m.ef
    );
    assert!(m.rf < m.r0, "[{label}] body rate damps: {} -> {}", m.r0, m.rf);
    assert!(
        m.rf < 0.05,
        "[{label}] final body rate near the orbital rate (<0.05 rad/s): {}",
        m.rf
    );
}

#[stellarator::test]
async fn closed_loop_converges_static_and_dlopen() {
    // Path 1 — plant/nav/ctrl statically linked (slot/sequences still dlopen).
    let static_run = run_and_measure(build_static(), CYCLES).await;
    assert_converged("static", &static_run);

    // Path 2 — the SAME mission, every system dlopen'd + resolved from the `Wiring`.
    let dl_coord = adcs_fsw2::build_sim_coordinator()
        .expect("build the system cdylibs, dlopen + resolve them");
    let dl_run = run_and_measure(dl_coord, CYCLES).await;
    assert_converged("dlopen", &dl_run);

    // The hard gate's payoff: the dlopen run lands on the SAME converged envelope as the
    // static run (same systems + params + seed, just loaded vs linked).
    let d_err = (static_run.ef - dl_run.ef).abs();
    let d_rate = (static_run.rf - dl_run.rf).abs();
    println!("static ≡ dlopen: |Δerror| = {d_err:.3e} rad, |Δrate| = {d_rate:.3e} rad/s");
    assert!(
        d_err < PARITY_TOL,
        "dlopen final tracking error matches static within {PARITY_TOL:e}: \
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
