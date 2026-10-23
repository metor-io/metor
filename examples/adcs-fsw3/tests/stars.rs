//! The star tracker's P0 gate. `sim.py`'s loop with the ideal tracker holds
//! the filter under 0.05° of the true attitude, and nav converges from
//! identity on star vectors alone with the truth 30°, 90° and 170° away.

use core::cell::RefCell;
use core::future::{Future, poll_fn};
use core::task::Poll;
use core::time::Duration;
use std::path::Path;
use std::rc::Rc;
use std::sync::OnceLock;

use adcs_contracts::{AttitudeEstimate, BodyState, V3};
use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::cli::run::prepare;
use metor_fsw_3::coordinator::{
    Clock, Coordinator, CoordinatorConfig, InputConfig, PortRef, SystemConfig,
};
use metor_fsw_3::{Input, system};
use nox::{ArrayRepr, Matrix3};

/// Cycles per run, 30 s at 120 Hz.
const CYCLES: u64 = 3600;
const CYCLE_S: f64 = 1.0 / 120.0;
/// Filter error bound (rad), 0.05°.
const BOUND: f64 = 0.05 * core::f64::consts::PI / 180.0;

/// Filter error against the true attitude (rad) and its normalised squared
/// error under the filter's attitude covariance, every cycle.
type Errors = Rc<RefCell<Vec<(f64, f64)>>>;

struct Watch(Errors);

#[system]
fn watch(
    #[state] state: &mut Watch,
    body: &mut Input<BodyState>,
    estimate: &mut Input<AttitudeEstimate>,
) {
    let (Ok(Some(body)), Ok(Some(estimate))) = (body.latest(), estimate.latest()) else {
        return;
    };
    let error: f64 = body
        .q_b_eci
        .angular_distance(&estimate.q_hat_b_eci)
        .into_buf();
    let d = estimate.q_hat_b_eci.inverse() * body.q_b_eci;
    let [x, y, z, w] = d.0.into_buf();
    let e = [x, y, z].map(|c| 2.0 * c * w.signum());
    let p = estimate.p.into_buf();
    let p_tt = Matrix3::<f64, ArrayRepr>::from_buf([
        [p[0][0], p[0][1], p[0][2]],
        [p[1][0], p[1][1], p[1][2]],
        [p[2][0], p[2][1], p[2][2]],
    ]);
    let nees = p_tt.try_inverse().map_or(f64::NAN, |inv| {
        let e = V3::from_buf(e);
        e.dot(&inv.dot(&e)).into_buf()
    });
    state.0.borrow_mut().push((error.abs(), nees));
}

/// `sim.py`'s target on a simulated clock, with the watcher added and the
/// publish link and db removed.
fn config() -> &'static CoordinatorConfig {
    static CONFIG: OnceLock<CoordinatorConfig> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let deployment = prepare(Some(&manifest.join("sim.py"))).expect("sim.py evaluates");
        let mut config = deployment
            .targets
            .into_iter()
            .next()
            .expect("one target")
            .coordinator;
        config.clock = Clock::Simulated {
            dt: Duration::from_secs_f64(CYCLE_S),
            epoch: None,
        };
        config
            .systems
            .retain(|s| !["fsw.publish", "fsw.db"].contains(&s.ty.as_str()));
        config.systems.push(SystemConfig {
            inputs: vec![
                input("body", PortRef::new("plant", "body")),
                input("estimate", PortRef::new("nav", "attitude_estimate")),
            ],
            ..SystemConfig::new("watch", "watch")
        });
        for system in &mut config.systems {
            if let Some(ty) = system.ty.strip_prefix("adcs.") {
                system.ty = ty.into();
            }
        }
        config
    })
}

fn input(port: &str, from: PortRef) -> InputConfig {
    InputConfig {
        port: port.into(),
        from: vec![from],
    }
}

/// `config` with the plant started `angle` from identity and nav on star
/// vectors alone.
fn from_identity(angle_deg: f64) -> CoordinatorConfig {
    let mut config = config().clone();
    for system in &mut config.systems {
        match system.id.as_str() {
            "plant" => system.params["init_angle"] = angle_deg.to_radians().into(),
            "nav" => system.params["sun_mag"] = false.into(),
            _ => {}
        }
    }
    config
}

fn build(config: &CoordinatorConfig, errors: &Errors) -> Coordinator {
    let mut table = adcs_systems::pack();
    let shared = errors.clone();
    table
        .register("watch", watch(), move || Watch(shared.clone()))
        .expect("valid records");
    register_builtins(&mut table).expect("valid records");
    config.clone().build(&table).expect("the config builds")
}

fn run(config: &CoordinatorConfig) -> Vec<(f64, f64)> {
    run_for(config, CYCLES)
}

fn run_for(config: &CoordinatorConfig, cycles: u64) -> Vec<(f64, f64)> {
    let errors = Errors::default();
    let mut coordinator = build(config, &errors);
    stellarator::run(move || async move {
        coordinator.run(stop(cycles)).await;
    });
    errors.take()
}

fn stop(cycles: u64) -> impl Future<Output = ()> {
    let mut polls = 0;
    poll_fn(move |_| {
        polls += 1;
        if polls >= cycles {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
}

/// Seconds from the first sample until the error stays under [`BOUND`].
fn settled(label: &str, samples: &[(f64, f64)]) -> f64 {
    let errors: Vec<f64> = samples.iter().map(|s| s.0).collect();
    let mut sorted = errors.clone();
    sorted.sort_by(f64::total_cmp);
    let p99 = sorted
        .get(sorted.len() * 99 / 100)
        .copied()
        .unwrap_or(f64::NAN);
    let max = sorted.last().copied().unwrap_or(f64::NAN);
    let late = &samples[samples.len() / 10..];
    let nees = late.iter().map(|s| s.1).sum::<f64>() / late.len().max(1) as f64;
    println!(
        "[{label}] max {:.4}°, p99 {:.4}°, mean NEES after the first 10% {nees:.2}",
        max.to_degrees(),
        p99.to_degrees()
    );
    let last_over = errors.iter().rposition(|e| *e >= BOUND);
    let first = errors.first().copied().unwrap_or(f64::NAN);
    let settled = last_over.map_or(0, |i| i + 1);
    let t = settled as f64 * CYCLE_S;
    println!(
        "[{label}] error {:.3}° at the start, under 0.05° from {t:.2} s; {:.5}° at the end",
        first.to_degrees(),
        errors.last().copied().unwrap_or(f64::NAN).to_degrees()
    );
    assert!(settled < errors.len(), "[{label}] never settled");
    t
}

#[test]
fn sim_loop_holds_the_filter_under_the_bound() {
    let errors = run(config());
    assert!(errors.len() > 3000, "{} samples", errors.len());
    assert!(settled("sim.py", &errors) < 30.0);
}

#[test]
fn stars_alone_converge_from_identity() {
    for angle in [30.0, 90.0, 170.0] {
        let errors = run(&from_identity(angle));
        assert!(settled(&format!("{angle}° from identity"), &errors) < 30.0);
    }
}

/// A gyro five times noisier than nav models still converges, with nav's
/// process noise margin keeping the filter near consistent.
#[test]
fn noisy_gyro_stays_under_the_bound() {
    let errors = run(&noisy_gyro());
    assert!(settled("gyro 5x", &errors) < 30.0);
}

/// `sim.py` with a gyro five times noisier than nav models.
fn noisy_gyro() -> CoordinatorConfig {
    let mut config = config().clone();
    for system in &mut config.systems {
        if system.id == "plant" {
            system.params["meas_sigma"] = 0.01.into();
        }
    }
    config
}

/// The 5× gyro case over 300 s, for its steady state. Slow, so run by hand:
/// `cargo test --test stars -- --ignored --nocapture`.
#[test]
#[ignore]
fn noisy_gyro_steady_state() {
    let samples = run_for(&noisy_gyro(), 120 * 300);
    let late = &samples[samples.len() / 10..];
    let max = late.iter().map(|s| s.0).fold(0.0, f64::max);
    let mean_nees = late.iter().map(|s| s.1).sum::<f64>() / late.len() as f64;
    let over = late.iter().filter(|s| s.0 >= BOUND).count();
    println!(
        "[gyro 5x, 300 s] after 30 s: max {:.4}°, mean NEES {mean_nees:.2}, {:.1}% of cycles over 0.05°",
        max.to_degrees(),
        100.0 * over as f64 / late.len() as f64
    );
    settled("gyro 5x, 300 s", &samples);
}
