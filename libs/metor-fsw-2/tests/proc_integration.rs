//! End-to-end tests of cross-process systems (`docs/process-systems.md`).
//!
//! This binary is its own worker executable: `main` calls
//! [`proc::worker_entry`] before anything else, so the coordinator's
//! re-exec'd children route into the worker instead of the test harness —
//! which is why this target sets `harness = false` (a libtest `main` cannot
//! take the guard). The tests run sequentially in-order.
//!
//! Coverage: a describe worker feeding `add_proc_cyclic` (the host never
//! dlopens the artifact), mmap rings + doorbell lockstep against a static
//! producer and an in-process dl twin, telemetry taps on the worker's
//! outputs (user frame, Postcard events, implicit health), clean teardown,
//! and a SIGKILL'd worker: `StopReason::ProcessDied`, ring reclamation, and
//! the rest of the graph flowing on.
//!
//! The fixture cdylib is `metor-fsw-2-dl-fixture`, built by a nested cargo
//! invocation exactly as in `dl_integration.rs`; if it cannot be produced
//! the tests skip with a message rather than fail.

use std::path::{Path, PathBuf};
use std::process::Command;

use metor_fsw_2::metor_proto::types::{ComponentId, Timestamp};
use metor_fsw_2::{
    ClockMode, Coordinator, CoordinatorConfig, CyclicSystem, Frame, Input, MsgIn, Out, Output,
    PortRef, StopReason, System, SystemHealth, SystemInput, SystemOutput, WorkerRunState,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

fn main() {
    // The whole point of this harness: a spawned worker child never reaches
    // the tests below.
    metor_fsw_2::proc::worker_entry();
    let Some(lib_path) = locate_fixture() else {
        eprintln!("skipping proc_integration: fixture build unavailable");
        return;
    };
    // Each test on its own thread, sequentially: `stellarator::run` consumes
    // the thread-local executor, so one thread cannot host two runs (libtest
    // gives every #[test] its own thread for the same reason).
    let run = |name: &str, f: fn(&Path), lib: &Path| {
        let lib = lib.to_path_buf();
        std::thread::spawn(move || f(&lib)).join().unwrap();
        println!("proc_integration::{name} ... ok");
    };
    run("lockstep_end_to_end", lockstep_end_to_end, &lib_path);
    run(
        "death_reclaims_and_keeps_flowing",
        death_reclaims_and_keeps_flowing,
        &lib_path,
    );
    run(
        "worker_restarts_then_exhausts_budget",
        worker_restarts_then_exhausts_budget,
        &lib_path,
    );
}

// ---------------------------------------------------------------------------
// Host-side mirrors of the fixture's frames/params/messages
// (byte-identical to `tests/fixtures/dl-fixture`).
// ---------------------------------------------------------------------------

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_in")]
struct TickIn {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    value: u64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "tick_out")]
struct TickOut {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    count: u64,
}

#[derive(serde::Serialize, Default)]
struct CounterParams {
    start: u64,
    scale: f64,
}

#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug)]
struct TickEvent {
    count: u64,
}

/// Emits `tick_in.value = 1, 2, 3, ...` (see `dl_integration.rs`).
struct Ticker {
    n: u64,
}

#[derive(SystemInput)]
struct TickerIn {}

#[derive(SystemOutput)]
struct TickerOut {
    tick: Output<TickIn>,
}

impl System for Ticker {
    type Input = TickerIn;
    type Output = Out<TickerOut>;
    const NAME: &'static str = "ticker";
}

impl CyclicSystem for Ticker {
    fn execute(&mut self, now: Timestamp, _input: &mut TickerIn, output: &mut Out<TickerOut>) {
        self.n += 1;
        let _ = output.tick.write(&TickIn {
            timestamp: now,
            value: self.n,
        });
    }
}

// ---------------------------------------------------------------------------
// Fixture build/locate (as in dl_integration.rs)
// ---------------------------------------------------------------------------

fn fixture_lib_name() -> String {
    let stem = "metor_fsw_2_dl_fixture";
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

fn locate_fixture() -> Option<PathBuf> {
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "metor-fsw-2-dl-fixture",
            "--message-format=json",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "skipping: fixture build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let want = fixture_lib_name();
    for line in stdout.lines() {
        if !line.contains("compiler-artifact") || !line.contains(&want) {
            continue;
        }
        for tok in line.split('"') {
            if tok.ends_with(&want) {
                let path = PathBuf::from(tok);
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    eprintln!("skipping: built the fixture but could not locate {want} in cargo output");
    None
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The descriptor exactly as resolve obtains it: a describe-mode worker run
/// (this very binary, re-executed) plus the public wire-mirror decode. The
/// test host never dlopens the artifact for its process system.
fn proc_descriptor(lib_path: &Path) -> metor_fsw_2::SystemDescriptor {
    let bytes = metor_fsw_2::proc::describe_via_worker(None, lib_path)
        .expect("describe worker over the fixture");
    let msg: metor_fsw_2::abi::SystemDescriptorMsg =
        postcard::from_bytes(&bytes).expect("descriptor bytes decode");
    msg.into_descriptor()
}

fn counter_params() -> Vec<u8> {
    postcard::to_allocvec(&CounterParams {
        start: 1000,
        scale: 1.0,
    })
    .unwrap()
}

/// Pids of this process's direct *worker* children, via `ps` (the worker
/// `Child` is private to the coordinator, so the test finds it the
/// operator's way). A worker is a re-exec of this very binary, so children
/// are filtered by our own executable name — which also excludes the `ps`
/// snapshot process itself, a momentary child of ours.
fn child_pids() -> Vec<u32> {
    let out = Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm="])
        .output()
        .expect("ps runs");
    let me = std::process::id();
    let exe = std::env::current_exe().expect("current_exe");
    let exe_name = exe.file_name().unwrap().to_string_lossy().into_owned();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let pid: u32 = cols.next()?.parse().ok()?;
            let ppid: u32 = cols.next()?.parse().ok()?;
            let comm = cols.next_back()?;
            (ppid == me && comm.ends_with(&exe_name)).then_some(pid)
        })
        .collect()
}

fn kill9(pid: u32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

// ---------------------------------------------------------------------------
// Test 1: lockstep end to end, mixed with an in-process dl twin
// ---------------------------------------------------------------------------

fn lockstep_end_to_end(lib_path: &Path) {
    let desc = proc_descriptor(lib_path);
    assert_eq!(desc.name, "dl_counter");
    assert_eq!(desc.inputs.len(), 1, "one user input (tick_in)");
    assert_eq!(desc.outputs.len(), 4, "out + events + health + log");

    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 200.0,
        default_depth: 8,
        clock: ClockMode::Simulated {
            dt: std::time::Duration::from_millis(5),
        },
        ..CoordinatorConfig::default()
    });
    let ticker = b.add_cyclic_named("ticker", Ticker { n: 0 });
    // The process system, plus the same fixture dlopen'd in-process: all
    // three loading modes coexist in one graph over one producer.
    let proc_sys = b.add_proc_cyclic(
        "proc_counter",
        desc,
        lib_path.to_path_buf(),
        counter_params(),
    );
    let dl_twin = metor_fsw_2::DlSystem::open(lib_path).expect("dlopen the fixture in-process");
    let dl_sys = b.add_dl_cyclic("dl_obs", dl_twin, counter_params());
    b.connect(
        PortRef::new::<TickIn>(ticker),
        PortRef::new::<TickIn>(proc_sys),
    )
    .expect("ticker -> proc edge");
    b.connect(
        PortRef::new::<TickIn>(ticker),
        PortRef::new::<TickIn>(dl_sys),
    )
    .expect("ticker -> dl edge");

    // build() spawns the worker and waits for it to attach.
    let mut coord = b.build().expect("build spawns and attaches the worker");
    let spawned = child_pids();
    assert_eq!(spawned.len(), 1, "exactly one worker child: {spawned:?}");

    // Taps over the worker's outputs, through the same registry as any
    // system's: the user frame, the Postcard events, and the implicit health
    // the worker's own CyclicRunner publishes from the other process.
    let registry = coord.registry();
    let mut out_view: Input<TickOut> = Input::new(
        registry
            .view(ComponentId::new("proc_counter.tick_out"))
            .expect("proc output registered")
            .expect("reader slot available"),
    );
    let mut events_in: MsgIn<TickEvent> = MsgIn::new(
        registry
            .view(ComponentId::new("proc_counter.TickEvent"))
            .expect("proc message channel registered")
            .expect("reader slot available"),
    );
    let mut health_view: Input<SystemHealth> = Input::new(
        registry
            .view(ComponentId::new("proc_counter.health"))
            .expect("proc health registered")
            .expect("reader slot available"),
    );
    let mut dl_out_view: Input<TickOut> = Input::new(
        registry
            .view(ComponentId::new("dl_obs.tick_out"))
            .expect("dl output registered")
            .expect("reader slot available"),
    );

    const CYCLES: usize = 6;
    let coord = stellarator::run(|| async move {
        coord.run_for(CYCLES).await;
        coord
    });

    assert!(coord.stopped().is_empty(), "nothing stopped: {:?}", coord.stopped());
    // The status surface names the worker process: the coordinator's worker
    // list carries the process system's pid and run state (its in-process
    // dl twin appears nowhere here).
    let workers = coord.workers();
    assert_eq!(workers.len(), 1, "one process system: {workers:?}");
    assert_eq!(workers[0].name, "proc_counter");
    assert_eq!(workers[0].pid, spawned[0], "the telemetered pid is the child's");
    assert_eq!(workers[0].restarts, 0);
    assert_eq!(workers[0].state, WorkerRunState::Running);
    // The worker consumed each cycle's fresh tick in lockstep, exactly like
    // its in-process dl twin.
    let out = out_view.latest().expect("worker produced tick_out");
    assert_eq!(out.get().count, 1000 + CYCLES as u64, "start + latest value");
    drop(out);
    let dl_out = dl_out_view.latest().expect("dl twin produced tick_out");
    assert_eq!(dl_out.get().count, 1000 + CYCLES as u64, "dl twin agrees");
    drop(dl_out);
    // Every cycle's event crossed the process boundary, in order.
    let mut counts: Vec<u64> = Vec::new();
    events_in.drain(|e| counts.push(e.count));
    assert_eq!(
        counts,
        (1..=CYCLES as u64).map(|v| 1000 + v).collect::<Vec<_>>(),
        "every-record log semantics across the process boundary"
    );
    // The implicit health frame flowed from the worker process.
    let health = health_view.latest().expect("worker health flowed");
    assert_eq!(health.get().errors, 0, "no worker-side errors");
    assert!(health.get().cycles >= CYCLES as u64, "worker counted its cycles");
    drop(health);

    // Teardown: shutdown reaps the worker; dropping the coordinator unmaps
    // the rings and removes the session dir. Not crashing (and not leaking a
    // child) is the assertion.
    drop(coord);
    drop((out_view, events_in, health_view, dl_out_view));
    assert!(
        child_pids().is_empty(),
        "no worker child survives a clean teardown"
    );
}

// ---------------------------------------------------------------------------
// Test 2: SIGKILL the worker mid-run
// ---------------------------------------------------------------------------

fn death_reclaims_and_keeps_flowing(lib_path: &Path) {
    let desc = proc_descriptor(lib_path);
    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 200.0,
        default_depth: 8,
        clock: ClockMode::Wall,
        proc_step_timeout: std::time::Duration::from_millis(50),
        // Restart opted out: this test pins the permanent-stop semantics.
        proc_max_restarts: 0,
        ..CoordinatorConfig::default()
    });
    let ticker = b.add_cyclic_named("ticker", Ticker { n: 0 });
    let proc_sys = b.add_proc_cyclic(
        "proc_counter",
        desc,
        lib_path.to_path_buf(),
        counter_params(),
    );
    b.connect(
        PortRef::new::<TickIn>(ticker),
        PortRef::new::<TickIn>(proc_sys),
    )
    .expect("ticker -> proc edge");
    let mut coord = b.build().expect("build spawns and attaches the worker");

    // The just-spawned worker is our only child; kill -9 it mid-run.
    let workers = child_pids();
    assert_eq!(workers.len(), 1, "exactly one worker child: {workers:?}");
    let victim = workers[0];
    let killer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let _ = Command::new("kill")
            .args(["-9", &victim.to_string()])
            .status();
    });

    let registry = coord.registry();
    let mut ticker_health: Input<SystemHealth> = Input::new(
        registry
            .view(ComponentId::new("ticker.health"))
            .expect("ticker health registered")
            .expect("reader slot available"),
    );

    // 150 cycles at 200 Hz ≈ 750 ms: the kill lands around cycle 50, and the
    // loop must keep pace afterwards instead of hanging on a dead worker.
    let coord = stellarator::run(|| async move {
        coord.run_for(150).await;
        coord
    });
    killer.join().unwrap();

    // The death is a permanent, telemetered stop...
    let stopped = coord.stopped();
    assert_eq!(stopped.len(), 1, "the killed worker is reported: {stopped:?}");
    assert_eq!(stopped[0].name, "proc_counter");
    assert_eq!(stopped[0].reason, StopReason::ProcessDied);
    // ...and the worker list reflects it: no live pid, no restarts granted.
    let workers = coord.workers();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].state, WorkerRunState::Stopped, "{workers:?}");
    assert_eq!(workers[0].pid, 0, "no live worker behind the slot");
    assert_eq!(workers[0].restarts, 0, "restart was opted out");
    // ...whose reader cursors were reclaimed: the producer never saw a full
    // ring (a dead pinned cursor would have surfaced as publish_dropped
    // errors on the ticker's health well within ~100 post-kill cycles).
    let health = ticker_health.latest().expect("ticker health flowed");
    assert_eq!(
        health.get().errors,
        0,
        "producer kept publishing after the reclaim"
    );
    drop(health);

    drop(coord);
    drop(ticker_health);
    assert!(child_pids().is_empty(), "the dead worker was reaped");
}

// ---------------------------------------------------------------------------
// Test 3: restart — recover from one kill, go terminal past the budget
// ---------------------------------------------------------------------------

fn worker_restarts_then_exhausts_budget(lib_path: &Path) {
    let desc = proc_descriptor(lib_path);
    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 200.0,
        default_depth: 8,
        clock: ClockMode::Wall,
        proc_step_timeout: std::time::Duration::from_millis(50),
        proc_max_restarts: 1,
        proc_restart_backoff: std::time::Duration::from_millis(50),
        ..CoordinatorConfig::default()
    });
    let ticker = b.add_cyclic_named("ticker", Ticker { n: 0 });
    let proc_sys = b.add_proc_cyclic(
        "proc_counter",
        desc,
        lib_path.to_path_buf(),
        counter_params(),
    );
    b.connect(
        PortRef::new::<TickIn>(ticker),
        PortRef::new::<TickIn>(proc_sys),
    )
    .expect("ticker -> proc edge");
    let mut coord = b.build().expect("build spawns and attaches the worker");
    let first = child_pids();
    assert_eq!(first.len(), 1, "exactly one worker child: {first:?}");
    let first_pid = first[0];

    // Kill the worker, wait for its replacement (the restart pipeline), let
    // the replacement produce for a while, then kill it too — past the
    // budget of 1, so the second death must be terminal. The join propagates
    // the "replacement appeared" assertion, which is the recovery proof.
    let killer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(250));
        kill9(first_pid);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let replacement = loop {
            if let Some(pid) = child_pids().into_iter().find(|&p| p != first_pid) {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no replacement worker appeared after the kill"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        kill9(replacement);
    });

    let registry = coord.registry();
    let mut ticker_health: Input<SystemHealth> = Input::new(
        registry
            .view(ComponentId::new("ticker.health"))
            .expect("ticker health registered")
            .expect("reader slot available"),
    );
    let mut out_view: Input<TickOut> = Input::new(
        registry
            .view(ComponentId::new("proc_counter.tick_out"))
            .expect("proc output registered")
            .expect("reader slot available"),
    );

    // 400 cycles at 200 Hz = 2 s: room for kill → backoff → respawn →
    // re-init → produce → second kill → terminal.
    let coord = stellarator::run(|| async move {
        coord.run_for(400).await;
        coord
    });
    killer.join().unwrap();

    // Terminal: the second death exhausted the budget of one restart.
    let stopped = coord.stopped();
    assert_eq!(stopped.len(), 1, "terminal stop reported: {stopped:?}");
    assert_eq!(stopped[0].reason, StopReason::ProcessDied);
    let workers = coord.workers();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].restarts, 1, "exactly one restart was granted: {workers:?}");
    assert_eq!(workers[0].state, WorkerRunState::Stopped);
    assert_eq!(workers[0].pid, 0, "no live worker behind the slot");
    // The replacement produced (any output at all proves the restarted
    // worker re-attached the same rings and resumed the lockstep)...
    assert!(
        out_view.latest().expect("output flowed").get().count > 1000,
        "the restarted worker produced"
    );
    // ...and both reclaims kept the producer flowing: no publish errors.
    let health = ticker_health.latest().expect("ticker health flowed");
    assert_eq!(health.get().errors, 0, "producer never backpressured");
    drop(health);

    drop(coord);
    drop((ticker_health, out_view));
    assert!(child_pids().is_empty(), "all workers reaped");
}
