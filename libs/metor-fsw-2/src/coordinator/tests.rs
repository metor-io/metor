//! Coordinator acceptance tests (coordinator.md "Tests"): the two-cyclic-system graph end
//! to end, the lapped hard-stop, an async system wired through a private copy-in
//! buffer, build-time wiring validation, and the init barrier. Systems are
//! registered and wired through the `Coordinator` builder — no hand-built ports.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use metor_fsw_ring::{BoxBacking, Notifier};
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    AllOutputs, AsyncSystem, ClockMode, Coordinator, CoordinatorConfig, CyclicSystem, Input, MsgIn,
    MsgOut, Out, Output, PortRef, StopReason, System, SystemInput, SystemOutput, WireError,
};

// ---------------------------------------------------------------------------
// Frames under test.
// ---------------------------------------------------------------------------

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "nav")]
struct Nav {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    angle: f64,
}

// Empty bundles for systems with no user inputs / outputs.
#[derive(SystemInput)]
struct NoIn {}

#[derive(SystemOutput)]
struct NoOut {}

// ---------------------------------------------------------------------------
// A cyclic producer: writes an incrementing `Imu` every cycle. No inputs.
// ---------------------------------------------------------------------------

struct Producer {
    n: f64,
    /// Optional burst count (writes N records per execute) for the drop-on-full test.
    burst: u64,
    init_counter: Option<Arc<AtomicUsize>>,
}

impl Producer {
    fn new() -> Self {
        Self {
            n: 0.0,
            burst: 1,
            init_counter: None,
        }
    }
}

#[derive(SystemOutput)]
struct ProdOut {
    imu: Output<Imu>,
}

impl System for Producer {
    type Input = NoIn;
    type Output = Out<ProdOut>;
    const NAME: &'static str = "producer";
    fn init(&mut self, _o: &mut Self::Output) {
        if let Some(c) = &self.init_counter {
            c.fetch_add(1, Relaxed);
        }
    }
}

impl CyclicSystem for Producer {
    fn execute(&mut self, now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        for _ in 0..self.burst {
            self.n += 1.0;
            let _ = o.imu.write(&Imu {
                timestamp: now,
                omega: self.n,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// A cyclic consumer: records every `Imu` omega it samples. Empty output.
// ---------------------------------------------------------------------------

struct Consumer {
    seen: Rc<RefCell<Vec<f64>>>,
    /// When false the consumer never drains its input (forces a lap).
    drain: bool,
    init_counter: Option<Arc<AtomicUsize>>,
    first_exec_init: Option<Arc<AtomicUsize>>,
}

#[derive(SystemInput)]
struct ConsIn {
    imu: Input<Imu>,
}

impl System for Consumer {
    type Input = ConsIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "consumer";
    fn init(&mut self, _o: &mut Self::Output) {
        if let Some(c) = &self.init_counter {
            c.fetch_add(1, Relaxed);
        }
    }
}

impl CyclicSystem for Consumer {
    fn execute(&mut self, _now: Timestamp, input: &mut ConsIn, _o: &mut Self::Output) {
        if let Some(c) = &self.first_exec_init {
            // Record (once) how many inits had run before the first execute.
            if let Some(ic) = &self.init_counter {
                let _ = c.compare_exchange(0, ic.load(Relaxed), Relaxed, Relaxed);
            }
        }
        if !self.drain {
            return; // ignore the input so it eventually laps
        }
        if let Some(imu) = input.imu.latest() {
            self.seen.borrow_mut().push(imu.get().omega);
        }
    }
}

fn config() -> CoordinatorConfig {
    CoordinatorConfig {
        cycle_rate: 1000.0,
        default_depth: crate::DEFAULT_DEPTH,
        clock: ClockMode::Wall,
    }
}

// ---------------------------------------------------------------------------
// Two-cyclic-system graph, end to end (the headline smoke test).
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn two_system_end_to_end() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = Coordinator::builder(config());
    let prod = b.add_cyclic(Producer::new());
    let cons = b.add_cyclic(Consumer {
        seen: seen.clone(),
        drain: true,
        init_counter: None,
        first_exec_init: None,
    });
    b.connect(PortRef::new::<Imu>(prod), PortRef::new::<Imu>(cons))
        .unwrap();
    let mut coord = b.build().unwrap();

    coord.run_for(5).await;

    // The producer runs before the consumer each cycle (registration order), so
    // the consumer samples this cycle's fresh value: 1.0, 2.0, .. 5.0.
    assert_eq!(*seen.borrow(), vec![1.0, 2.0, 3.0, 4.0, 5.0]);
}

// ---------------------------------------------------------------------------
// Lapped input → permanent hard-stop, surfaced in the coordinator status frame.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn lapped_input_hard_stops() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = Coordinator::builder(config());
    let prod = b.add_cyclic(Producer::new());
    let cons = b.add_cyclic(Consumer {
        seen: seen.clone(),
        drain: false, // never reads → its view laps as the producer writes
        init_counter: None,
        first_exec_init: None,
    });
    b.connect(PortRef::new::<Imu>(prod), PortRef::new::<Imu>(cons))
        .unwrap();
    let mut coord = b.build().unwrap();

    coord.run_for(60).await;

    // The consumer hard-stopped and is named in the status surface; the producer
    // (no inputs) keeps running.
    let stopped = coord.stopped();
    assert_eq!(stopped.len(), 1, "exactly the consumer stopped");
    assert_eq!(stopped[0].name, "consumer");
    assert_eq!(stopped[0].reason, StopReason::LappedInput);

    // And it appears in the coordinator status *frame*.
    let frame = coord.read_status().expect("status frame published");
    assert!(
        frame.iter().any(|(name, code)| name == "consumer" && *code == 1),
        "consumer named in status frame: {frame:?}"
    );
}

// ---------------------------------------------------------------------------
// Async system wired through a private copy-in buffer.
// ---------------------------------------------------------------------------

struct AsyncConsumer {
    count: Arc<AtomicU64>,
    last: Arc<AtomicU64>,
}

#[derive(SystemInput)]
struct AsyncIn {
    imu: Input<Imu, BoxBacking, Notifier, Notifier>,
}

#[derive(SystemOutput)]
struct AsyncNoOut {}

impl System for AsyncConsumer {
    type Input = AsyncIn;
    type Output = Out<AsyncNoOut, BoxBacking, Notifier, Notifier>;
    const NAME: &'static str = "async_consumer";
}

impl AsyncSystem for AsyncConsumer {
    async fn run(&mut self, input: &mut Self::Input, _o: &mut Self::Output) {
        match input.imu.recv().await {
            Ok(imu) => {
                self.count.fetch_add(1, Relaxed);
                self.last.store(imu.get().omega as u64, Relaxed);
            }
            Err(_) => input.imu.resync(), // lapped: drop-on-full, keep going
        }
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn async_through_copy_in() {
    let count = Arc::new(AtomicU64::new(0));
    let last = Arc::new(AtomicU64::new(0));
    let mut producer = Producer::new();
    producer.burst = 8; // burst past the private buffer to exercise drop-on-full

    let mut b = Coordinator::builder(config());
    let prod = b.add_cyclic(producer);
    let asy = b.add_async(AsyncConsumer {
        count: count.clone(),
        last: last.clone(),
    });
    b.connect(PortRef::new::<Imu>(prod), PortRef::new::<Imu>(asy))
        .unwrap();
    let mut coord = b.build().unwrap();

    // Completes without blocking despite overflow (overwrite private buffer).
    coord.run_for(30).await;

    assert!(
        count.load(Relaxed) >= 1,
        "async system received via recv through the copy-in"
    );
    assert!(last.load(Relaxed) > 0, "received a real sample");
}

// ---------------------------------------------------------------------------
// Build-time validation.
// ---------------------------------------------------------------------------

#[test]
fn validation_incompatible_frame_id_mismatch() {
    // connect rejects a producer/consumer that do not even share a frame id.
    let mut b = Coordinator::builder(config());
    let prod = b.add_cyclic(Producer::new());
    let cons = b.add_cyclic(Consumer {
        seen: Rc::new(RefCell::new(Vec::new())),
        drain: true,
        init_counter: None,
        first_exec_init: None,
    });
    let err = b
        .connect(PortRef::new::<Nav>(prod), PortRef::new::<Imu>(cons))
        .unwrap_err();
    assert!(matches!(err, WireError::FrameIdMismatch { .. }));
}

#[test]
fn validation_unknown_port() {
    // The producer has no `Nav` output, so the edge fails at build.
    let mut b = Coordinator::builder(config());
    let prod = b.add_cyclic(Producer::new());
    let cons = b.add_cyclic(Consumer {
        seen: Rc::new(RefCell::new(Vec::new())),
        drain: true,
        init_counter: None,
        first_exec_init: None,
    });
    // Same frame id on both sides (passes connect), but the producer lacks a Nav
    // output port → UnknownPort at build.
    b.connect(PortRef::new::<Nav>(prod), PortRef::new::<Nav>(cons))
        .unwrap();
    assert!(matches!(b.build(), Err(WireError::UnknownPort { .. })));
}

#[test]
fn validation_unconnected_input() {
    let mut b = Coordinator::builder(config());
    let _prod = b.add_cyclic(Producer::new());
    let _cons = b.add_cyclic(Consumer {
        seen: Rc::new(RefCell::new(Vec::new())),
        drain: true,
        init_counter: None,
        first_exec_init: None,
    });
    // No connect at all → the consumer's `imu` input is unconnected.
    assert!(matches!(b.build(), Err(WireError::UnconnectedInput { .. })));
}

#[test]
fn validation_invalid_cycle_rate_wall_clock() {
    // A 0/negative/NaN/infinite cycle_rate would panic in `Duration::from_secs_f64`
    // at run time under a Wall clock; build() rejects it up front instead.
    for rate in [0.0, -5.0, f64::NAN, f64::INFINITY] {
        let mut b = Coordinator::builder(CoordinatorConfig {
            cycle_rate: rate,
            default_depth: crate::DEFAULT_DEPTH,
            clock: ClockMode::Wall,
        });
        b.add_cyclic(Producer::new());
        assert!(
            matches!(b.build(), Err(WireError::InvalidCycleRate { .. })),
            "rate {rate} rejected at build"
        );
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn simulated_clock_ignores_cycle_rate() {
    // `cycle_rate` is documented ignored under `Simulated`, so an unusable rate must
    // neither fail build nor panic mid-run (the budget is only computed under Wall).
    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 0.0,
        default_depth: crate::DEFAULT_DEPTH,
        clock: ClockMode::Simulated {
            dt: Duration::from_micros(100),
        },
    });
    b.add_cyclic(Producer::new());
    let mut coord = b.build().expect("cycle_rate is not validated under Simulated");
    coord.run_for(3).await; // panicked in Duration::from_secs_f64 before the fix
}

#[test]
fn validation_double_connect() {
    let mut b = Coordinator::builder(config());
    let prod1 = b.add_cyclic(Producer::new());
    let prod2 = b.add_cyclic(Producer::new());
    let cons = b.add_cyclic(Consumer {
        seen: Rc::new(RefCell::new(Vec::new())),
        drain: true,
        init_counter: None,
        first_exec_init: None,
    });
    b.connect(PortRef::new::<Imu>(prod1), PortRef::new::<Imu>(cons))
        .unwrap();
    b.connect(PortRef::new::<Imu>(prod2), PortRef::new::<Imu>(cons))
        .unwrap();
    assert!(matches!(b.build(), Err(WireError::DoubleConnect { .. })));
}

// ---------------------------------------------------------------------------
// Init barrier: every system's `init` completes before the first `execute`/`run`.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn init_barrier_holds() {
    let init_counter = Arc::new(AtomicUsize::new(0));
    let first_exec_init = Arc::new(AtomicUsize::new(0));

    let mut b = Coordinator::builder(config());
    let prod = b.add_cyclic(Producer {
        n: 0.0,
        burst: 1,
        init_counter: Some(init_counter.clone()),
    });
    let cons = b.add_cyclic(Consumer {
        seen: Rc::new(RefCell::new(Vec::new())),
        drain: true,
        init_counter: Some(init_counter.clone()),
        first_exec_init: Some(first_exec_init.clone()),
    });
    b.connect(PortRef::new::<Imu>(prod), PortRef::new::<Imu>(cons))
        .unwrap();
    let mut coord = b.build().unwrap();

    coord.run_for(3).await;

    // Both inits ran before the first execute observed the counter.
    assert_eq!(init_counter.load(Relaxed), 2, "both systems inited");
    assert_eq!(
        first_exec_init.load(Relaxed),
        2,
        "all inits completed before the first execute"
    );
}

// ---------------------------------------------------------------------------
// Feedback loops: an unbroken cycle is rejected; `connect_delayed` breaks it.
// ---------------------------------------------------------------------------

// `Looper` produces `Imu`, consumes `Nav`; `Backer` produces `Nav`, consumes
// `Imu`. Plain-connecting both directions closes a 2-system cycle.
struct Looper {
    n: f64,
}

#[derive(SystemInput)]
struct LooperIn {
    nav: Input<Nav>,
}

#[derive(SystemOutput)]
struct LooperOut {
    imu: Output<Imu>,
}

impl System for Looper {
    type Input = LooperIn;
    type Output = Out<LooperOut>;
    const NAME: &'static str = "looper";
}

impl CyclicSystem for Looper {
    fn execute(&mut self, now: Timestamp, input: &mut LooperIn, o: &mut Self::Output) {
        let _ = input.nav.latest(); // sample the (one-cycle-late) feedback
        self.n += 1.0;
        let _ = o.imu.write(&Imu {
            timestamp: now,
            omega: self.n,
        });
    }
}

struct Backer;

#[derive(SystemInput)]
struct BackerIn {
    imu: Input<Imu>,
}

#[derive(SystemOutput)]
struct BackerOut {
    nav: Output<Nav>,
}

impl System for Backer {
    type Input = BackerIn;
    type Output = Out<BackerOut>;
    const NAME: &'static str = "backer";
}

impl CyclicSystem for Backer {
    fn execute(&mut self, now: Timestamp, input: &mut BackerIn, o: &mut Self::Output) {
        let angle = match input.imu.latest() {
            Some(imu) => imu.get().omega,
            None => 0.0,
        };
        let _ = o.nav.write(&Nav {
            timestamp: now,
            angle,
        });
    }
}

#[test]
fn feedback_cycle_unbroken_is_rejected() {
    let mut b = Coordinator::builder(config());
    let looper = b.add_cyclic(Looper { n: 0.0 });
    let backer = b.add_cyclic(Backer);
    // Both directions are plain forward edges → an unbroken cycle.
    b.connect(PortRef::new::<Imu>(looper), PortRef::new::<Imu>(backer))
        .unwrap();
    b.connect(PortRef::new::<Nav>(backer), PortRef::new::<Nav>(looper))
        .unwrap();
    let err = b.build().err().expect("the cycle is rejected at build");
    assert!(
        matches!(&err, WireError::FeedbackCycle { systems } if systems.contains(&"looper") && systems.contains(&"backer")),
        "{err:?}"
    );
}

#[cfg(not(miri))]
#[stellarator::test]
async fn delayed_edge_allows_feedback_loop() {
    let mut b = Coordinator::builder(config());
    let looper = b.add_cyclic(Looper { n: 0.0 });
    let backer = b.add_cyclic(Backer);
    // The forward Imu edge; the Nav back-edge is the explicit one-cycle delay.
    b.connect(PortRef::new::<Imu>(looper), PortRef::new::<Imu>(backer))
        .unwrap();
    b.connect_delayed(PortRef::new::<Nav>(backer), PortRef::new::<Nav>(looper))
        .unwrap();
    let mut coord = b.build().expect("connect_delayed breaks the cycle so it builds");

    coord.run_for(5).await;

    // Both systems ran the whole way without lapping/hard-stopping.
    assert!(coord.stopped().is_empty(), "no system hard-stopped");
}

// ---------------------------------------------------------------------------
// Self-loop frame edges: a system plainly connected to itself is the tightest
// feedback loop, so it needs `connect_delayed` like any other loop.
// ---------------------------------------------------------------------------

// `SelfLoop` consumes and produces `Imu` — its input can only ever be its own
// previous-cycle output.
struct SelfLoop {
    n: f64,
}

#[derive(SystemInput)]
struct SelfLoopIn {
    imu: Input<Imu>,
}

#[derive(SystemOutput)]
struct SelfLoopOut {
    imu: Output<Imu>,
}

impl System for SelfLoop {
    type Input = SelfLoopIn;
    type Output = Out<SelfLoopOut>;
    const NAME: &'static str = "self_loop";
}

impl CyclicSystem for SelfLoop {
    fn execute(&mut self, now: Timestamp, input: &mut SelfLoopIn, o: &mut Self::Output) {
        let _ = input.imu.latest(); // sample last cycle's own output
        self.n += 1.0;
        let _ = o.imu.write(&Imu {
            timestamp: now,
            omega: self.n,
        });
    }
}

#[test]
fn self_loop_plain_connect_is_rejected() {
    let mut b = Coordinator::builder(config());
    let s = b.add_cyclic(SelfLoop { n: 0.0 });
    // A plain frame edge from a system to itself: previously exempt from cycle
    // detection, now a one-member `FeedbackCycle`.
    b.connect(PortRef::new::<Imu>(s), PortRef::new::<Imu>(s))
        .unwrap();
    let err = b.build().err().expect("a plain self-edge is rejected at build");
    assert!(
        matches!(&err, WireError::FeedbackCycle { systems } if systems == &vec!["self_loop"]),
        "{err:?}"
    );
}

#[cfg(not(miri))]
#[stellarator::test]
async fn self_loop_delayed_connect_builds_and_runs() {
    let mut b = Coordinator::builder(config());
    let s = b.add_cyclic(SelfLoop { n: 0.0 });
    b.connect_delayed(PortRef::new::<Imu>(s), PortRef::new::<Imu>(s))
        .unwrap();
    let mut coord = b.build().expect("connect_delayed declares the self-feedback");

    coord.run_for(5).await;

    assert!(coord.stopped().is_empty(), "no system hard-stopped");
}

// ---------------------------------------------------------------------------
// Registration order vs dataflow (StaleFrameEdge): a non-delayed frame edge
// between cyclic systems must point forward in registration order, or the
// consumer would permanently read last cycle's value.
// ---------------------------------------------------------------------------

#[test]
fn backward_frame_edge_is_rejected() {
    let mut b = Coordinator::builder(config());
    // The consumer registers (and therefore steps) BEFORE its producer.
    let cons = b.add_cyclic(Consumer {
        seen: Rc::new(RefCell::new(Vec::new())),
        drain: true,
        init_counter: None,
        first_exec_init: None,
    });
    let prod = b.add_cyclic(Producer::new());
    b.connect(PortRef::new::<Imu>(prod), PortRef::new::<Imu>(cons))
        .unwrap();
    let err = b.build().err().expect("a backward frame edge is rejected");
    assert!(
        matches!(
            err,
            WireError::StaleFrameEdge {
                producer: "producer",
                consumer: "consumer",
                ..
            }
        ),
        "{err:?}"
    );
}

#[cfg(not(miri))]
#[stellarator::test]
async fn backward_frame_edge_allowed_when_delayed() {
    // The same backward edge declared with `connect_delayed` builds: the one-cycle
    // staleness is now explicit, and the consumer samples last cycle's value.
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = Coordinator::builder(config());
    let cons = b.add_cyclic(Consumer {
        seen: seen.clone(),
        drain: true,
        init_counter: None,
        first_exec_init: None,
    });
    let prod = b.add_cyclic(Producer::new());
    b.connect_delayed(PortRef::new::<Imu>(prod), PortRef::new::<Imu>(cons))
        .unwrap();
    let mut coord = b.build().expect("connect_delayed declares the staleness");

    coord.run_for(5).await;

    // The consumer steps first each cycle: nothing on cycle 1, then last cycle's value.
    assert_eq!(*seen.borrow(), vec![1.0, 2.0, 3.0, 4.0]);
}

#[cfg(not(miri))]
#[stellarator::test]
async fn backward_edge_to_async_consumer_is_allowed() {
    // Registration order carries no execution-order semantics for an async consumer
    // (it reads through the post-step copy-in), so registering it before its producer
    // is fine — the StaleFrameEdge check only covers cyclic-to-cyclic edges.
    let count = Arc::new(AtomicU64::new(0));
    let last = Arc::new(AtomicU64::new(0));
    let mut b = Coordinator::builder(config());
    let asy = b.add_async(AsyncConsumer {
        count: count.clone(),
        last: last.clone(),
    });
    let prod = b.add_cyclic(Producer::new());
    b.connect(PortRef::new::<Imu>(prod), PortRef::new::<Imu>(asy))
        .unwrap();
    let mut coord = b
        .build()
        .expect("async endpoints are exempt from the registration-order check");

    coord.run_for(30).await;

    assert!(count.load(Relaxed) >= 1, "the async consumer received data");
}

// ---------------------------------------------------------------------------
// One coordinator drives exactly one run: a second `run_for` panics instead of
// silently re-initing every system over consumed async plumbing.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[test]
#[should_panic(expected = "run_for called twice")]
fn run_for_twice_panics() {
    stellarator::run(|| async {
        let mut b = Coordinator::builder(config());
        b.add_cyclic(Producer::new());
        let mut coord = b.build().unwrap();
        coord.run_for(1).await;
        coord.run_for(1).await; // the first run consumed the coordinator
    });
}

// ---------------------------------------------------------------------------
// Stopped-set change detection compares membership, not just length.
// ---------------------------------------------------------------------------

#[test]
fn stopped_set_change_detection_compares_membership() {
    use super::{StoppedSystem, stopped_set_changed};
    let a = StoppedSystem {
        name: "a",
        reason: StopReason::LappedInput,
    };
    let b = StoppedSystem {
        name: "b",
        reason: StopReason::LappedInput,
    };
    let a_panicked = StoppedSystem {
        name: "a",
        reason: StopReason::Panicked,
    };
    // Equal length, different member (a slot recovered the same cycle another
    // stopped) — the length-only check missed exactly this.
    assert!(stopped_set_changed(&[a], &[b]));
    // Equal length, same system, new reason.
    assert!(stopped_set_changed(&[a], &[a_panicked]));
    // Identical sets are unchanged; length changes still register.
    assert!(!stopped_set_changed(&[a, b], &[a, b]));
    assert!(stopped_set_changed(&[a], &[]));
    assert!(!stopped_set_changed(&[], &[]));
}

// ---------------------------------------------------------------------------
// Simulated clock: deterministic, monotonic per-cycle timestamps (cycle k at
// start + k*dt), with no wall-clock pacing.
// ---------------------------------------------------------------------------

struct StampRec {
    stamps: Rc<RefCell<Vec<Timestamp>>>,
}

impl System for StampRec {
    type Input = NoIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "stamp_rec";
}

impl CyclicSystem for StampRec {
    fn execute(&mut self, now: Timestamp, _in: &mut NoIn, _o: &mut Self::Output) {
        self.stamps.borrow_mut().push(now);
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn simulated_clock_is_deterministic_and_monotonic() {
    let dt = Duration::from_micros(8333); // ~1/120 s, whole microseconds
    let stamps = Rc::new(RefCell::new(Vec::new()));
    let mut b = Coordinator::builder(CoordinatorConfig {
        cycle_rate: 1000.0,
        default_depth: crate::DEFAULT_DEPTH,
        clock: ClockMode::Simulated { dt },
    });
    b.add_cyclic(StampRec {
        stamps: stamps.clone(),
    });
    let mut coord = b.build().unwrap();

    coord.run_for(10).await;

    let s = stamps.borrow();
    assert_eq!(s.len(), 10, "one stamp per cycle");
    let step = dt.as_micros() as i64;
    for k in 1..s.len() {
        // Cycle k sits exactly k*dt past cycle 0, and the clock is strictly rising.
        assert_eq!(s[k].0 - s[0].0, k as i64 * step, "cycle {k} at start + k*dt");
        assert!(s[k].0 > s[k - 1].0, "monotonically increasing");
    }
}

#[test]
fn simulated_clock_does_not_wrap_past_u32_cycles() {
    // The old arithmetic (`epoch + dt * k as u32`) truncated the cycle index, jumping
    // the timeline back to `epoch` every 2^32 cycles. The wide-integer helper stays
    // exact and strictly monotonic across that boundary.
    use super::simulated_now;
    let epoch = Timestamp(1_000);
    let dt = Duration::from_micros(8_333);
    let k = 1u64 << 32;
    let before = simulated_now(epoch, dt, k - 1);
    let at = simulated_now(epoch, dt, k);
    assert!(
        at.0 > before.0,
        "monotonic across 2^32 cycles: {} then {}",
        before.0,
        at.0
    );
    assert_eq!(at.0 - epoch.0, 8_333 * (1i64 << 32), "exactly k*dt past epoch");
}

// ---------------------------------------------------------------------------
// Message edges (docs/message-wiring.md §3): typed `MsgOut<M>` -> `MsgIn<M>`,
// fan-in, and the wiring-validation behaviour (id-mismatch, optional inputs).
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema)]
struct TestEvent {
    seq: u64,
}

#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema)]
struct OtherEvent {
    x: u32,
}

// A wired message port needs the explicit, stable name token (A10).
impl crate::NamedMsg for TestEvent {
    const NAME: &'static str = "TestEvent";
}
impl crate::NamedMsg for OtherEvent {
    const NAME: &'static str = "OtherEvent";
}

// A cyclic producer emitting one `TestEvent` per cycle. No frame inputs.
struct MsgProducer {
    n: u64,
}

#[derive(SystemOutput)]
struct MsgProdOut {
    events: MsgOut<TestEvent>,
}

impl System for MsgProducer {
    type Input = NoIn;
    type Output = Out<MsgProdOut>;
    const NAME: &'static str = "msg_producer";
}

impl CyclicSystem for MsgProducer {
    fn execute(&mut self, _now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        self.n += 1;
        let _ = o.events.emit(&TestEvent { seq: self.n });
    }
}

// A cyclic consumer draining every `TestEvent` it sees.
struct MsgConsumer {
    seen: Rc<RefCell<Vec<u64>>>,
}

#[derive(SystemInput)]
struct MsgConsIn {
    events: MsgIn<TestEvent>,
}

impl System for MsgConsumer {
    type Input = MsgConsIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "msg_consumer";
}

impl CyclicSystem for MsgConsumer {
    fn execute(&mut self, _now: Timestamp, input: &mut MsgConsIn, _o: &mut Self::Output) {
        input.events.drain(|e| self.seen.borrow_mut().push(e.seq));
    }
}

// A consumer of a *different* Msg type, for the id-mismatch check.
struct OtherConsumer;

#[derive(SystemInput)]
struct OtherConsIn {
    events: MsgIn<OtherEvent>,
}

impl System for OtherConsumer {
    type Input = OtherConsIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "other_consumer";
}

impl CyclicSystem for OtherConsumer {
    fn execute(&mut self, _now: Timestamp, input: &mut OtherConsIn, _o: &mut Self::Output) {
        input.events.drain(|_| {});
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn msg_edge_two_cyclic_systems() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = Coordinator::builder(config());
    let prod = b.add_cyclic(MsgProducer { n: 0 });
    let cons = b.add_cyclic(MsgConsumer { seen: seen.clone() });
    b.connect_msg(
        PortRef::msg::<TestEvent>(prod),
        PortRef::msg::<TestEvent>(cons),
    )
    .unwrap();
    let mut coord = b.build().unwrap();

    coord.run_for(4).await;

    // Producer emits 1..=4; the consumer drains each the same cycle (producer runs first).
    assert_eq!(*seen.borrow(), vec![1, 2, 3, 4]);
}

#[cfg(not(miri))]
#[stellarator::test]
async fn msg_fanin_two_emitters_one_consumer() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = Coordinator::builder(config());
    let prod_a = b.add_cyclic_named("emitter_a", MsgProducer { n: 0 });
    let prod_b = b.add_cyclic_named("emitter_b", MsgProducer { n: 100 });
    let cons = b.add_cyclic(MsgConsumer { seen: seen.clone() });
    // Two producers fan in to one message input — no `DoubleConnect`.
    b.connect_msg(
        PortRef::msg::<TestEvent>(prod_a),
        PortRef::msg::<TestEvent>(cons),
    )
    .unwrap();
    b.connect_msg(
        PortRef::msg::<TestEvent>(prod_b),
        PortRef::msg::<TestEvent>(cons),
    )
    .unwrap();
    let mut coord = b.build().unwrap();

    coord.run_for(2).await;

    // Both producers' records arrive (emitter_a: 1,2; emitter_b: 101,102).
    let mut got = seen.borrow().clone();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2, 101, 102]);
}

#[test]
fn msg_exact_duplicate_edge_is_rejected() {
    // Fan-in of DISTINCT producers is legal (covered above); the exact same edge
    // twice would deliver every record twice, so build() rejects the duplicate.
    let mut b = Coordinator::builder(config());
    let prod = b.add_cyclic(MsgProducer { n: 0 });
    let cons = b.add_cyclic(MsgConsumer {
        seen: Rc::new(RefCell::new(Vec::new())),
    });
    b.connect_msg(
        PortRef::msg::<TestEvent>(prod),
        PortRef::msg::<TestEvent>(cons),
    )
    .unwrap();
    // The copy-pasted duplicate: same producer, same consumer, same port.
    b.connect_msg(
        PortRef::msg::<TestEvent>(prod),
        PortRef::msg::<TestEvent>(cons),
    )
    .unwrap();
    let err = b.build().err().expect("the duplicate message edge is rejected");
    assert!(
        matches!(
            err,
            WireError::DuplicateMsgEdge {
                producer: "msg_producer",
                consumer: "msg_consumer",
                ..
            }
        ),
        "{err:?}"
    );
}

#[test]
fn msg_edge_mismatched_type_is_rejected() {
    let mut b = Coordinator::builder(config());
    let prod = b.add_cyclic(MsgProducer { n: 0 });
    let cons = b.add_cyclic(OtherConsumer);
    // `MsgOut<TestEvent>` -> `MsgIn<OtherEvent>`: distinct `M::ID`, so the two PortRefs name
    // different ports — rejected at `connect_msg` (the same guard a frame-id mismatch hits).
    let err = b
        .connect_msg(
            PortRef::msg::<TestEvent>(prod),
            PortRef::msg::<OtherEvent>(cons),
        )
        .unwrap_err();
    assert!(matches!(err, WireError::FrameIdMismatch { .. }));
}

#[cfg(not(miri))]
#[stellarator::test]
async fn msg_input_may_be_unconnected() {
    // A message input with zero producers builds fine and drains nothing (§3.2) — unlike a
    // frame input, which would be an `UnconnectedInput` build error.
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = Coordinator::builder(config());
    let _cons = b.add_cyclic(MsgConsumer { seen: seen.clone() });
    let mut coord = b.build().unwrap();
    coord.run_for(3).await;
    assert!(seen.borrow().is_empty());
}

// ---------------------------------------------------------------------------
// AllOutputs receive-all tap (docs/message-wiring.md §4): a non-telemetry system
// declaring `AllOutputs` sees every frame output and telemetered message channel,
// and self-derives its reader-slot budget on every buffer.
// ---------------------------------------------------------------------------

struct AllTap {
    frame_outs: Rc<std::cell::Cell<usize>>,
    msg_chans: Rc<std::cell::Cell<usize>>,
}

#[derive(SystemOutput)]
struct AllTapOut {
    all: AllOutputs,
}

impl System for AllTap {
    type Input = NoIn;
    type Output = Out<AllTapOut>;
    const NAME: &'static str = "all_tap";
    fn init(&mut self, o: &mut Self::Output) {
        // The registries are frozen by build time, so init observes the whole graph.
        self.frame_outs.set(o.all.outputs.entries().len());
        self.msg_chans.set(o.all.messages.entries().len());
    }
}

impl CyclicSystem for AllTap {
    fn execute(&mut self, _now: Timestamp, _in: &mut NoIn, _o: &mut Self::Output) {}
}

#[cfg(not(miri))]
#[stellarator::test]
async fn all_outputs_taps_the_whole_graph() {
    let frame_outs = Rc::new(std::cell::Cell::new(0));
    let msg_chans = Rc::new(std::cell::Cell::new(0));
    let mut b = Coordinator::builder(config());
    // A producer of a (telemetered) message channel + its implicit health/log frame outputs.
    let _prod = b.add_cyclic(MsgProducer { n: 0 });
    let _tap = b.add_cyclic(AllTap {
        frame_outs: frame_outs.clone(),
        msg_chans: msg_chans.clone(),
    });
    let mut coord = b.build().unwrap();
    coord.run_for(1).await;

    // The tap — with no wired edges — sees the producer's message channel and the frame
    // outputs (health/log of both systems + the coordinator's own), proving the broad tap
    // and that the `ReceiveAll` port reserved itself a reader slot everywhere (build succeeded).
    assert!(msg_chans.get() >= 1, "sees the message channel: {}", msg_chans.get());
    assert!(frame_outs.get() >= 2, "sees frame outputs: {}", frame_outs.get());
}
