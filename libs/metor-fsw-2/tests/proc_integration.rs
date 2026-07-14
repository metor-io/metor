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
//! the rest of the graph flowing on. The second half covers **process-mode
//! slots** (`docs/process-slots.md`): the polled Load pipeline and occupant
//! swap over one ring set, worker death folding to a terminal stop with an
//! operator `Reset` recovery, the cooperative abort crossing the process
//! boundary, and the isolation canary proving the host never maps an
//! occupant artifact.
//!
//! The fixture cdylibs are `metor-fsw-2-dl-fixture` and
//! `metor-fsw-2-seq-fixture`, built by nested cargo invocations exactly as
//! in `dl_integration.rs`; if one cannot be produced the tests skip with a
//! message rather than fail.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use metor_fsw_2::metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_fsw_2::metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};
use metor_fsw_2::{
    BuildSystem, ClockMode, Coordinator, CoordinatorConfig, CyclicSystem, Frame, Input, MsgIn, Out,
    Output, PortRef, SequenceStatus, SlotStatus, StopReason, System, SystemHealth, SystemInput,
    SystemOutput, WorkerRunState, split_record,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

fn main() {
    // The whole point of this harness: a spawned worker child never reaches
    // the tests below.
    metor_fsw_2::proc::worker_entry();
    let Some(lib_path) = locate_fixture("metor-fsw-2-dl-fixture", "metor_fsw_2_dl_fixture") else {
        eprintln!("skipping proc_integration: fixture build unavailable");
        return;
    };
    let Some(seq_lib) = locate_fixture("metor-fsw-2-seq-fixture", "metor_fsw_2_seq_fixture")
    else {
        eprintln!("skipping proc_integration: seq fixture build unavailable");
        return;
    };
    // The isolation canary (see the seq fixture): every process that maps the
    // occupant artifact appends its pid to this file, workers included, since
    // they inherit the environment. Set once, before any coordinator exists,
    // while this process is still single-threaded.
    let canary = std::env::temp_dir().join(format!("seq-fixture-canary-{}", std::process::id()));
    let _ = std::fs::remove_file(&canary);
    // SAFETY: no other thread is running yet.
    unsafe { std::env::set_var("SEQ_FIXTURE_CANARY", &canary) };
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
    run(
        "proc_slot_lifecycle_swap_and_isolation",
        proc_slot_lifecycle_swap_and_isolation,
        &seq_lib,
    );
    run(
        "proc_slot_death_is_terminal_and_reset_recovers",
        proc_slot_death_is_terminal_and_reset_recovers,
        &seq_lib,
    );
    run(
        "proc_slot_abort_crosses_the_boundary",
        proc_slot_abort_crosses_the_boundary,
        &seq_lib,
    );
    let _ = std::fs::remove_file(&canary);
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

impl BuildSystem for Ticker {
    type Params = ();
    fn new(_params: ()) -> Self {
        Ticker { n: 0 }
    }
}

// ---------------------------------------------------------------------------
// Fixture build/locate (as in dl_integration.rs)
// ---------------------------------------------------------------------------

fn fixture_lib_name(stem: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("lib{stem}.dylib")
    } else if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else {
        format!("lib{stem}.so")
    }
}

fn locate_fixture(package: &str, stem: &str) -> Option<PathBuf> {
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", package, "--message-format=json"])
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
    let want = fixture_lib_name(stem);
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
/// (this very binary, re-executed) plus the public wire-mirror decode of the
/// pack manifest, selecting the entry named `system`. The test host never
/// dlopens the artifact for its process system.
fn proc_descriptor(lib_path: &Path, system: &str) -> metor_fsw_2::SystemDescriptor {
    let bytes = metor_fsw_2::proc::describe_via_worker(None, lib_path)
        .expect("describe worker over the fixture");
    let manifest: metor_fsw_2::abi::PackManifest =
        postcard::from_bytes(&bytes).expect("pack manifest bytes decode");
    manifest
        .systems
        .into_iter()
        .find(|s| s.descriptor.name == system)
        .unwrap_or_else(|| panic!("the pack exports `{system}`"))
        .descriptor
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
    use metor_fsw_2::wiring::{Registry, resolve};
    use metor_fsw_2::{ClockSpec, CoordinatorSpec, WiringBuilder};

    let desc = proc_descriptor(lib_path, "DlCounter");
    assert_eq!(desc.name, "DlCounter");
    assert_eq!(desc.inputs.len(), 1, "one user input (tick_in)");
    assert_eq!(desc.outputs.len(), 4, "out + events + health + log");

    // A static producer, the same fixture entry run as a process system, and
    // the same entry dlopen'd in-process: all three loading modes coexist in
    // one graph over one producer. Resolve describes the process system via a
    // worker (never dlopening it here) and opens the in-process one directly.
    let mut wiring = WiringBuilder::new()
        .coordinator_spec(CoordinatorSpec {
            cycle_rate: 200.0,
            default_depth: Some(8),
            clock: ClockSpec::Simulated { dt_secs: 0.005 },
        })
        .artifact("counter", "metor-fsw-2-dl-fixture", "metor_fsw_2_dl_fixture")
        .system("ticker")
        .ty("Ticker")
        .from_static()
        .end()
        .system("proc_counter")
        .ty("DlCounter")
        .from_artifact("counter")
        .process()
        .params(CounterParams {
            start: 1000,
            scale: 1.0,
        })
        .end()
        .system("dl_obs")
        .ty("DlCounter")
        .from_artifact("counter")
        .params(CounterParams {
            start: 1000,
            scale: 1.0,
        })
        .end()
        .connect("ticker", "tick_in", "proc_counter", "tick_in")
        .connect("ticker", "tick_in", "dl_obs", "tick_in")
        .build();
    // `main` already built and located the fixture; fill the path in place of
    // the build driver.
    wiring.artifacts[0].path = Some(lib_path.to_path_buf());
    let mut registry = Registry::new();
    registry.register::<Ticker, _>("Ticker");

    // resolve's `build()` spawns the worker and waits for it to attach.
    let mut coord = resolve(&wiring, &registry).expect("resolve spawns and attaches the worker");
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
    assert_eq!(&*workers[0].name, "proc_counter");
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
//
// Tests 2 and 3 stay on `Coordinator::builder`: they pin the worker
// restart/timeout policy (`proc_max_restarts`, `proc_step_timeout`,
// `proc_restart_backoff`), which lives on `CoordinatorConfig` but has no mirror
// on the `Wiring` IR's `CoordinatorSpec`, so the resolve path cannot set it.
// They can only move off the builder once the IR carries that policy (a WP4
// concern); see the WP3 report.

fn death_reclaims_and_keeps_flowing(lib_path: &Path) {
    let desc = proc_descriptor(lib_path, "DlCounter");
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
        "DlCounter",
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
    assert_eq!(&*stopped[0].name, "proc_counter");
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
    let desc = proc_descriptor(lib_path, "DlCounter");
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
        "DlCounter",
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

// ---------------------------------------------------------------------------
// Process-mode slots (docs/process-slots.md)
// ---------------------------------------------------------------------------

// SlotState wire codes (SlotState::code): Empty=0, Loaded=1, Loading=2, Running=3, Done=4, Stopped=5.
const LOADED: u8 = 1;
const LOADING: u8 = 2;
const RUNNING: u8 = 3;
const DONE: u8 = 4;
const SLOT_STOPPED: u8 = 5;
// Terminal SequenceStatus run_state codes.
const ABORTED: u8 = 2;

/// A command addressed to the slot channel `ch`.
fn seq_cmd(ch: &str, command: SequenceCommandKind) -> SequenceCommand {
    SequenceCommand {
        channel: ch.to_string(),
        command,
    }
}

fn seq_load(ch: &str, occupant: &str) -> SequenceCommand {
    seq_cmd(
        ch,
        SequenceCommandKind::Load {
            name: occupant.to_string(),
        },
    )
}

/// Drain a message ring, decoding every record as one `Msg` type.
fn drain_msgs<M: Msg + serde::de::DeserializeOwned>(
    view: &mut metor_fsw_2::ring::View<metor_fsw_2::ring::NoWake>,
) -> Vec<M> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    while view
        .try_read_into(&mut buf)
        .expect("no lap on the message tap")
    {
        let (id, payload) = split_record(&buf).expect("a 2-byte-id record");
        assert_eq!(id, M::ID, "every record on this channel carries M::ID");
        out.push(postcard::from_bytes::<M>(payload).expect("postcard round-trip"));
    }
    out
}

/// Await phase `want` on the slot's status stream, appending every newly
/// published phase to `phases` with consecutive repeats deduplicated (the
/// frame is republished each cycle). Gives up at `deadline` and answers
/// `false`, so a wedged run fails the driver's assertion with the collected
/// phase walk instead of hanging the executor.
async fn wait_phase(
    view: &mut Input<SlotStatus>,
    phases: &mut Vec<u8>,
    want: u8,
    deadline: Instant,
) -> bool {
    let from = phases.len();
    loop {
        view.drain(|f| {
            let p = f.get().phase;
            if phases.last() != Some(&p) {
                phases.push(p);
            }
        })
        .expect("no lap on the slot status tap");
        if phases[from..].contains(&want) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        stellarator::yield_now().await;
    }
}

/// `want` appears within `walk` in order (not necessarily adjacent).
fn is_subsequence(walk: &[u8], want: &[u8]) -> bool {
    let mut it = walk.iter();
    want.iter().all(|w| it.any(|p| p == w))
}

/// A compact comparable token per event kind (`SequenceEventKind` derives no
/// `PartialEq`).
fn event_token(kind: &SequenceEventKind) -> String {
    match kind {
        SequenceEventKind::Loaded { name } => format!("loaded:{name}"),
        SequenceEventKind::Unloaded => "unloaded".to_string(),
        SequenceEventKind::Started => "started".to_string(),
        SequenceEventKind::Progress { detail } => format!("progress:{detail}"),
        SequenceEventKind::Stopped => "stopped".to_string(),
        SequenceEventKind::Aborted => "aborted".to_string(),
        SequenceEventKind::Completed => "completed".to_string(),
        SequenceEventKind::Failed { reason } => format!("failed:{reason}"),
        SequenceEventKind::Loading { name } => format!("loading:{name}"),
        SequenceEventKind::Refused { reason } => format!("refused:{reason}"),
    }
}

/// A simulated clock advancing 1 ns per cycle, so the waiter fixture's 2 µs
/// wait spans ~2000 cycles — a wide mid-run window to kill or abort into
/// (under a wall clock it elapses within one cycle). The `Wiring` IR carries
/// no `proc_step_timeout`, so the process-slot tests run on the coordinator
/// default; death detection is a touch slower but well inside their deadlines.
fn slow_sim_coordinator() -> metor_fsw_2::CoordinatorSpec {
    metor_fsw_2::CoordinatorSpec {
        cycle_rate: 1000.0,
        default_depth: Some(8),
        clock: metor_fsw_2::ClockSpec::Simulated { dt_secs: 1e-9 },
    }
}

// ---------------------------------------------------------------------------
// Slot test 1: lifecycle + occupant swap through the KDL front-end, plus the
// isolation canary
// ---------------------------------------------------------------------------

fn proc_slot_lifecycle_swap_and_isolation(seq_lib: &Path) {
    use metor_fsw_2::wiring::{Registry, resolve};
    use metor_fsw_2::{ClockSpec, WiringBuilder};

    // Two entries of the one pack (the fixture registers the same body as
    // `waiter` and `napper`): two allowed occupants with distinct Load names
    // and identical contracts. `process()` runs each occupant in its own
    // worker, and the msg edge carries the in-proc control handle's commands.
    let mut wiring = WiringBuilder::new()
        .coordinator(500.0, ClockSpec::Wall)
        .artifact("seqs", "metor-fsw-2-seq-fixture", "metor_fsw_2_seq_fixture")
        .slot("adcs")
        .process()
        .allow("waiter")
        .allow("napper")
        .end()
        .connect_msg("coordinator", "adcs", "SequenceCommand")
        .build();
    // `main` already built and located the fixture; fill the paths in place
    // of the build driver.
    for artifact in &mut wiring.artifacts {
        artifact.path = Some(seq_lib.to_path_buf());
    }
    let mut coord =
        resolve(&wiring, &Registry::new()).expect("resolve describes each occupant via a worker");
    // Resolve ran short-lived describe workers only, and `build()` spawned
    // nothing either: a process slot spawns one worker per Load.
    assert!(child_pids().is_empty(), "no worker child before the first Load");

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status registered")
            .expect("reader slot available"),
    );
    let mut events_view = coord
        .registry()
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");
    let mut control = coord.control_handle().expect("taken once per coordinator");

    // Load waiter through the polled pipeline, run it to Done, then Load
    // napper over the terminal — the occupant swap — and run it to Done over
    // the very same rings. Wall clock: the pipeline phases are real
    // spawn+dlopen time, and the waiter's 2 µs wait elapses within a cycle.
    let (coord, phases, pid_a, pid_b) = stellarator::run(|| async move {
        let driver = stellarator::spawn(async move {
            let mut phases: Vec<u8> = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(20);
            control.emit(&seq_load("adcs", "waiter")).unwrap();
            assert!(
                wait_phase(&mut slot_view, &mut phases, LOADED, deadline).await,
                "waiter's pipeline reached Loaded: {phases:?}"
            );
            let pid_a = *child_pids().first().expect("waiter's worker is live");
            control.emit(&seq_cmd("adcs", SequenceCommandKind::Start)).unwrap();
            assert!(
                wait_phase(&mut slot_view, &mut phases, DONE, deadline).await,
                "waiter ran to Done: {phases:?}"
            );
            control.emit(&seq_load("adcs", "napper")).unwrap();
            assert!(
                wait_phase(&mut slot_view, &mut phases, LOADED, deadline).await,
                "napper's pipeline reached Loaded: {phases:?}"
            );
            let pid_b = *child_pids().first().expect("napper's worker is live");
            control.emit(&seq_cmd("adcs", SequenceCommandKind::Start)).unwrap();
            assert!(
                wait_phase(&mut slot_view, &mut phases, DONE, deadline).await,
                "napper ran to Done: {phases:?}"
            );
            (phases, pid_a, pid_b)
        });
        coord.run_for(2500).await;
        let (phases, pid_a, pid_b) = driver.await.expect("the driver task completed");
        (coord, phases, pid_a, pid_b)
    });

    // The phase walk shows both full cycles, each entering through the
    // pipeline's Loading (published at least once: the Load cycle itself
    // polls the just-spawned worker Pending before publishing).
    assert!(
        is_subsequence(
            &phases,
            &[LOADING, LOADED, RUNNING, DONE, LOADING, LOADED, RUNNING, DONE]
        ),
        "two Load/run cycles through the pipeline: {phases:?}"
    );
    // One worker per occupant Load: the swap spawned a fresh process.
    assert_ne!(pid_a, pid_b, "napper got its own worker");
    // The ordered event stream, both occupants: each Load announces its
    // pipeline with Loading, progress crossed the mmap SequenceStatus ring,
    // and each terminal folded to Completed. Kinds are rendered to tokens
    // because `SequenceEventKind` carries no `PartialEq`.
    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    let kinds: Vec<String> = events.iter().map(|e| event_token(&e.kind)).collect();
    let expect = |name: &str| {
        [
            format!("loading:{name}"),
            format!("loaded:{name}"),
            "started".to_string(),
            "progress:waiting".to_string(),
            "progress:done".to_string(),
            "completed".to_string(),
        ]
    };
    let expected: Vec<String> = expect("waiter").into_iter().chain(expect("napper")).collect();
    assert_eq!(kinds, expected, "the full ordered event stream");
    // The worker list names the slot's live process: napper's worker holds
    // its roles through Done, and no unplanned death was counted.
    let workers = coord.workers();
    assert_eq!(workers.len(), 1, "{workers:?}");
    assert_eq!(&*workers[0].name, "adcs");
    assert_eq!(workers[0].pid, pid_b, "the telemetered pid is napper's worker");
    assert_eq!(workers[0].restarts, 0, "Loads are commanded, not deaths");
    assert_eq!(workers[0].state, WorkerRunState::Running);
    assert!(coord.stopped().is_empty(), "Done is not a hard-stop");

    // Teardown reaps napper's worker.
    drop(coord);
    drop(events_view);
    assert!(child_pids().is_empty(), "no worker child survives teardown");

    // The isolation canary: every process that ever mapped the occupant
    // artifact logged its pid. The workers (describe and run) appear; this
    // host process must not.
    let canary = std::env::var("SEQ_FIXTURE_CANARY").expect("main set the canary path");
    let logged = std::fs::read_to_string(&canary).expect("the canary file exists");
    let pids: Vec<u32> = logged.lines().filter_map(|l| l.trim().parse().ok()).collect();
    assert!(
        !pids.contains(&std::process::id()),
        "the host never mapped the occupant artifact: {pids:?}"
    );
    assert!(
        pids.contains(&pid_a) && pids.contains(&pid_b),
        "each occupant worker mapped the artifact: {pids:?}"
    );
}

// ---------------------------------------------------------------------------
// Slot test 2: SIGKILL the occupant worker mid-run, recover by Reset
// ---------------------------------------------------------------------------

fn proc_slot_death_is_terminal_and_reset_recovers(seq_lib: &Path) {
    use metor_fsw_2::wiring::{Registry, resolve};
    use metor_fsw_2::{SlotInitState, WiringBuilder};

    let mut wiring = WiringBuilder::new()
        .coordinator_spec(slow_sim_coordinator())
        .artifact("seqs", "metor-fsw-2-seq-fixture", "metor_fsw_2_seq_fixture")
        .slot("adcs")
        .process()
        .allow("waiter")
        .initial("waiter", SlotInitState::Loaded)
        .end()
        .connect_msg("coordinator", "adcs", "SequenceCommand")
        .build();
    wiring.artifacts[0].path = Some(seq_lib.to_path_buf());
    let mut coord = resolve(&wiring, &Registry::new()).expect("the process-slot graph resolves");
    // The initial occupant loads at init (inside the barrier), not at build.
    assert!(child_pids().is_empty(), "a process slot spawns per Load, not at build");

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status registered")
            .expect("reader slot available"),
    );
    let mut events_view = coord
        .registry()
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");
    let mut control = coord.control_handle().expect("taken once per coordinator");

    let (coord, phases, pid, pid2) = stellarator::run(|| async move {
        let driver = stellarator::spawn(async move {
            let mut phases: Vec<u8> = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(20);
            // The initial occupant loaded blocking inside the init barrier.
            assert!(
                wait_phase(&mut slot_view, &mut phases, LOADED, deadline).await,
                "the initial occupant is Loaded: {phases:?}"
            );
            let pid = *child_pids().first().expect("the initial worker is live");
            control.emit(&seq_cmd("adcs", SequenceCommandKind::Start)).unwrap();
            assert!(
                wait_phase(&mut slot_view, &mut phases, RUNNING, deadline).await,
                "the occupant is Running: {phases:?}"
            );
            // `kill` blocks this thread — the single-threaded executor, and
            // so the cycle loop — which lands the SIGKILL deterministically
            // inside the waiter's ~2000-cycle window.
            kill9(pid);
            assert!(
                wait_phase(&mut slot_view, &mut phases, SLOT_STOPPED, deadline).await,
                "the death folded to Stopped: {phases:?}"
            );
            // No auto-restart: the dead worker was reaped and nothing
            // replaced it. Recovery is the operator's Reset, which respawns
            // through the polled pipeline.
            assert!(
                child_pids().is_empty(),
                "a dead occupant worker is reaped, never respawned"
            );
            control.emit(&seq_cmd("adcs", SequenceCommandKind::Reset)).unwrap();
            assert!(
                wait_phase(&mut slot_view, &mut phases, LOADED, deadline).await,
                "Reset brought a fresh worker to Loaded: {phases:?}"
            );
            let pid2 = *child_pids().first().expect("the reset worker is live");
            control.emit(&seq_cmd("adcs", SequenceCommandKind::Start)).unwrap();
            assert!(
                wait_phase(&mut slot_view, &mut phases, DONE, deadline).await,
                "the rerun completed: {phases:?}"
            );
            (phases, pid, pid2)
        });
        coord.run_for(1_000_000).await;
        let (phases, pid, pid2) = driver.await.expect("the driver task completed");
        (coord, phases, pid, pid2)
    });

    assert!(
        is_subsequence(
            &phases,
            &[LOADED, RUNNING, SLOT_STOPPED, LOADING, LOADED, RUNNING, DONE]
        ),
        "death is terminal, Reset re-runs the pipeline: {phases:?}"
    );
    assert_ne!(pid, pid2, "Reset spawned a fresh worker");
    // The event stream names the death and the recovery. Progress lines are
    // not pinned here: the killed occupant's last published records may drain
    // during the rerun's fold.
    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    let kinds: Vec<&SequenceEventKind> = events.iter().map(|e| &e.kind).collect();
    let failed_at = kinds
        .iter()
        .position(|k| matches!(k, SequenceEventKind::Failed { reason } if reason.contains("worker")))
        .unwrap_or_else(|| panic!("a Failed event names the worker death: {kinds:?}"));
    let loads: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter_map(|(i, k)| matches!(k, SequenceEventKind::Loaded { .. }).then_some(i))
        .collect();
    assert_eq!(loads.len(), 2, "the initial load and the Reset reload: {kinds:?}");
    assert!(
        loads[0] < failed_at && failed_at < loads[1],
        "Failed sits between the two loads: {kinds:?}"
    );
    assert!(
        matches!(kinds.last(), Some(SequenceEventKind::Completed)),
        "the rerun completed: {kinds:?}"
    );
    // Telemetry counted the one unplanned death, and the recovered worker is
    // the live one.
    let workers = coord.workers();
    assert_eq!(workers.len(), 1, "{workers:?}");
    assert_eq!(workers[0].restarts, 1, "one unplanned death counted: {workers:?}");
    assert_eq!(workers[0].pid, pid2);
    assert_eq!(workers[0].state, WorkerRunState::Running);
    // The stopped list reflects current states: the recovered slot left it.
    assert!(coord.stopped().is_empty(), "{:?}", coord.stopped());

    drop(coord);
    drop(events_view);
    assert!(child_pids().is_empty(), "all workers reaped");
}

// ---------------------------------------------------------------------------
// Slot test 3: Abort crosses the process boundary
// ---------------------------------------------------------------------------

fn proc_slot_abort_crosses_the_boundary(seq_lib: &Path) {
    use metor_fsw_2::wiring::{Registry, resolve};
    use metor_fsw_2::{SlotInitState, WiringBuilder};

    let mut wiring = WiringBuilder::new()
        .coordinator_spec(slow_sim_coordinator())
        .artifact("seqs", "metor-fsw-2-seq-fixture", "metor_fsw_2_seq_fixture")
        .slot("adcs")
        .process()
        .allow("waiter")
        .initial("waiter", SlotInitState::Running)
        .end()
        .connect_msg("coordinator", "adcs", "SequenceCommand")
        .build();
    wiring.artifacts[0].path = Some(seq_lib.to_path_buf());
    let mut coord = resolve(&wiring, &Registry::new()).expect("the process-slot graph resolves");

    let mut slot_view: Input<SlotStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.slot_status"))
            .expect("slot status registered")
            .expect("reader slot available"),
    );
    let mut seq_view: Input<SequenceStatus> = Input::new(
        coord
            .registry()
            .view(ComponentId::new("adcs.sequence"))
            .expect("occupant SequenceStatus is registered")
            .expect("reader slot available"),
    );
    let mut events_view = coord
        .registry()
        .view(ComponentId::new("adcs.sequences"))
        .expect("the slot events channel is registered")
        .expect("reader slot available");
    let mut control = coord.control_handle().expect("taken once per coordinator");

    let (coord, phases) = stellarator::run(|| async move {
        let driver = stellarator::spawn(async move {
            let mut phases: Vec<u8> = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(20);
            assert!(
                wait_phase(&mut slot_view, &mut phases, RUNNING, deadline).await,
                "the initial=running occupant is Running: {phases:?}"
            );
            // The cancel frame is written by the host-side control writer and
            // crosses on the mmap ring; the occupant folds it at its next
            // wait, deep inside its ~2000-cycle window.
            control.emit(&seq_cmd("adcs", SequenceCommandKind::Abort)).unwrap();
            assert!(
                wait_phase(&mut slot_view, &mut phases, DONE, deadline).await,
                "the abort went terminal: {phases:?}"
            );
            phases
        });
        coord.run_for(500_000).await;
        let phases = driver.await.expect("the driver task completed");
        (coord, phases)
    });

    assert!(
        is_subsequence(&phases, &[RUNNING, DONE]),
        "Aborted is still terminal Done: {phases:?}"
    );
    // The occupant's own terminal outcome crossed back over the mmap
    // SequenceStatus ring: the safing branch ran in the worker.
    let mut states = Vec::new();
    seq_view
        .drain(|f| states.push(f.get().run_state))
        .expect("no lap");
    assert_eq!(
        states.last(),
        Some(&ABORTED),
        "the occupant cooperatively aborted: {states:?}"
    );
    let events = drain_msgs::<SequenceChannelEvent>(&mut events_view);
    let kinds: Vec<&SequenceEventKind> = events.iter().map(|e| &e.kind).collect();
    assert!(
        matches!(kinds.last(), Some(SequenceEventKind::Aborted)),
        "the terminal fold is Aborted: {kinds:?}"
    );
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, SequenceEventKind::Progress { detail } if detail == "aborted")),
        "the safing branch's progress line arrived: {kinds:?}"
    );

    drop(coord);
    drop(events_view);
    assert!(child_pids().is_empty(), "all workers reaped");
}
