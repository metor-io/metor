//! Eclipse acceptance: booted inside Earth shadow the CSS heads read only their noise
//! floor, SRP vanishes, and nav — magnetometer-only, by its own validity call — stays
//! bounded; crossing INTO shadow mid-run, the truth flag and the sensor darkness flip in
//! lockstep and the estimate rides through the sun loss.
//!
//! Both runs use the static path (plant/nav/ctrl as rlibs, the `momentum.rs` pattern) off a
//! patched `target.py`: `init_orbit_phase` places the boot inside / just before the
//! shadow arc — the phase is **computed** by scanning [`in_earth_shadow`] at the target
//! epoch, never hardcoded. The `mode` slot starts empty (ctrl holds its identity reference)
//! so the estimator behavior under sun loss is the whole subject. Deterministic: seeded
//! RNG, fixed epoch, no wall clock. Gated off `miri` (provision_artifacts builds the sequence
//! cdylibs the slot loads even when unoccupied).

#![cfg(not(miri))]

use std::cell::RefCell;
use std::f64::consts::TAU;
use std::path::Path;
use std::rc::Rc;

use adcs_contracts::{
    ALTITUDE, AttitudeEstimate, BodyState, CSS_THRESHOLD, Disturbances, EARTH_RADIUS, Quat,
    Sensors, V3, World, in_earth_shadow, mission_epoch, sun_dir_eci,
};
use metor_fsw_2::metor_proto::types::ComponentId;
use metor_fsw_2::wiring::Registry;
use metor_fsw_2::wiring::{ParamSource, eval_python_target, provision_artifacts, resolve};
use metor_fsw_2::{BuildOptions, Coordinator, Input};

fn mission_py() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target.py")
}

mod common;

/// The first lit→dark orbit phase at the target epoch, by scanning the shadow function
/// around the boot orbit (equatorial, radius `EARTH_RADIUS + ALTITUDE`) in 0.1° steps.
fn shadow_entry_phase() -> f64 {
    let sun = sun_dir_eci(mission_epoch());
    let r = EARTH_RADIUS + ALTITUDE;
    let pos = |theta: f64| -> V3 { V3::from_buf([theta.cos(), theta.sin(), 0.0]) * r };
    let step = 0.1f64.to_radians();
    let mut theta = 0.0;
    while theta < TAU {
        if !in_earth_shadow(&pos(theta), &sun) && in_earth_shadow(&pos(theta + step), &sun) {
            return theta + step;
        }
        theta += step;
    }
    panic!("an equatorial 400 km orbit at the target epoch always crosses Earth shadow");
}

/// The patched target: boot at `phase` radians of orbit, slot empty (no commissioning).
fn build_static(phase: f64) -> Option<Coordinator> {
    if !common::ensure_stubs() {
        return None;
    }
    let mut wiring = match eval_python_target(&mission_py()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("skipping: target.py did not evaluate: {e}");
            return None;
        }
    };
    // Boot the plant `phase` radians into its orbit — the eclipse-arc anchor.
    let plant = wiring
        .systems
        .iter_mut()
        .find(|s| s.name == "plant")
        .expect("the plant system");
    let ParamSource::Value(serde_json::Value::Object(params)) = &mut plant.params else {
        panic!("plant carries a params value tree");
    };
    params.insert("init_orbit_phase".into(), phase.into());
    for spec in &mut wiring.systems {
        // Static rlib systems, in-process (test binaries can't host a process worker).
        spec.artifact = None;
        spec.process = false;
    }
    for slot in &mut wiring.slots {
        slot.initial = None;
    }
    if let Err(e) = provision_artifacts(&mut wiring, &BuildOptions::default()) {
        eprintln!("skipping: provision_artifacts failed: {e}");
        return None;
    }
    let mut registry = Registry::with_builtins();
    registry.register_pack(adcs_systems::pack());
    Some(resolve(&wiring, &registry).expect("resolve the patched target"))
}

/// One cycle's view of the target: the truth illumination flag, the brightest CSS head,
/// |SRP torque|, and the attitude-estimate error against truth.
#[derive(Clone, Copy, Debug)]
struct Sample {
    illuminated: bool,
    css_max: f64,
    srp: f64,
    est_err: f64,
}

/// Run `coord` for `cycles`, sampling every cycle (idle views stall rings — the sampler
/// must interleave, like `closed_loop.rs`'s).
async fn run_and_sample(mut coord: Coordinator, cycles: usize) -> Vec<Sample> {
    let registry = coord.registry();
    let tap = |name: &str| {
        registry
            .view(ComponentId::new(name))
            .unwrap_or_else(|| panic!("`{name}` is registered"))
            .expect("a reader slot is available")
    };
    let world_view: Input<World> = Input::new(tap("plant.world"));
    let sensors_view: Input<Sensors> = Input::new(tap("plant.sensors"));
    let disturb_view: Input<Disturbances> = Input::new(tap("plant.disturb"));
    let est_view: Input<AttitudeEstimate> = Input::new(tap("nav.attitude_estimate"));
    let body_view: Input<BodyState> = Input::new(tap("plant.body"));

    let samples = Rc::new(RefCell::new(Vec::<Sample>::new()));
    let captured = samples.clone();
    let sampler = stellarator::spawn(async move {
        let (mut world, mut sensors, mut disturb, mut est, mut body) =
            (world_view, sensors_view, disturb_view, est_view, body_view);
        loop {
            stellarator::yield_now().await;
            let (Ok(Some(w)), Ok(Some(s)), Ok(Some(d)), Ok(Some(b))) = (
                world.latest(),
                sensors.latest(),
                disturb.latest(),
                body.latest(),
            ) else {
                continue;
            };
            // Before nav's first estimate the error is measured against the MEKF's identity
            // init — still meaningful (it is what ctrl flies on).
            let q_hat = est
                .latest()
                .ok()
                .flatten()
                .map(|e| e.get().q_hat_b_eci)
                .unwrap_or_else(Quat::identity);
            let est_err: f64 = q_hat.angular_distance(&b.get().q_b_eci).into_buf().abs();
            let css = s.get().css;
            captured.borrow_mut().push(Sample {
                illuminated: w.get().illuminated != 0,
                css_max: css.iter().cloned().fold(f64::MIN, f64::max),
                srp: d.get().srp_b.norm().into_buf(),
                est_err,
            });
        }
    })
    .drop_guard();

    coord.run_for(cycles).await;
    // The canceled sampler releases its Rc at its next scheduler poll, not at drop — clone
    // the captured samples out under a borrow instead of try_unwrap.
    drop(sampler);
    let out = samples.borrow().clone();
    out
}

/// Booted deep in the shadow arc: dark throughout, SRP off, the CSS heads never clear the
/// validity threshold, and the mag-only estimate neither diverges nor goes non-finite.
#[stellarator::test]
async fn eclipse_darkens_sensors_and_nav_stays_bounded() {
    // ~11.5° past entry — inside the ~137° shadow arc for the whole 2000-cycle window.
    let Some(coord) = build_static(shadow_entry_phase() + 0.2) else {
        return;
    };
    let samples = run_and_sample(coord, 2000).await;
    assert!(
        samples.len() > 1000,
        "the sampler kept up: {}",
        samples.len()
    );

    let err_0 = samples.first().unwrap().est_err;
    let err_f = samples.last().unwrap().est_err;
    println!(
        "[shadowed] css_max {:.4}, est err {:.4} -> {:.4} rad",
        samples.iter().map(|s| s.css_max).fold(f64::MIN, f64::max),
        err_0,
        err_f
    );
    for s in &samples {
        assert!(!s.illuminated, "the whole window is in shadow: {s:?}");
        assert!(
            s.css_max < CSS_THRESHOLD,
            "every CSS head stays noise-floor: {s:?}"
        );
        assert!(s.srp == 0.0, "SRP is off in shadow: {s:?}");
        assert!(
            s.est_err.is_finite(),
            "the mag-only estimate stays finite: {s:?}"
        );
    }
    // Mag-only from a COLD start owes no convergence: the about-B̂ component is unobserved,
    // and ctrl steering on the imperfect estimate moves the truth underneath it, so the
    // error legitimately wanders (measured ~0.23 → ~0.34 rad here). The requirement is no
    // divergence: everything stays well under the π of a lost filter.
    let err_max = samples.iter().map(|s| s.est_err).fold(f64::MIN, f64::max);
    assert!(
        err_max < 1.0,
        "the mag-only estimate never diverges: max {err_max} ({err_0} -> {err_f})"
    );
}

/// Booted just before shadow entry: the truth flag and the CSS darkness flip together
/// mid-run (once — a single crossing), and the estimate rides through the sun loss.
#[stellarator::test]
async fn shadow_entry_flips_sensors_in_lockstep_and_nav_rides_through() {
    // 1° before entry ≈ 15 s of orbit; the 3000-cycle (25 s) window crosses mid-run.
    let Some(coord) = build_static(shadow_entry_phase() - 1.0f64.to_radians()) else {
        return;
    };
    let samples = run_and_sample(coord, 3000).await;

    let lit_first = samples.first().unwrap().illuminated;
    let lit_last = samples.last().unwrap().illuminated;
    assert!(lit_first, "the run starts sunlit");
    assert!(!lit_last, "the run ends in shadow");
    let mut crossings = 0;
    for pair in samples.windows(2) {
        if pair[0].illuminated != pair[1].illuminated {
            crossings += 1;
        }
    }
    assert_eq!(crossings, 1, "shadow entry is a single crossing");

    // The sensor's own report tracks the truth flag exactly: lit ⇒ some head is bright
    // (dimmest lit reading ≥ 1/√3, ~50σ above the threshold), dark ⇒ all heads noise-floor.
    for s in &samples {
        assert_eq!(
            s.css_max >= CSS_THRESHOLD,
            s.illuminated,
            "CSS brightness tracks illumination: {s:?}"
        );
    }

    // Sunlit convergence first, then the estimate rides through the sun loss.
    let err_at_entry = samples
        .windows(2)
        .find(|p| p[0].illuminated && !p[1].illuminated)
        .map(|p| p[0].est_err)
        .unwrap();
    let err_f = samples.last().unwrap().est_err;
    println!("[transition] err at entry {err_at_entry:.4}, err at end {err_f:.4} rad");
    assert!(err_at_entry < 0.1, "converged before entry: {err_at_entry}");
    assert!(
        err_f < 0.2,
        "the estimate rides through the sun loss: {err_f}"
    );
}
