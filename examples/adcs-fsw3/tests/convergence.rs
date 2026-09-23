//! The gate: `sim.py`'s loop drives the spacecraft onto its nadir target, and
//! the run built against the pack's rlib matches the one built against the
//! cdylib `pack dev` staged, cycle for cycle.

use core::cell::RefCell;
use core::future::{Future, poll_fn};
use core::task::Poll;
use core::time::Duration;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Once;

use adcs_contracts::{AttitudeEstimate, BodyState, DT, ModeCmd, TorqueCmd, tracking_sample};
use metor_fsw_3::cli::cargo::{cdylib_name, triple};
use metor_fsw_3::cli::run::prepare;
use metor_fsw_3::coordinator::{
    Clock, Coordinator, CoordinatorConfig, InputConfig, PortRef, SystemConfig, SystemTable,
};
use metor_fsw_3::link::register_builtins;
use metor_fsw_3::{Input, Pack, system};
use metor_proto_wkt::LogEvent;
use tracing_subscriber::layer::SubscriberExt;
use zerocopy::IntoBytes;

/// Cycles each run steps: 20 s of simulated time at 120 Hz. The error crosses
/// the bound at cycle 942 (~7.9 s), so the horizon carries twice the margin
/// the loop needs.
const CYCLES: u64 = 2400;

/// The commanded law the controller flies, which `mode` holds for the run.
const LAW: u8 = ModeCmd::LAW_NADIR;

/// The pointing error the run converges below (rad, ~6°), and the body rate it
/// damps below (rad/s) — the fsw-2 closed-loop envelope.
const ERR_BOUND: f64 = 0.1;
const RATE_BOUND: f64 = 0.05;

/// What one run recorded.
#[derive(Default)]
struct Record {
    /// The tracking error and body rate of every cycle, from the truth state.
    track: Vec<(f64, f64)>,
    /// The filter's disagreement with the truth attitude (rad), every cycle.
    estimate: Vec<f64>,
    /// Every commanded torque, as its bytes sat on the ring; the stamp is the
    /// coordinator's own epoch, so it is left out.
    torque: Vec<Vec<u8>>,
    /// Every line the filter logged.
    logs: Vec<String>,
}

type Shared = Rc<RefCell<Record>>;

/// Scores the loop against its commanded target from the plant's truth state.
struct Watch(Shared);

#[system]
impl Watch {
    fn execute(&mut self, body: &mut Input<BodyState>, estimate: &mut Input<AttitudeEstimate>) {
        let Ok(Some(body)) = body.latest() else {
            return;
        };
        let mut record = self.0.borrow_mut();
        record.track.push(tracking_sample(&body, LAW));
        if let Ok(Some(estimate)) = estimate.latest() {
            let error = body.q_b_eci.angular_distance(&estimate.q_hat_b_eci);
            record.estimate.push(error.into_buf().abs());
        }
    }
}

/// Keeps the controller's command stream and the filter's log lines.
struct Tap(Shared);

#[system]
impl Tap {
    fn execute(&mut self, torque_cmd: &mut Input<TorqueCmd>, log: &mut Input<LogEvent>) {
        let mut record = self.0.borrow_mut();
        for torque in torque_cmd.drain().flatten() {
            record.torque.push(torque.torque_b.as_bytes().to_vec());
        }
        for line in log.drain().flatten() {
            record.logs.push(line.message.into_owned());
        }
    }
}

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// `sim.py`'s coordinator on a simulated clock, with the sinks appended and the
/// publish link dropped: the gate is io-less, and two runs share a port.
///
/// Evaluated once per process: `prepare` stages the pack and holds it open, so
/// a second call refuses to restage it.
fn config() -> CoordinatorConfig {
    let deployment = prepare(Some(&manifest().join("sim.py"))).expect("sim.py evaluates");
    assert_eq!(deployment.targets.len(), 1, "sim.py is one target");
    let mut config = deployment
        .targets
        .into_iter()
        .next()
        .expect("one target")
        .coordinator;
    config.clock = Clock::Simulated {
        dt: Duration::from_secs_f64(DT),
    };
    config.systems.retain(|system| system.ty != "fsw.publish");
    config.systems.push(SystemConfig {
        inputs: vec![
            input("body", PortRef::new("plant", "body")),
            input("estimate", PortRef::new("nav", "attitude_estimate")),
        ],
        ..SystemConfig::new("watch", "watch")
    });
    config.systems.push(SystemConfig {
        inputs: vec![
            input("torque_cmd", PortRef::new("ctrl", "torque_cmd")),
            input("log", PortRef::new("nav", "log")),
        ],
        ..SystemConfig::new("tap", "tap")
    });
    config
}

fn input(port: &str, from: PortRef) -> InputConfig {
    InputConfig {
        port: port.into(),
        from: vec![from],
    }
}

/// The sinks and the built-ins, on whichever table holds the pack's systems.
fn with_sinks(mut table: SystemTable, shared: &Shared) -> SystemTable {
    let watch = shared.clone();
    table
        .register("watch", move || Watch(watch.clone()))
        .expect("valid records");
    let tap = shared.clone();
    table
        .register("tap", move || Tap(tap.clone()))
        .expect("valid records");
    register_builtins(&mut table).expect("valid records");
    table
}

/// The cdylib `pack dev` staged beside the pack's crate.
fn staged() -> PathBuf {
    manifest()
        .join("systems/adcs/.metor/adcs_pack/_libs")
        .join(triple())
        .join(cdylib_name("adcs_systems_3"))
}

/// The loop with the pack's systems linked into the test binary.
fn linked(config: &CoordinatorConfig, shared: &Shared) -> Coordinator {
    let mut config = config.clone();
    for system in &mut config.systems {
        if let Some(ty) = system.ty.strip_prefix("adcs.") {
            system.ty = ty.into();
        }
    }
    config
        .build(&with_sinks(adcs_systems::pack(), shared))
        .expect("the static config builds")
}

/// The loop with the pack's systems loaded from the staged cdylib.
fn loaded(config: &CoordinatorConfig, shared: &Shared, pack: &Pack) -> Coordinator {
    let mut table = with_sinks(SystemTable::new(), shared);
    table.register_pack("adcs", pack).expect("valid records");
    config
        .clone()
        .build(&table)
        .expect("the pack config builds")
}

/// Steps each coordinator for `cycles` cycles of its simulated clock, in the
/// order given; one executor serves the whole test.
fn drive(coordinators: Vec<Coordinator>, cycles: u64) {
    stellarator::run(move || async move {
        for mut coordinator in coordinators {
            coordinator.run(stop(cycles)).await;
            let latched: Vec<&str> = coordinator.latched().collect();
            assert!(latched.is_empty(), "latched off: {latched:?}");
        }
    });
}

fn stop(cycles: u64) -> impl Future<Output = ()> {
    let mut polls = 0;
    poll_fn(move |_| {
        polls += 1;
        match polls >= cycles {
            true => Poll::Ready(()),
            false => Poll::Pending,
        }
    })
}

/// Sends the systems' `tracing` lines to their `log` ports, as `metor run` does.
fn install_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(metor_fsw_3::log::layer());
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// The loop converged onto its commanded target and the filter tracked the truth.
fn assert_converged(label: &str, record: &Record) {
    let (err0, rate0) = record.track.first().copied().expect("a first sample");
    let (err, rate) = record.track.last().copied().expect("a last sample");
    println!(
        "[{label}] tracking error {err0:.4} -> {err:.5} rad ({:.2} deg); \
         rate {rate0:.4} -> {rate:.6} rad/s",
        err.to_degrees()
    );
    let settled = record
        .track
        .iter()
        .position(|(error, _)| *error < ERR_BOUND)
        .expect("the error reached the bound");
    println!("[{label}] first under the bound at cycle {settled}");
    assert!(err0 > 0.3, "[{label}] starts off target: {err0}");
    assert!(err < ERR_BOUND, "[{label}] converges: {err0} -> {err}");
    assert!(rate < RATE_BOUND, "[{label}] damps: {rate0} -> {rate}");
    let estimate = record.estimate.last().copied().expect("a last estimate");
    assert!(
        estimate < ERR_BOUND,
        "[{label}] the filter tracks: {estimate}"
    );
    assert!(
        record
            .logs
            .iter()
            .any(|line| line.contains("first GPS fix")),
        "[{label}] the filter logged its first fix: {:?}",
        record.logs
    );
}

#[test]
fn test_loop_converges_linked_and_loaded() {
    install_logging();

    let config = config();
    // SAFETY: `prepare` refreshed this pack, which this workspace built
    // against this ABI; `open` checks the version before calling into it.
    let pack = unsafe { Pack::open(&staged()) }.expect("the staged pack opens");
    let (linked_run, loaded_run) = (Shared::default(), Shared::default());
    drive(
        vec![
            linked(&config, &linked_run),
            loaded(&config, &loaded_run, &pack),
        ],
        CYCLES,
    );

    let linked = linked_run.take();
    let loaded = loaded_run.take();
    assert_converged("linked", &linked);
    assert_converged("loaded", &loaded);

    assert_eq!(
        linked.torque.len(),
        loaded.torque.len(),
        "the runs commanded the same number of torques"
    );
    assert!(
        linked.torque == loaded.torque,
        "the two command streams differ"
    );
}
