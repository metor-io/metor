//! Checks that `sim.py`'s loop points the spacecraft at nadir, and that the run
//! with the pack linked in commands the same torques as the run with the pack
//! loaded from the cdylib `pack dev` staged.
//!
//! Two more runs swap `mode` for the `commission` sequence, once linked and
//! once loaded from the wasm module `pack dev` staged, and must command the
//! same torques as each other.

use core::cell::RefCell;
use core::future::{Future, poll_fn};
use core::task::Poll;
use core::time::Duration;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Once, OnceLock};

use adcs_contracts::{AttitudeEstimate, BodyState, Law, TorqueCmd, target_for};
use adcs_seqs::Commission;
use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::cli::run::prepare;
use metor_fsw_3::coordinator::{
    BuildOptions, Clock, Coordinator, CoordinatorConfig, EndReason, InputConfig, PortRef,
    SystemConfig, SystemTable,
};
use metor_fsw_3::dl::{PackKind, PackRef, cdylib_name, triple};
use metor_fsw_3::{DlPack, Input, Pack, system};
use metor_proto_wkt::LogEvent;
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt;
use zerocopy::IntoBytes;

/// Cycles per run, 20 s at 120 Hz. The error first drops under the bound near
/// cycle 940, so each run lasts over twice as long as the loop needs.
const CYCLES: u64 = 2400;

/// Cycles per sequenced run, 40 s at 120 Hz. `commission` commands nadir near
/// cycle 1460 and the error first drops under the bound near cycle 2380, so
/// each run lasts about twice as long as the loop needs.
const SEQUENCED_CYCLES: u64 = 4800;

/// The estimated rate (rad/s) under which `commission` ends its detumble. The
/// magnetorquers damp the boot rate of 0.15 rad/s over minutes, so this sits
/// just under it.
const SETTLED_RATE: f64 = 0.14;

/// The longest detumble (s) `commission` waits out, well past its settling.
const DETUMBLE_TIMEOUT_S: f64 = 60.0;

/// The pointing law `mode` commands for the whole run, and `commission` ends on.
const LAW: Law = Law::Nadir;

/// Pointing error bound (rad), about 6°.
const ERR_BOUND: f64 = 0.1;
/// Body rate bound (rad/s).
const RATE_BOUND: f64 = 0.05;

/// What one run recorded.
#[derive(Default)]
struct Record {
    /// Tracking error and body rate, every cycle.
    track: Vec<(f64, f64)>,
    /// Filter error against the true attitude (rad), every cycle.
    estimate: Vec<f64>,
    /// Every commanded torque as raw bytes, without the stamp, which differs
    /// between coordinators.
    torque: Vec<Vec<u8>>,
    /// Every line the filter and the controller logged.
    logs: Vec<String>,
    /// The law each of the controller's "pointing law changed" lines names.
    laws: Vec<String>,
}

type Shared = Rc<RefCell<Record>>;

/// Records tracking and filter error against the plant's true state.
struct Watch(Shared);

#[system]
fn watch(
    #[state] state: &mut Watch,
    body: &mut Input<BodyState>,
    estimate: &mut Input<AttitudeEstimate>,
) {
    let Ok(Some(body)) = body.latest() else {
        return;
    };
    let mut record = state.0.borrow_mut();
    let target = target_for(LAW, &body.pos_eci, &body.vel_eci);
    record.track.push(body.tracking_error(&target));
    if let Ok(Some(estimate)) = estimate.latest() {
        let error = body.q_b_eci.angular_distance(&estimate.q_hat_b_eci);
        record.estimate.push(error.into_buf().abs());
    }
}

/// Records the controller's torque commands, and the filter's and the
/// controller's log lines.
struct Tap(Shared);

#[system]
fn tap(#[state] state: &mut Tap, torque_cmd: &mut Input<TorqueCmd>, log: &mut Input<LogEvent>) {
    let mut record = state.0.borrow_mut();
    for torque in torque_cmd.drain().flatten() {
        record.torque.push(torque.torque_b.as_bytes().to_vec());
    }
    for line in log.drain().flatten() {
        record.laws.extend(law_change(&line));
        record.logs.push(line.message.into_owned());
    }
}

/// The law a "pointing law changed" line names.
fn law_change(line: &LogEvent) -> Option<String> {
    if line.message != "pointing law changed" {
        return None;
    }
    let (_, to) = line.fields.iter().find(|(key, _)| key == "to")?;
    Some(to.trim_matches('"').to_string())
}

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Returns `sim.py`'s coordinator on a simulated clock, with the recorders added
/// and the publish link and db removed so the runs don't contend for their
/// ports.
///
/// `prepare` stages the packs and holds them open, so it runs once per process.
fn config() -> &'static CoordinatorConfig {
    static CONFIG: OnceLock<CoordinatorConfig> = OnceLock::new();
    CONFIG.get_or_init(evaluate)
}

fn evaluate() -> CoordinatorConfig {
    let deployment = prepare(Some(&manifest().join("sim.py"))).expect("sim.py evaluates");
    assert_eq!(deployment.targets.len(), 1, "sim.py is one target");
    let mut config = deployment
        .targets
        .into_iter()
        .next()
        .expect("one target")
        .coordinator;
    config.clock = Clock::Simulated {
        dt: Duration::from_secs_f64(1.0 / 120.0),
        // A fixed epoch (2026-09-21 UTC), as a deterministic build needs.
        epoch: Some(metor_fsw_3::Timestamp(1_790_000_000_000_000)),
    };
    config
        .systems
        .retain(|system| !["fsw.publish", "fsw.db"].contains(&system.ty.as_str()));
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
            InputConfig {
                port: "log".into(),
                from: vec![PortRef::new("nav", "log"), PortRef::new("ctrl", "log")],
            },
        ],
        ..SystemConfig::new("tap", "tap")
    });
    config
}

/// Returns `config` with `mode` swapped for `commission`, which feeds `ctrl` in its place.
fn sequenced(config: &CoordinatorConfig) -> CoordinatorConfig {
    let mut config = config.clone();
    for system in &mut config.systems {
        if system.id == "mode" {
            *system = SystemConfig {
                params: json!({ "rate": SETTLED_RATE, "detumble_timeout_s": DETUMBLE_TIMEOUT_S }),
                inputs: vec![input("estimate", PortRef::new("nav", "attitude_estimate"))],
                ..SystemConfig::new("commission", "seqs.commission")
            };
        }
        let sources = system.inputs.iter_mut().flat_map(|input| &mut input.from);
        for from in sources.filter(|from| from.system == "mode") {
            from.system = "commission".into();
        }
    }
    config
}

fn input(port: &str, from: PortRef) -> InputConfig {
    InputConfig {
        port: port.into(),
        from: vec![from],
    }
}

/// Registers the recorders and the built-in systems on `table`.
fn with_sinks(mut table: SystemTable, shared: &Shared) -> SystemTable {
    let shared_watch = shared.clone();
    table
        .register("watch", watch(), move || Watch(shared_watch.clone()))
        .expect("valid records");
    let shared_tap = shared.clone();
    table
        .register("tap", tap(), move || Tap(shared_tap.clone()))
        .expect("valid records");
    register_builtins(&mut table).expect("valid records");
    table
}

/// Opens the `adcs` cdylib `pack dev` staged beside the pack's crate.
fn staged_library() -> Arc<Pack> {
    let path = manifest()
        .join("systems/adcs/.metor/adcs_pack/_libs")
        .join(triple())
        .join(cdylib_name("adcs_systems_3"));
    // SAFETY: `prepare` just built this pack from this workspace against this
    // ABI, and `open` checks the version before calling into it.
    let pack = unsafe { DlPack::open(&path) }.expect("the staged pack opens");
    Arc::new(Pack::Dl(pack))
}

/// Opens the `startracker_sim` cdylib `pack dev` staged beside the pack's crate.
fn staged_star_sim() -> Arc<Pack> {
    let path = manifest()
        .join("systems/startracker-sim/.metor/startracker_sim_pack/_libs")
        .join(triple())
        .join(cdylib_name("startracker_sim_3"));
    // SAFETY: as for `staged_library`.
    let pack = unsafe { DlPack::open(&path) }.expect("the staged pack opens");
    Arc::new(Pack::Dl(pack))
}

/// Opens the `seqs` module `pack dev` staged beside the pack's crate.
fn staged_module() -> Arc<Pack> {
    let reference = PackRef {
        kind: PackKind::Wasm(Default::default()),
        ..PackRef::dylib(
            "seqs".into(),
            "adcs_seqs_3".into(),
            manifest().join("systems/seqs/.metor/adcs_seqs/_libs"),
        )
    };
    Arc::new(reference.open().expect("the staged module opens"))
}

/// Builds the loop with both packs linked into the test binary.
fn linked(config: &CoordinatorConfig, shared: &Shared) -> Coordinator {
    let mut config = config.clone();
    for system in &mut config.systems {
        let pack_ty = ["adcs.", "seqs.", "startracker_sim."]
            .iter()
            .find_map(|prefix| system.ty.strip_prefix(prefix));
        if let Some(ty) = pack_ty {
            system.ty = ty.into();
        }
    }
    let mut table = adcs_systems::pack();
    table
        .register_async(
            "star_sim",
            startracker_sim::system::star_sim(),
            startracker_sim::StarSim::new,
        )
        .expect("valid records");
    table
        .register_sequence(
            "commission",
            adcs_seqs::commission(),
            core::convert::identity::<Commission>,
        )
        .expect("valid records");
    config
        .build_with(&with_sinks(table, shared), deterministic())
        .expect("the static config builds")
}

/// Turns the star simulator's thread on the coordinator thread, so its
/// images, and so the tracker's vectors and nav, are the same in every run.
fn deterministic() -> BuildOptions {
    BuildOptions {
        deterministic: true,
    }
}

/// Builds the loop with `packs` loaded, each under its id.
fn loaded(
    config: &CoordinatorConfig,
    shared: &Shared,
    packs: &[(&str, &Arc<Pack>)],
) -> Coordinator {
    let mut table = with_sinks(SystemTable::new(), shared);
    for (id, pack) in packs {
        table
            .register_pack(id, (*pack).clone())
            .expect("valid records");
    }
    config
        .clone()
        .build_with(&table, deterministic())
        .expect("the pack config builds")
}

/// Runs each coordinator for `cycles` simulated cycles, one after another on a
/// single executor, and returns the steps each one saw end.
fn drive(coordinators: Vec<Coordinator>, cycles: u64) -> Vec<Vec<(String, EndReason)>> {
    stellarator::run(move || async move {
        let mut ended = Vec::new();
        for mut coordinator in coordinators {
            coordinator.run(stop(cycles)).await;
            let steps: Vec<(String, EndReason)> = coordinator
                .systems()
                .filter_map(|(id, why)| Some((id.into(), why?.clone())))
                .collect();
            let latched: Vec<&str> = steps
                .iter()
                .filter(|(_, why)| *why == EndReason::Panicked)
                .map(|(id, _)| id.as_str())
                .collect();
            assert!(latched.is_empty(), "latched off: {latched:?}");
            ended.push(steps);
        }
        ended
    })
}

/// Returns a future that resolves on its `cycles`th poll.
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

/// Routes the systems' `tracing` lines to their `log` ports, as `metor run` does.
fn install_logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::registry().with(metor_fsw_3::LogLayer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

/// Asserts the loop converged onto its target and the filter tracked the true attitude.
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

/// Asserts two runs commanded the same torques.
fn assert_same_torques(a: &Record, b: &Record) {
    assert_eq!(
        a.torque.len(),
        b.torque.len(),
        "the runs commanded the same number of torques"
    );
    let first = a.torque.iter().zip(&b.torque).position(|(x, y)| x != y);
    assert!(
        a.torque == b.torque,
        "the two command streams differ at {first:?}"
    );
}

#[test]
fn test_loop_converges_linked_and_loaded() {
    install_logging();

    let config = config();
    let (adcs, star) = (staged_library(), staged_star_sim());
    let (linked_run, loaded_run) = (Shared::default(), Shared::default());
    drive(
        vec![
            linked(config, &linked_run),
            loaded(
                config,
                &loaded_run,
                &[("adcs", &adcs), ("startracker_sim", &star)],
            ),
        ],
        CYCLES,
    );

    let linked = linked_run.take();
    let loaded = loaded_run.take();
    assert_converged("linked", &linked);
    assert_converged("loaded", &loaded);
    assert_same_torques(&linked, &loaded);
}

#[test]
fn test_sequenced_loop_converges_linked_and_module() {
    install_logging();

    let config = sequenced(config());
    let (adcs, seqs, star) = (staged_library(), staged_module(), staged_star_sim());
    let (linked_run, module_run) = (Shared::default(), Shared::default());
    let ended = drive(
        vec![
            linked(&config, &linked_run),
            loaded(
                &config,
                &module_run,
                &[("adcs", &adcs), ("seqs", &seqs), ("startracker_sim", &star)],
            ),
        ],
        SEQUENCED_CYCLES,
    );

    let linked = linked_run.take();
    let module = module_run.take();
    assert_converged("sequenced (linked)", &linked);
    assert_converged("sequenced (module)", &module);
    assert_same_torques(&linked, &module);

    let completed = ("commission".to_string(), EndReason::Completed);
    assert_eq!(ended[1], [completed], "commission completed in the module");
    assert_eq!(
        module.laws,
        ["detumble", "nadir"],
        "ctrl detumbled, then pointed"
    );
}
