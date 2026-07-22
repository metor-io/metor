//! Coordinator acceptance tests: a two-cyclic-system pipeline end to end,
//! backpressure from an idle consumer, an async system fed through its private
//! copy-in buffer, build-time wiring validation, and the init barrier. Every
//! graph here is registered and wired through the [`Coordinator`] builder.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use metor_fsw_ring::Notifier;
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    AllOutputs, AsyncSystem, ClockMode, CoordinatorConfig, CyclicSystem, Input, MsgIn, MsgOut, Out,
    Output, StopReason, System, SystemInput, SystemOutput, WireError,
};

use super::PortRef;
use super::init::{async_node, cyclic_node};
use super::slot::plan_slot;
use crate::descriptor::PortId;
use crate::frame::Frame as _;

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

// Empty bundles for systems with no user inputs or outputs.
#[derive(SystemInput)]
struct NoIn {}

#[derive(SystemOutput)]
struct NoOut {}

// ---------------------------------------------------------------------------
// A cyclic producer: writes an incrementing `Imu` every cycle. No inputs.
// ---------------------------------------------------------------------------

struct Producer {
    n: f64,
    /// Records written per execute; more than one bursts past a per-cycle mirror.
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
    /// When false the consumer never reads, so its view eventually fills the ring.
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
            // Record, once, how many inits had run before the first execute.
            if let Some(ic) = &self.init_counter {
                let _ = c.compare_exchange(0, ic.load(Relaxed), Relaxed, Relaxed);
            }
        }
        if !self.drain {
            return; // ignore the input so the ring fills up
        }
        if let Ok(Some(imu)) = input.imu.latest() {
            self.seen.borrow_mut().push(imu.get().omega);
        }
    }
}

fn config() -> CoordinatorConfig {
    CoordinatorConfig {
        cycle_rate: 1000.0,
        default_depth: crate::DEFAULT_DEPTH,
        clock: ClockMode::Wall,
        ..CoordinatorConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Two-cyclic-system graph, end to end (the headline smoke test).
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn two_system_end_to_end() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen: seen.clone(),
            drain: true,
            init_counter: None,
            first_exec_init: None,
        },
    ));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let mut coord = b.build().unwrap();

    coord.run_for(5).await;

    // The producer runs before the consumer each cycle (registration order), so
    // the consumer samples this cycle's fresh value: 1.0, 2.0, .. 5.0.
    assert_eq!(*seen.borrow(), vec![1.0, 2.0, 3.0, 4.0, 5.0]);
}

// ---------------------------------------------------------------------------
// A consumer that never drains backpressures its producer: nothing is
// overwritten, nothing stops, and the cycle loop never blocks because the
// producer's failed writes are counted rather than returned.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn idle_consumer_backpressures_producer() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen: seen.clone(),
            drain: false, // never reads, so its view eventually fills the ring
            init_counter: None,
            first_exec_init: None,
        },
    ));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let mut coord = b.build().unwrap();

    coord.run_for(60).await;

    // Both systems keep running; the writer's rejected writes were dropped and
    // counted on the producer's health.
    assert!(
        coord.stopped().is_empty(),
        "backpressure never stops a system"
    );
    assert!(seen.borrow().is_empty(), "the consumer never drained");
}

// ---------------------------------------------------------------------------
// Async system wired through a private copy-in buffer.
// ---------------------------------------------------------------------------

struct AsyncConsumer {
    count: Arc<AtomicU64>,
    last: Arc<AtomicU64>,
    shutdown: Arc<AtomicBool>,
}

#[derive(SystemInput)]
struct AsyncIn {
    imu: Input<Imu, Notifier>,
}

#[derive(SystemOutput)]
struct AsyncNoOut {}

impl System for AsyncConsumer {
    type Input = AsyncIn;
    type Output = Out<AsyncNoOut, Notifier>;
    const NAME: &'static str = "async_consumer";

    fn shutdown(&mut self, _output: &mut Self::Output) {
        self.shutdown.store(true, Relaxed);
    }
}

impl AsyncSystem for AsyncConsumer {
    async fn run(
        &mut self,
        context: &crate::AsyncContext,
        input: &mut Self::Input,
        _o: &mut Self::Output,
    ) {
        while let Some(Ok(imu)) = context.until_cancelled(input.imu.recv()).await {
            self.count.fetch_add(1, Relaxed);
            self.last.store(imu.get().omega as u64, Relaxed);
        }
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn async_through_copy_in() {
    let count = Arc::new(AtomicU64::new(0));
    let last = Arc::new(AtomicU64::new(0));
    let shutdown = Arc::new(AtomicBool::new(false));
    let mut producer = Producer::new();
    producer.burst = 8; // burst past the copy-in's per-cycle mirror (latest-wins)

    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), producer));
    let asy = b.push_node(async_node(
        AsyncConsumer::NAME.into(),
        AsyncConsumer {
            count: count.clone(),
            last: last.clone(),
            shutdown: shutdown.clone(),
        },
    ));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: asy,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let mut coord = b.build().unwrap();

    // Completes without blocking despite the burst; the copy-in mirrors only the
    // newest record per cycle, and a full private ring skips rather than suspends.
    coord.run_for(30).await;

    assert!(
        count.load(Relaxed) >= 1,
        "async system received via recv through the copy-in"
    );
    assert!(last.load(Relaxed) > 0, "received a real sample");
    assert!(shutdown.load(Relaxed), "recv cancellation ran shutdown");
}

// ---------------------------------------------------------------------------
// Build-time validation.
// ---------------------------------------------------------------------------

#[test]
fn validation_unknown_port() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen: Rc::new(RefCell::new(Vec::new())),
            drain: true,
            init_counter: None,
            first_exec_init: None,
        },
    ));
    // Both sides name the same frame id, so connect accepts the edge, but the
    // producer has no `Nav` output port and build reports UnknownPort.
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Nav::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Nav::FRAME_ID),
        },
    );
    assert!(matches!(b.build(), Err(WireError::UnknownPort { .. })));
}

#[test]
fn validation_incompatible_edge() {
    // An edge whose endpoints carry different frame ids (both ports exist) is
    // rejected at build. This is the shape a mismatched `out=`/`in=` wiring pair
    // lands as: resolve builds each `PortRef` from its own descriptor, so the
    // mismatch surfaces here rather than at edge-record time.
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let backer = b.push_node(cyclic_node(Backer::NAME.into(), Backer)); // Nav out
    let cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen: Rc::new(RefCell::new(Vec::new())),
            drain: true,
            init_counter: None,
            first_exec_init: None,
        },
    )); // Imu in
    b.connect(
        PortRef {
            system: backer,
            port: PortId::Component(Nav::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    assert!(matches!(b.build(), Err(WireError::Incompatible { .. })));
}

#[test]
fn validation_unconnected_input() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let _prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let _cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen: Rc::new(RefCell::new(Vec::new())),
            drain: true,
            init_counter: None,
            first_exec_init: None,
        },
    ));
    // With no connect call at all, the consumer's `imu` input is unconnected.
    assert!(matches!(b.build(), Err(WireError::UnconnectedInput { .. })));
}

#[test]
fn validation_invalid_cycle_rate_wall_clock() {
    // A zero, negative, NaN, or infinite cycle_rate would panic inside
    // `Duration::from_secs_f64` at run time under a Wall clock, so build()
    // rejects it up front.
    for rate in [0.0, -5.0, f64::NAN, f64::INFINITY] {
        let mut b = crate::coordinator::init::InitGraph::new(CoordinatorConfig {
            cycle_rate: rate,
            default_depth: crate::DEFAULT_DEPTH,
            clock: ClockMode::Wall,
            ..CoordinatorConfig::default()
        });
        b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
        assert!(
            matches!(b.build(), Err(WireError::InvalidCycleRate { .. })),
            "rate {rate} rejected at build"
        );
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn simulated_clock_ignores_cycle_rate() {
    // `cycle_rate` is ignored under `Simulated`, so an unusable rate must neither
    // fail build nor panic mid-run; the cycle budget is only computed under Wall.
    let mut b = crate::coordinator::init::InitGraph::new(CoordinatorConfig {
        cycle_rate: 0.0,
        default_depth: crate::DEFAULT_DEPTH,
        clock: ClockMode::Simulated {
            dt: Duration::from_micros(100),
        },
        ..CoordinatorConfig::default()
    });
    b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let mut coord = b
        .build()
        .expect("cycle_rate is not validated under Simulated");
    coord.run_for(3).await; // must complete without touching Duration::from_secs_f64
}

#[test]
fn simulated_clock_rejects_a_zero_step() {
    let b = crate::coordinator::init::InitGraph::new(CoordinatorConfig {
        clock: ClockMode::Simulated { dt: Duration::ZERO },
        ..CoordinatorConfig::default()
    });
    assert!(matches!(
        b.build(),
        Err(WireError::InvalidSimulatedStep { .. })
    ));
}

#[test]
fn validation_double_connect() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod1 = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let prod2 = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen: Rc::new(RefCell::new(Vec::new())),
            drain: true,
            init_counter: None,
            first_exec_init: None,
        },
    ));
    b.connect(
        PortRef {
            system: prod1,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    b.connect(
        PortRef {
            system: prod2,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
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

    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(
        Producer::NAME.into(),
        Producer {
            n: 0.0,
            burst: 1,
            init_counter: Some(init_counter.clone()),
        },
    ));
    let cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen: Rc::new(RefCell::new(Vec::new())),
            drain: true,
            init_counter: Some(init_counter.clone()),
            first_exec_init: Some(first_exec_init.clone()),
        },
    ));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
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

// `Looper` produces `Imu` and consumes `Nav`; `Backer` produces `Nav` and
// consumes `Imu`. Plain-connecting both directions closes a two-system cycle.
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
        let _ = input.nav.latest(); // sample the one-cycle-late feedback
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
            Ok(Some(imu)) => imu.get().omega,
            _ => 0.0,
        };
        let _ = o.nav.write(&Nav {
            timestamp: now,
            angle,
        });
    }
}

#[test]
fn feedback_cycle_unbroken_is_rejected() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let looper = b.push_node(cyclic_node(Looper::NAME.into(), Looper { n: 0.0 }));
    let backer = b.push_node(cyclic_node(Backer::NAME.into(), Backer));
    // Both directions are plain forward edges, which closes an unbroken cycle.
    b.connect(
        PortRef {
            system: looper,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: backer,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    b.connect(
        PortRef {
            system: backer,
            port: PortId::Component(Nav::FRAME_ID),
        },
        PortRef {
            system: looper,
            port: PortId::Component(Nav::FRAME_ID),
        },
    );
    let err = b.build().err().expect("the cycle is rejected at build");
    assert!(
        matches!(&err, WireError::FeedbackCycle { systems } if systems.iter().any(|s| s == "looper") && systems.iter().any(|s| s == "backer")),
        "{err:?}"
    );
}

#[cfg(not(miri))]
#[stellarator::test]
async fn delayed_edge_allows_feedback_loop() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let looper = b.push_node(cyclic_node(Looper::NAME.into(), Looper { n: 0.0 }));
    let backer = b.push_node(cyclic_node(Backer::NAME.into(), Backer));
    // The forward Imu edge; the Nav back-edge is the explicit one-cycle delay.
    b.connect(
        PortRef {
            system: looper,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: backer,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    b.connect_delayed(
        PortRef {
            system: backer,
            port: PortId::Component(Nav::FRAME_ID),
        },
        PortRef {
            system: looper,
            port: PortId::Component(Nav::FRAME_ID),
        },
    );
    let mut coord = b
        .build()
        .expect("connect_delayed breaks the cycle so it builds");

    coord.run_for(5).await;

    assert!(coord.stopped().is_empty(), "no system hard-stopped");
}

// ---------------------------------------------------------------------------
// Self-loop frame edges: a system plainly connected to itself is the tightest
// feedback loop, so it needs `connect_delayed` like any other loop.
// ---------------------------------------------------------------------------

// `SelfLoop` consumes and produces `Imu`, so its input can only ever be its
// own previous-cycle output.
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
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let s = b.push_node(cyclic_node(SelfLoop::NAME.into(), SelfLoop { n: 0.0 }));
    // A plain frame edge from a system to itself is a one-member feedback cycle.
    b.connect(
        PortRef {
            system: s,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: s,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let err = b
        .build()
        .err()
        .expect("a plain self-edge is rejected at build");
    assert!(
        matches!(&err, WireError::FeedbackCycle { systems } if systems == &vec!["self_loop"]),
        "{err:?}"
    );
}

#[cfg(not(miri))]
#[stellarator::test]
async fn self_loop_delayed_connect_builds_and_runs() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let s = b.push_node(cyclic_node(SelfLoop::NAME.into(), SelfLoop { n: 0.0 }));
    b.connect_delayed(
        PortRef {
            system: s,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: s,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let mut coord = b
        .build()
        .expect("connect_delayed declares the self-feedback");

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
    let mut b = crate::coordinator::init::InitGraph::new(config());
    // The consumer registers, and therefore steps, before its producer.
    let cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen: Rc::new(RefCell::new(Vec::new())),
            drain: true,
            init_counter: None,
            first_exec_init: None,
        },
    ));
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let err = b.build().err().expect("a backward frame edge is rejected");
    assert!(
        matches!(
            err,
            WireError::StaleFrameEdge {
                ref producer,
                ref consumer,
                ..
            } if producer == "producer" && consumer == "consumer"
        ),
        "{err:?}"
    );
}

#[cfg(not(miri))]
#[stellarator::test]
async fn backward_frame_edge_allowed_when_delayed() {
    // The same backward edge declared with `connect_delayed` builds; the one-cycle
    // staleness is now explicit, and the consumer samples last cycle's value.
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen: seen.clone(),
            drain: true,
            init_counter: None,
            first_exec_init: None,
        },
    ));
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    b.connect_delayed(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let mut coord = b.build().expect("connect_delayed declares the staleness");

    coord.run_for(5).await;

    // The consumer steps first each cycle: nothing on cycle 1, then last cycle's value.
    assert_eq!(*seen.borrow(), vec![1.0, 2.0, 3.0, 4.0]);
}

#[cfg(not(miri))]
#[stellarator::test]
async fn backward_edge_to_async_consumer_is_allowed() {
    // Registration order carries no execution-order meaning for an async consumer,
    // which reads through the post-step copy-in, so registering it before its
    // producer is fine; the StaleFrameEdge check only covers cyclic-to-cyclic edges.
    let count = Arc::new(AtomicU64::new(0));
    let last = Arc::new(AtomicU64::new(0));
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let asy = b.push_node(async_node(
        AsyncConsumer::NAME.into(),
        AsyncConsumer {
            count: count.clone(),
            last: last.clone(),
            shutdown: Arc::new(AtomicBool::new(false)),
        },
    ));
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: asy,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
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
        let mut b = crate::coordinator::init::InitGraph::new(config());
        b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
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
        name: std::sync::Arc::from("a"),
        reason: StopReason::Panicked,
    };
    let b = StoppedSystem {
        name: std::sync::Arc::from("b"),
        reason: StopReason::Panicked,
    };
    // Equal length but a different member, as when one slot recovers the same
    // cycle another stops. Length alone cannot distinguish these sets.
    assert!(stopped_set_changed(
        std::slice::from_ref(&a),
        std::slice::from_ref(&b)
    ));
    // Identical sets are unchanged; length changes still register.
    assert!(!stopped_set_changed(
        &[a.clone(), b.clone()],
        &[a.clone(), b]
    ));
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
    let mut b = crate::coordinator::init::InitGraph::new(CoordinatorConfig {
        cycle_rate: 1000.0,
        default_depth: crate::DEFAULT_DEPTH,
        clock: ClockMode::Simulated { dt },
        ..CoordinatorConfig::default()
    });
    b.push_node(cyclic_node(
        StampRec::NAME.into(),
        StampRec {
            stamps: stamps.clone(),
        },
    ));
    let mut coord = b.build().unwrap();

    coord.run_for(10).await;

    let s = stamps.borrow();
    assert_eq!(s.len(), 10, "one stamp per cycle");
    let step = dt.as_micros() as i64;
    for k in 1..s.len() {
        // Cycle k sits exactly k*dt past cycle 0, and the clock is strictly rising.
        assert_eq!(
            s[k].0 - s[0].0,
            k as i64 * step,
            "cycle {k} at start + k*dt"
        );
        assert!(s[k].0 > s[k - 1].0, "monotonically increasing");
    }
}

#[test]
fn simulated_clock_does_not_wrap_past_u32_cycles() {
    // Computing `epoch + dt * k as u32` truncates the cycle index and jumps the
    // timeline back to the epoch every 2^32 cycles. `simulated_now` uses wide
    // integers, so it stays exact and strictly monotonic across that boundary.
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
    assert_eq!(
        at.0 - epoch.0,
        8_333 * (1i64 << 32),
        "exactly k*dt past epoch"
    );
}

#[test]
fn simulated_clock_saturates_instead_of_wrapping() {
    let at = super::simulated_now(Timestamp(i64::MAX - 1), Duration::MAX, u64::MAX);
    assert_eq!(at, Timestamp(i64::MAX));
}

// ---------------------------------------------------------------------------
// Message edges: typed `MsgOut<M>` to `MsgIn<M>`, fan-in, and their wiring
// validation (id mismatch, duplicate edges, optional inputs).
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema)]
struct TestEvent {
    seq: u64,
}

impl crate::NamedMsg for TestEvent {
    const NAME: &'static str = "TestEvent";
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
        input
            .events
            .drain(|e| self.seen.borrow_mut().push(e.seq))
            .unwrap();
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn msg_edge_two_cyclic_systems() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(MsgProducer::NAME.into(), MsgProducer { n: 0 }));
    let cons = b.push_node(cyclic_node(
        MsgConsumer::NAME.into(),
        MsgConsumer { seen: seen.clone() },
    ));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Packet(TestEvent::ID),
        },
        PortRef {
            system: cons,
            port: PortId::Packet(TestEvent::ID),
        },
    );
    let mut coord = b.build().unwrap();

    coord.run_for(4).await;

    // The producer emits 1..=4; the consumer drains each the same cycle (producer runs first).
    assert_eq!(*seen.borrow(), vec![1, 2, 3, 4]);
}

#[cfg(not(miri))]
#[stellarator::test]
async fn msg_fanin_two_emitters_one_consumer() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod_a = b.push_node(cyclic_node("emitter_a".into(), MsgProducer { n: 0 }));
    let prod_b = b.push_node(cyclic_node("emitter_b".into(), MsgProducer { n: 100 }));
    let cons = b.push_node(cyclic_node(
        MsgConsumer::NAME.into(),
        MsgConsumer { seen: seen.clone() },
    ));
    // Two producers fan in to one message input without a `DoubleConnect`.
    b.connect(
        PortRef {
            system: prod_a,
            port: PortId::Packet(TestEvent::ID),
        },
        PortRef {
            system: cons,
            port: PortId::Packet(TestEvent::ID),
        },
    );
    b.connect(
        PortRef {
            system: prod_b,
            port: PortId::Packet(TestEvent::ID),
        },
        PortRef {
            system: cons,
            port: PortId::Packet(TestEvent::ID),
        },
    );
    let mut coord = b.build().unwrap();

    coord.run_for(2).await;

    // Both producers' records arrive (emitter_a: 1,2; emitter_b: 101,102).
    let mut got = seen.borrow().clone();
    got.sort_unstable();
    assert_eq!(got, vec![1, 2, 101, 102]);
}

#[test]
fn msg_exact_duplicate_edge_is_rejected() {
    // Fan-in of distinct producers is legal (covered above); the exact same edge
    // twice would deliver every record twice, so build() rejects the duplicate.
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(MsgProducer::NAME.into(), MsgProducer { n: 0 }));
    let cons = b.push_node(cyclic_node(
        MsgConsumer::NAME.into(),
        MsgConsumer {
            seen: Rc::new(RefCell::new(Vec::new())),
        },
    ));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Packet(TestEvent::ID),
        },
        PortRef {
            system: cons,
            port: PortId::Packet(TestEvent::ID),
        },
    );
    // The copy-pasted duplicate: same producer, same consumer, same port.
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Packet(TestEvent::ID),
        },
        PortRef {
            system: cons,
            port: PortId::Packet(TestEvent::ID),
        },
    );
    let err = b
        .build()
        .err()
        .expect("the duplicate message edge is rejected");
    assert!(
        matches!(
            err,
            WireError::DuplicateEdge {
                ref producer,
                ref consumer,
                ..
            } if producer == "msg_producer" && consumer == "msg_consumer"
        ),
        "{err:?}"
    );
}

#[test]
fn delayed_edge_into_log_input_is_rejected() {
    // `delayed` marks a one-cycle-late snapshot sample; on a Log input it means
    // nothing, so build rejects the edge instead of silently ignoring the flag.
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(MsgProducer::NAME.into(), MsgProducer { n: 0 }));
    let cons = b.push_node(cyclic_node(
        MsgConsumer::NAME.into(),
        MsgConsumer {
            seen: Rc::new(RefCell::new(Vec::new())),
        },
    ));
    b.connect_delayed(
        PortRef {
            system: prod,
            port: PortId::Packet(TestEvent::ID),
        },
        PortRef {
            system: cons,
            port: PortId::Packet(TestEvent::ID),
        },
    );
    let err = b.build().err().expect("delayed Log edge is rejected");
    assert!(
        matches!(
            err,
            WireError::DelayedLogEdge {
                ref producer,
                ref consumer,
                ..
            } if producer == "msg_producer" && consumer == "msg_consumer"
        ),
        "{err:?}"
    );
}

// A consumer whose hand-written input descriptor declares the ill-defined
// FanIn::Many with Delivery::Snapshot combination.
struct SnapshotFanInConsumer;

struct SnapshotFanInIn {
    // Bound but never drained: the test only exercises build-time validation.
    #[allow(dead_code)]
    imu: Input<Imu>,
}

impl SystemInput for SnapshotFanInIn {
    fn decls() -> crate::Declarations {
        vec![crate::PortDesc::of::<Imu>().with_fan_in(crate::FanIn::Many)].into()
    }
}

impl crate::BindPorts for SnapshotFanInIn {
    fn bind<S: crate::RingSource>(src: &mut S) -> Self {
        Self {
            imu: Input::bind(src),
        }
    }
}

impl System for SnapshotFanInConsumer {
    type Input = SnapshotFanInIn;
    type Output = Out<NoOut>;
    const NAME: &'static str = "snapshot_fan_in";
}

impl CyclicSystem for SnapshotFanInConsumer {
    fn execute(&mut self, _now: Timestamp, _in: &mut SnapshotFanInIn, _o: &mut Self::Output) {}
}

#[test]
fn snapshot_fan_in_is_rejected() {
    // Latest-wins across several producers is ill-defined, so many-producer
    // snapshot inputs are a build error even before any edge is connected.
    let mut b = crate::coordinator::init::InitGraph::new(config());
    b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    b.push_node(cyclic_node(
        SnapshotFanInConsumer::NAME.into(),
        SnapshotFanInConsumer,
    ));
    let err = b.build().err().expect("Many × Snapshot is rejected");
    assert!(
        matches!(
            err,
            WireError::SnapshotFanIn {
                ref system,
                ..
            } if system == "snapshot_fan_in"
        ),
        "{err:?}"
    );
}

#[cfg(not(miri))]
#[stellarator::test]
async fn msg_input_may_be_unconnected() {
    // A message input with zero producers builds fine and drains nothing, unlike
    // a frame input, which would be an `UnconnectedInput` build error.
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let _cons = b.push_node(cyclic_node(
        MsgConsumer::NAME.into(),
        MsgConsumer { seen: seen.clone() },
    ));
    let mut coord = b.build().unwrap();
    coord.run_for(3).await;
    assert!(seen.borrow().is_empty());
}

// ---------------------------------------------------------------------------
// AllOutputs receive-all tap: a system declaring `AllOutputs` sees every frame
// output and telemetered message channel, and reserves itself a reader slot on
// every one of those buffers.
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
        let (frames, msgs) = o
            .all
            .entries()
            .fold((0, 0), |(f, m), e| match e.desc.schema {
                crate::PortSchema::Table { .. } => (f + 1, m),
                crate::PortSchema::Postcard { .. } => (f, m + 1),
            });
        self.frame_outs.set(frames);
        self.msg_chans.set(msgs);
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
    let mut b = crate::coordinator::init::InitGraph::new(config());
    // A producer of a telemetered message channel plus its implicit health/log frame outputs.
    let _prod = b.push_node(cyclic_node(MsgProducer::NAME.into(), MsgProducer { n: 0 }));
    let _tap = b.push_node(cyclic_node(
        AllTap::NAME.into(),
        AllTap {
            frame_outs: frame_outs.clone(),
            msg_chans: msg_chans.clone(),
        },
    ));
    let mut coord = b.build().unwrap();
    coord.run_for(1).await;

    // The tap has no wired edges, yet it sees the producer's message channel and
    // the frame outputs (health/log of both systems plus the coordinator's own).
    // Build succeeding also proves the receive-all port reserved itself a reader
    // slot on every buffer.
    assert!(
        msg_chans.get() >= 1,
        "sees the message channel: {}",
        msg_chans.get()
    );
    assert!(
        frame_outs.get() >= 2,
        "sees frame outputs: {}",
        frame_outs.get()
    );
}

// ---------------------------------------------------------------------------
// Capabilities: AllOutputs is a SystemDescriptor capability, not a port. It
// gets no sentinel desc, no placeholder ring, and no bind-cursor position.
// ---------------------------------------------------------------------------

struct MixedTap;

/// A capability field first and a real port after it. If the capability
/// consumed a bind-cursor position, `imu` would bind over the wrong ring.
#[derive(SystemOutput)]
struct MixedTapOut {
    all: AllOutputs,
    imu: crate::Output<Imu>,
}

impl System for MixedTap {
    type Input = NoIn;
    type Output = Out<MixedTapOut>;
    const NAME: &'static str = "mixed_tap";
}

impl CyclicSystem for MixedTap {
    fn execute(&mut self, now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        o.imu.publish(&Imu {
            timestamp: now,
            omega: 7.5,
        });
    }
}

/// The descriptor splits ports from capabilities: the port lists carry only
/// wired ports, and the capability set carries `ReceiveAll`.
#[test]
fn capability_lifts_out_of_the_port_lists() {
    let d = <MixedTap as CyclicSystem>::descriptor();
    assert_eq!(d.capabilities, vec![crate::Capability::ReceiveAll]);
    let names: Vec<&str> = d.outputs.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["imu", "health", "log"],
        "wired ports only, in field order"
    );
}

/// End-to-end bind alignment: with the capability declared before a real port,
/// the port still binds its own ring and its records are readable under its
/// registry key.
#[cfg(not(miri))]
#[stellarator::test]
async fn capability_field_consumes_no_bind_cursor() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    // Registered last: a cyclic ReceiveAll holder must step after everything else.
    b.push_node(cyclic_node(MixedTap::NAME.into(), MixedTap));
    let mut coord = b.build().unwrap();
    // Claim the view before any cycle runs; a view starts at the live edge of its
    // ring, so it must exist before data flows.
    let registry = coord.registry();
    let key = metor_proto::types::ComponentId::new("mixed_tap.imu");
    let entry = registry
        .get(key)
        .expect("the port after the capability is registered");
    let mut view = entry.view().expect("reader slot");
    coord.run_for(2).await;

    let mut buf = Vec::new();
    assert!(
        view.try_read_into(&mut buf).expect("readable ring"),
        "the port bound the ring the coordinator reserved for it"
    );
}

#[test]
fn command_ring_registered_but_untelemetered() {
    // The coordinator's command channel is an ordinary registered entry, findable
    // by key through the full registry, but flagged untelemetered so the
    // AllOutputs and downlink surfaces never see inbound control.
    let mut b = crate::coordinator::init::InitGraph::new(config());
    b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let coord = b.build().unwrap();
    let registry = coord.registry();
    let key = metor_proto::types::ComponentId::new("coordinator.commands");
    let entry = registry.get(key).expect("command ring is registered");
    assert!(!entry.telemetered(), "inbound control is never downlinked");
    assert!(matches!(
        entry.desc.schema,
        crate::PortSchema::Postcard { .. }
    ));
    assert!(matches!(entry.delivery(), crate::Delivery::Log));
}

#[test]
fn coordinator_bundle_registry_keys_are_golden() {
    // The coordinator's channels come off its declared bundle, but external
    // consumers address these buffers by their key strings, so the exact keys
    // are pinned here.
    let mut b = crate::coordinator::init::InitGraph::new(config());
    b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let coord = b.build().unwrap();
    let registry = coord.registry();
    for (key, telemetered) in [
        ("coordinator.health", true),
        ("coordinator.log", true),
        ("coordinator.coordinator_status", true),
        ("coordinator.sequences", true),
        ("coordinator.commands", false),
    ] {
        let entry = registry
            .get(metor_proto::types::ComponentId::new(key))
            .unwrap_or_else(|| panic!("{key} is registered"));
        assert_eq!(entry.telemetered(), telemetered, "{key}");
        assert_eq!(&*entry.instance, "coordinator", "{key}");
    }
}

#[test]
fn duplicate_registry_key_is_rejected() {
    // Two systems registered under the same instance name compute colliding
    // `<instance>.<name>` keys for every port. Build reports the collision
    // instead of letting one entry silently shadow the other.
    let mut b = crate::coordinator::init::InitGraph::new(config());
    b.push_node(cyclic_node("dup".into(), Producer::new()));
    b.push_node(cyclic_node("dup".into(), Producer::new()));
    let err = b
        .build()
        .err()
        .expect("colliding registry keys are rejected");
    assert!(
        matches!(&err, WireError::DuplicateRegistryKey { key } if key == "dup.imu"),
        "{err:?}"
    );
}

// A producer whose frame output opts out of telemetry.
struct QuietProducer;

struct QuietOut {
    imu: Output<Imu>,
}

impl SystemOutput for QuietOut {
    fn decls() -> crate::Declarations {
        vec![crate::PortDesc::of::<Imu>().untelemetered()].into()
    }
}

impl crate::BindPorts for QuietOut {
    fn bind<S: crate::RingSource>(src: &mut S) -> Self {
        Self {
            imu: Output::bind(src),
        }
    }
}

impl System for QuietProducer {
    type Input = NoIn;
    type Output = Out<QuietOut>;
    const NAME: &'static str = "quiet";
}

impl CyclicSystem for QuietProducer {
    fn execute(&mut self, now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        let _ = o.imu.write(&Imu {
            timestamp: now,
            omega: 1.0,
        });
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn untelemetered_frame_output_hidden_from_all_outputs() {
    // A frame output declared untelemetered stays a fully registered, readable
    // buffer but is invisible through `AllOutputs`, the frame-side twin of the
    // command-channel opt-out.
    let frame_outs = Rc::new(std::cell::Cell::new(0));
    let msg_chans = Rc::new(std::cell::Cell::new(0));
    let mut b = crate::coordinator::init::InitGraph::new(config());
    b.push_node(cyclic_node(QuietProducer::NAME.into(), QuietProducer));
    b.push_node(cyclic_node(
        AllTap::NAME.into(),
        AllTap {
            frame_outs: frame_outs.clone(),
            msg_chans: msg_chans.clone(),
        },
    ));
    let mut coord = b.build().unwrap();

    let registry = coord.registry();
    let key = metor_proto::types::ComponentId::new("quiet.imu");
    let entry = registry
        .get(key)
        .expect("untelemetered frame is registered");
    assert!(!entry.telemetered());

    coord.run_for(1).await;
    // The tap sees the two systems' health frames (2) plus the coordinator's
    // health and status (2), but not quiet.imu; the log ports are message
    // channels. The exact count keeps a filter regression loud.
    assert_eq!(frame_outs.get(), 4, "quiet.imu is filtered at the source");
}

// ---------------------------------------------------------------------------
// Process systems: which rings become session-dir files.
// ---------------------------------------------------------------------------

/// A process registration marks exactly the crossing buffers as file-backed —
/// every output of the process system (implicit health/log included) plus the
/// producer output it consumes — and `alloc_rings` creates precisely those as
/// ring files in a session directory that is removed when the alloc drops.
/// A bystander system's rings stay heap.
#[test]
fn proc_rings_are_file_backed() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let _bystander = b.push_node(cyclic_node("bystander".into(), Producer::new()));
    // The process system's descriptor is a real consumer's, exactly what a
    // describe worker would hand back; no worker is spawned in this test
    // (spawn happens at build(), which is never called).
    let desc = Consumer {
        seen: Rc::new(RefCell::new(Vec::new())),
        drain: true,
        init_counter: None,
        first_exec_init: None,
    }
    .instance_descriptor();
    let n_proc_outputs = desc.outputs.len();
    let cons = b.add_proc_cyclic(
        "proc-cons",
        desc,
        "/nonexistent.so".into(),
        "cons",
        Vec::new(),
    );
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );

    let cons_edges = b.solve_edges().unwrap();
    let shared = b.shared_outputs(&cons_edges);
    // The producer's imu output crosses; its health/log do not.
    let imu_out = b.systems[prod.id]
        .desc
        .outputs
        .iter()
        .position(|p| p.id() == crate::PortId::Component(<Imu as crate::Frame>::FRAME_ID))
        .unwrap();
    assert!(shared.contains(&(prod.id, imu_out)));
    // Every process-system output crosses.
    for out_idx in 0..n_proc_outputs {
        assert!(shared.contains(&(cons.id, out_idx)));
    }
    assert_eq!(
        shared.len(),
        n_proc_outputs + 1,
        "nothing else crosses (bystander and coordinator rings stay heap)"
    );

    let fan_out = b.count_fan_out(&cons_edges);
    let alloc = b.alloc_rings(&cons_edges, &fan_out).unwrap();
    assert_eq!(
        alloc
            .ring_paths
            .keys()
            .copied()
            .collect::<std::collections::HashSet<_>>(),
        shared,
        "one ring file per crossing buffer"
    );
    let session = alloc.session.as_ref().expect("proc graph has a session");
    let session_path = session.path().to_path_buf();
    for path in alloc.ring_paths.values() {
        assert!(path.exists(), "ring file {} exists", path.display());
        assert!(path.starts_with(&session_path));
    }
    drop(alloc);
    assert!(!session_path.exists(), "session dir removed on drop");
}

/// A graph without process systems allocates no session and no ring files.
#[test]
fn heap_only_graph_has_no_session() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let cons = b.push_node(cyclic_node(
        Consumer::NAME.into(),
        Consumer {
            seen,
            drain: true,
            init_counter: None,
            first_exec_init: None,
        },
    ));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: cons,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let cons_edges = b.solve_edges().unwrap();
    let fan_out = b.count_fan_out(&cons_edges);
    let alloc = b.alloc_rings(&cons_edges, &fan_out).unwrap();
    assert!(alloc.session.is_none());
    assert!(alloc.ring_paths.is_empty());
}

// ---------------------------------------------------------------------------
// Process slots: which of a slot's rings become session-dir files.
// ---------------------------------------------------------------------------

/// Push a slot registration shaped like `add_slot`'s derived descriptor — an
/// Edge occupant input plus the Host [`SlotControlIn`], an occupant
/// [`SequenceStatus`] output, then the runner tail — without opening any
/// occupant artifact: the alloc passes under test read only the descriptor,
/// the prefix split, and the `process` flag.
fn push_slot_reg(
    b: &mut crate::coordinator::init::InitGraph,
    name: &'static str,
    process: bool,
) -> super::SystemHandle {
    let base = crate::SystemDescriptor {
        name: name.into(),
        kind: crate::SystemKind::Cyclic,
        inputs: vec![crate::PortDesc::of::<Imu>()],
        outputs: Vec::new(),
        capabilities: Vec::new(),
    };
    let ports = super::slot::SlotPorts::for_occupant(&base, "occ").unwrap();
    b.push_system(
        ports.registered(name),
        name.to_string(),
        super::init::SystemBind::Slot(super::slot::SlotReg {
            allowed: Vec::new(),
            initial: None,
            ports,
            process,
        }),
    )
}

/// A process slot's backing selection marks exactly the occupant prefix: its
/// own outputs, the producer behind each Edge input, and the Host control
/// ring. The runner tail — the `commands` fan-in (and so its producers), the
/// self-tap, the [`SlotStatus`]/events outputs — sits past the prefix split
/// and stays heap.
#[test]
fn process_slot_rings_are_file_backed() {
    use metor_proto_wkt::SequenceCommand;
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let slot = push_slot_reg(&mut b, "seq-slot", true);
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: slot,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    b.connect(
        PortRef {
            system: b.coordinator_handle(),
            port: PortId::Packet(SequenceCommand::ID),
        },
        PortRef {
            system: slot,
            port: PortId::Packet(SequenceCommand::ID),
        },
    );

    let cons_edges = b.solve_edges().unwrap();
    let shared = b.shared_outputs(&cons_edges);
    let imu_out = b.systems[prod.id]
        .desc
        .outputs
        .iter()
        .position(|p| p.id() == crate::PortId::Component(<Imu as crate::Frame>::FRAME_ID))
        .unwrap();
    assert!(shared.contains(&(slot.id, 0)), "occupant output crosses");
    assert!(
        shared.contains(&(prod.id, imu_out)),
        "Edge-input producer crosses"
    );
    assert_eq!(
        shared.len(),
        2,
        "nothing else crosses: not the tail outputs, not the command producer"
    );

    let fan_out = b.count_fan_out(&cons_edges);
    let alloc = b.alloc_rings(&cons_edges, &fan_out).unwrap();
    assert_eq!(
        alloc
            .ring_paths
            .keys()
            .copied()
            .collect::<std::collections::HashSet<_>>(),
        shared,
        "one ring file per crossing buffer"
    );
    assert_eq!(
        alloc.host_input_paths.keys().copied().collect::<Vec<_>>(),
        vec![(slot.id, 1)],
        "the Host control ring is the one file-backed input"
    );
    let session = alloc
        .session
        .as_ref()
        .expect("process-slot graph has a session");
    let session_path = session.path().to_path_buf();
    for path in alloc
        .ring_paths
        .values()
        .chain(alloc.host_input_paths.values())
    {
        assert!(path.exists(), "ring file {} exists", path.display());
        assert!(path.starts_with(&session_path));
    }
    drop(alloc);
    assert!(!session_path.exists(), "session dir removed on drop");
}

/// The same slot registered in-process allocates all-heap: no session
/// directory, no output ring files, and a heap control ring.
#[test]
fn in_process_slot_stays_heap() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let slot = push_slot_reg(&mut b, "seq-slot", false);
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: slot,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let cons_edges = b.solve_edges().unwrap();
    let fan_out = b.count_fan_out(&cons_edges);
    let alloc = b.alloc_rings(&cons_edges, &fan_out).unwrap();
    assert!(alloc.session.is_none());
    assert!(alloc.ring_paths.is_empty());
    assert!(alloc.host_input_paths.is_empty());
    assert!(
        alloc.host_input_rings.contains_key(&(slot.id, 1)),
        "the control ring still exists, heap-backed"
    );
}

// ---------------------------------------------------------------------------
// The registered slot descriptor: add_slot's exact flattening.
// ---------------------------------------------------------------------------

/// Snapshot of the registered descriptor `add_slot` derives: the occupant's
/// user ports, the mount-appended [`SlotControlIn`]/[`SequenceStatus`], then
/// the runner tail, in exactly this sequence. Edge resolution, registry keys
/// (`"<slot>.sequences"` and friends), and the occupant-side positional bind
/// ABI all consume the flattened lists, so a change in this order or content
/// is a keyspace/ABI change, never a refactor.
#[test]
fn registered_slot_descriptor_snapshot() {
    use crate::sequence::{SequenceStatus, SlotControlIn};
    use crate::{Frame, PortConn, PortDesc, PortId, SlotStatus};
    use metor_proto_wkt::{ReloadSequences, SequenceChannelEvent, SequenceCommand};

    let occ = crate::AllowedOccupant {
        name: "seq".into(),
        params: Vec::new(),
        descriptor: crate::SystemDescriptor {
            name: "seq".into(),
            kind: crate::SystemKind::Cyclic,
            inputs: vec![PortDesc::of::<Imu>(), PortDesc::msg::<ReloadSequences>()],
            outputs: vec![PortDesc::of::<Nav>()],
            capabilities: Vec::new(),
        },
        // Never opened at registration; the path need not exist.
        backing: crate::OccupantBacking::Artifact("occ.so".into()),
    };
    let (desc, _, _) = plan_slot("snap", &[occ]).expect("a valid slot plans");

    let seq_status_id = PortId::Component(<SequenceStatus as Frame>::FRAME_ID);
    let got: Vec<(&str, PortId, PortConn)> = desc
        .inputs
        .iter()
        .map(|p| (p.name.as_str(), p.id(), p.conn))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "imu",
                PortId::Component(<Imu as Frame>::FRAME_ID),
                PortConn::Edge
            ),
            (
                "ReloadSequences",
                PortDesc::msg::<ReloadSequences>().id(),
                PortConn::Edge
            ),
            (
                "slot_control",
                PortId::Component(<SlotControlIn as Frame>::FRAME_ID),
                PortConn::Host
            ),
            (
                "SequenceCommand",
                PortDesc::msg::<SequenceCommand>().id(),
                PortConn::Edge
            ),
            ("sequence", seq_status_id, PortConn::SelfTap(seq_status_id)),
        ],
        "registered input sequence: occupant user ports, mount control, runner tail"
    );
    let got: Vec<(&str, PortId, PortConn)> = desc
        .outputs
        .iter()
        .map(|p| (p.name.as_str(), p.id(), p.conn))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "nav",
                PortId::Component(<Nav as Frame>::FRAME_ID),
                PortConn::Edge
            ),
            ("sequence", seq_status_id, PortConn::Edge),
            (
                "slot_status",
                PortId::Component(<SlotStatus as Frame>::FRAME_ID),
                PortConn::Host
            ),
            (
                "sequences",
                PortDesc::msg::<SequenceChannelEvent>().id(),
                PortConn::Host
            ),
        ],
        "registered output sequence: occupant user ports, mount status, runner tail"
    );
}

/// The descriptor-level `plan_slot` violations are clean [`SlotConfigError`]s,
/// one variant per defect. The occupants here are artifact-backed so nothing is
/// opened; only the registration checks run. (The pure-spec defects — an empty
/// allow set, an `initial` outside it — are gated at wiring `validate`, covered
/// there.)
#[test]
fn add_slot_rejects_contract_violations() {
    use crate::sequence::SlotControlIn;
    use crate::{PortDesc, SlotConfigError};

    fn art_occ(name: &str, inputs: Vec<PortDesc>) -> crate::AllowedOccupant {
        crate::AllowedOccupant {
            name: name.into(),
            params: Vec::new(),
            descriptor: crate::SystemDescriptor {
                name: "occ".into(),
                kind: crate::SystemKind::Cyclic,
                inputs,
                outputs: Vec::new(),
                capabilities: Vec::new(),
            },
            backing: crate::OccupantBacking::Artifact("occ.so".into()),
        }
    }

    assert!(matches!(
        plan_slot(
            "s",
            &[
                art_occ("first", vec![PortDesc::of::<Imu>()]),
                art_occ("second", vec![PortDesc::of::<Nav>()]),
            ],
        ),
        Err(SlotConfigError::OccupantMismatch { .. })
    ));
    assert!(matches!(
        plan_slot(
            "s",
            &[art_occ("pre-pack", vec![PortDesc::of::<SlotControlIn>()])]
        ),
        Err(SlotConfigError::ReservedPort { .. })
    ));
}

// ---------------------------------------------------------------------------
// Process slots: the SlotRunner proc backing over a fake occupant worker.
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod proc_slot {
    //! State-machine tests for the process-mode [`SlotRunner`], driven the
    //! way the proc unit tests drive `SeqWorker`: the ctl protocol is
    //! address-space agnostic, so the test plays the occupant worker over
    //! the slot's ctl file while the actually spawned child is an inert
    //! stand-in (`/bin/sh`, exiting immediately on its null stdin).

    use std::path::PathBuf;
    use std::time::Duration;

    use metor_fsw_ring::NoWake;
    use metor_proto::types::Timestamp;
    use metor_proto_wkt::{
        SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
    };

    use crate::abi::FswStatus;
    use crate::coordinator::slot::{
        AllowedOccupant, OccupantBacking, ProcParts, SlotRunner, slot_writer,
    };
    use crate::coordinator::{CyclicSlot, SlotState, StopReason, WorkerRunState};
    use crate::dynamic::FrameList;
    use crate::message::{MsgIn, MsgOut};
    use crate::port::{Input, Output};
    use crate::proc::{CtlWorker, WorkerCmd, WorkerState};
    use crate::sequence::{SequenceStatus, SlotControlIn};
    use crate::{Delivery, SlotStatus};

    /// Room enough for the runner's frames; sizing is not under test.
    const READERS: usize = 4;
    const DEPTH: usize = 8;

    /// One allowed occupant behind an artifact backing. The runner never
    /// opens the path, so it need not exist.
    fn proc_occ(name: &str) -> AllowedOccupant {
        AllowedOccupant {
            name: name.into(),
            params: Vec::new(),
            descriptor: crate::SystemDescriptor {
                name: "occ".into(),
                kind: crate::SystemKind::Cyclic,
                inputs: Vec::new(),
                outputs: Vec::new(),
                capabilities: Vec::new(),
            },
            backing: OccupantBacking::Artifact(PathBuf::from("/never-opened.so")),
        }
    }

    /// A process-mode runner over hand-allocated rings, plus the test's ends
    /// of them: the command writer, the events and status taps, and the
    /// occupant-side [`SequenceStatus`] writer the fake worker publishes
    /// through.
    struct Harness {
        runner: SlotRunner,
        cmds: MsgOut<SequenceCommand>,
        events: MsgIn<SequenceChannelEvent>,
        status: Input<SlotStatus>,
        seq_out: Output<SequenceStatus>,
        ctl_path: PathBuf,
        now: i64,
    }

    fn harness(dir: &tempfile::TempDir, occupants: &[&str]) -> Harness {
        let frame_ring = |desc: crate::PortDesc| {
            super::super::init::alloc_ring(desc.delivery, desc.max_size, DEPTH, READERS)
        };
        let control_ring = frame_ring(crate::PortDesc::of::<SlotControlIn>());
        let status_ring = frame_ring(crate::PortDesc::of::<SlotStatus>());
        let seq_ring = frame_ring(crate::PortDesc::of::<SequenceStatus>());
        let events_ring = super::super::init::alloc_ring(
            Delivery::Log,
            crate::PortDesc::msg::<SequenceChannelEvent>().max_size,
            DEPTH,
            READERS,
        );
        let cmd_ring = super::super::init::alloc_ring(
            Delivery::Log,
            crate::PortDesc::msg::<SequenceCommand>().max_size,
            DEPTH,
            READERS,
        );

        let ctl_path = dir.path().join("seq-slot.ctl");
        let manifests = occupants
            .iter()
            .map(|occ| {
                let path = dir.path().join(format!("seq-slot.{occ}.manifest"));
                std::fs::write(&path, b"unread by the stand-in child").unwrap();
                path
            })
            .collect();
        let runner = SlotRunner::new(
            std::sync::Arc::from("seq-slot"),
            occupants.iter().map(|o| proc_occ(o)).collect(),
            None,
            Vec::new(),
            Vec::new(),
            slot_writer::<SlotControlIn>(&control_ring),
            slot_writer::<SlotStatus>(&status_ring),
            super::super::init::owned_writer::<SequenceChannelEvent>(&events_ring),
            Input::new(seq_ring.view(NoWake).unwrap()),
            MsgIn::from_views(vec![cmd_ring.view(NoWake).unwrap()]),
            Some(ProcParts {
                manifests,
                ctl_path: ctl_path.clone(),
                exe: PathBuf::from("/bin/sh"),
                rings: Vec::new(),
                step_timeout: Duration::from_millis(200),
            }),
        );
        Harness {
            runner,
            cmds: super::super::init::owned_writer::<SequenceCommand>(&cmd_ring),
            events: MsgIn::from_views(vec![events_ring.view(NoWake).unwrap()]),
            status: Input::new(status_ring.view(NoWake).unwrap()),
            seq_out: slot_writer::<SequenceStatus>(&seq_ring),
            ctl_path,
            now: 0,
            // The rings live on inside the ports; the harness drops them.
        }
    }

    impl Harness {
        fn step(&mut self) {
            self.now += 1;
            self.runner.step(Timestamp(self.now));
        }

        fn send(&mut self, command: SequenceCommandKind) {
            self.cmds
                .emit(&SequenceCommand {
                    channel: "seq-slot".to_string(),
                    command,
                })
                .unwrap();
        }

        fn drain_events(&mut self) -> Vec<SequenceEventKind> {
            let mut out = Vec::new();
            self.events.drain(|e| out.push(e.kind)).unwrap();
            out
        }

        fn phase(&mut self) -> u8 {
            self.status
                .latest()
                .expect("ring readable")
                .expect("status published")
                .get()
                .phase
        }

        fn attach_ctl(&self) -> CtlWorker {
            CtlWorker::attach(&self.ctl_path).unwrap()
        }

        /// Drive a fresh occupant's pipeline to `Loaded` through a fake
        /// worker, asserting the `Loading` phase on the way, and hand back
        /// the fake's ctl end for the Running phase.
        fn load_occupant(&mut self, name: &str) -> CtlWorker {
            self.send(SequenceCommandKind::Load {
                name: name.to_string(),
            });
            self.step();
            assert!(matches!(self.runner.state(), SlotState::Loading));
            let ctl = self.attach_ctl();
            ctl.report(WorkerState::Attached);
            self.step(); // observes Attached, requests init
            assert!(matches!(self.runner.state(), SlotState::Loading));
            assert!(ctl.wait_init(), "init requested, not shutdown");
            ctl.report(WorkerState::Ready);
            self.step();
            assert!(matches!(self.runner.state(), SlotState::Loaded));
            ctl
        }
    }

    /// Load walks Empty → Loading (wire phase 2, worker `Restarting`) →
    /// Loaded, with the `Loading` event emitted at the spawn and the
    /// `Loaded` event deferred to the bind — and the command guards refuse
    /// `Start`/`Stop` mid-pipeline with `Refused` events.
    #[test]
    fn load_pipeline_phases_events_and_guards() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(&dir, &["alpha", "beta"]);
        h.runner.init();
        assert!(matches!(h.runner.state(), SlotState::Empty));

        h.send(SequenceCommandKind::Load {
            name: "alpha".to_string(),
        });
        h.step();
        assert!(matches!(h.runner.state(), SlotState::Loading));
        assert_eq!(h.phase(), 2);
        assert!(
            matches!(
                h.drain_events().as_slice(),
                [SequenceEventKind::Loading { name }] if name == "alpha"
            ),
            "the spawn announces Loading; Loaded waits for the bind"
        );
        let info = h.runner.worker_status().expect("process-mode slot");
        assert_eq!(info.state, WorkerRunState::Restarting);
        assert_ne!(info.pid, 0, "the spawned worker is telemetered");

        // Commands mid-pipeline are refused (with events), never applied.
        h.send(SequenceCommandKind::Start);
        h.send(SequenceCommandKind::Stop);
        h.step();
        assert!(matches!(h.runner.state(), SlotState::Loading));
        assert!(
            matches!(
                h.drain_events().as_slice(),
                [
                    SequenceEventKind::Refused { reason: start },
                    SequenceEventKind::Refused { reason: stop },
                ] if start.contains("start refused") && stop.contains("stop refused")
            ),
            "mid-pipeline commands are refused loudly"
        );

        let ctl = h.attach_ctl();
        ctl.report(WorkerState::Attached);
        h.step();
        assert!(matches!(h.runner.state(), SlotState::Loading));
        assert!(ctl.wait_init());
        ctl.report(WorkerState::Ready);
        h.step();
        assert!(matches!(h.runner.state(), SlotState::Loaded));
        assert_eq!(h.phase(), 1);
        assert!(matches!(
            h.drain_events().as_slice(),
            [SequenceEventKind::Loaded { name }] if name == "alpha"
        ));
        let info = h.runner.worker_status().unwrap();
        assert_eq!(info.state, WorkerRunState::Running);
        assert_ne!(info.pid, 0);
        assert_eq!(info.restarts, 0);
    }

    /// An acked `Done` folds through the existing terminal path: the staged
    /// `SequenceStatus` record (published before the ack, as the occupant
    /// would) refines the outcome, and the worker is not torn down.
    #[test]
    fn done_ack_folds_to_completed() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(&dir, &["alpha"]);
        h.runner.init();
        let ctl = h.load_occupant("alpha");
        h.drain_events();

        h.send(SequenceCommandKind::Start);
        // The occupant's terminal record lands before its Done ack; the
        // fake acks the one doorbell the Running poll rings.
        let _ = h.seq_out.write(&SequenceStatus {
            timestamp: Timestamp(0),
            run_state: 1,
            _pad: [0; 7],
            progress: FrameList::EMPTY,
        });
        let fake = std::thread::spawn(move || match ctl.next(0) {
            WorkerCmd::Step { seq, .. } => ctl.done(seq, FswStatus::Done),
            WorkerCmd::Shutdown => panic!("no shutdown was requested"),
        });
        h.step(); // Start applies, then the Running poll acks Done
        fake.join().unwrap();
        assert!(matches!(h.runner.state(), SlotState::Done { outcome: 1 }));
        assert!(matches!(
            h.drain_events().as_slice(),
            [SequenceEventKind::Started, SequenceEventKind::Completed]
        ));
        // Done holds its worker (and so its ring roles) until Reset/Load.
        let info = h.runner.worker_status().unwrap();
        assert_eq!(info.state, WorkerRunState::Running);
        assert_ne!(info.pid, 0);
        // Terminal: the occupant is not polled again.
        h.step();
        assert!(matches!(h.runner.state(), SlotState::Done { outcome: 1 }));
    }

    /// A worker death while Running is terminal — `Stopped { ProcessDied }`,
    /// a `Failed` event, no respawn — and an operator `Reset` brings a fresh
    /// pipeline up over the same rings.
    #[test]
    fn death_is_terminal_and_reset_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(&dir, &["alpha"]);
        h.runner.init();
        let ctl = h.load_occupant("alpha");
        h.drain_events();

        // Nothing acks (the stand-in child already exited): the step times
        // out, the reap resolves dead, and the stop is terminal.
        h.send(SequenceCommandKind::Start);
        h.step();
        assert!(matches!(
            h.runner.state(),
            SlotState::Stopped {
                reason: StopReason::ProcessDied
            }
        ));
        assert!(matches!(
            h.drain_events().as_slice(),
            [SequenceEventKind::Started, SequenceEventKind::Failed { .. }]
        ));
        let info = h.runner.worker_status().unwrap();
        assert_eq!((info.pid, info.state), (0, WorkerRunState::Stopped));
        assert_eq!(info.restarts, 1, "the death is counted");

        // No respawn behind the operator's back.
        h.step();
        assert!(matches!(h.runner.state(), SlotState::Stopped { .. }));
        assert_eq!(h.runner.worker_status().unwrap().pid, 0);

        // Reset spawns a fresh pipeline over the same manifest and rings.
        drop(ctl);
        h.send(SequenceCommandKind::Reset);
        h.step();
        assert!(matches!(h.runner.state(), SlotState::Loading));
        let ctl = h.attach_ctl();
        ctl.report(WorkerState::Attached);
        h.step();
        assert!(ctl.wait_init());
        ctl.report(WorkerState::Ready);
        h.step();
        assert!(matches!(h.runner.state(), SlotState::Loaded));
        assert!(matches!(
            h.drain_events().as_slice(),
            [
                SequenceEventKind::Loading { name: loading },
                SequenceEventKind::Loaded { name },
            ] if loading == "alpha" && name == "alpha"
        ));
        let info = h.runner.worker_status().unwrap();
        assert_eq!(info.state, WorkerRunState::Running);
        assert_ne!(info.pid, 0);
        assert_eq!(info.restarts, 1, "Reset is commanded, not a death");
    }

    /// A pipeline failure (the worker reports `Failed` during attach) lands
    /// the same terminal stop, naming the stage.
    #[test]
    fn pipeline_failure_is_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(&dir, &["alpha"]);
        h.runner.init();
        h.send(SequenceCommandKind::Load {
            name: "alpha".to_string(),
        });
        h.step();
        let ctl = h.attach_ctl();
        ctl.fail(3);
        h.step();
        assert!(matches!(
            h.runner.state(),
            SlotState::Stopped {
                reason: StopReason::ProcessDied
            }
        ));
        assert!(matches!(
            h.drain_events().as_slice(),
            [
                SequenceEventKind::Loading { .. },
                SequenceEventKind::Failed { reason },
            ] if reason.contains("attach")
        ));
        assert_eq!(h.runner.worker_status().unwrap().restarts, 1);
    }
}

/// A process system and a process slot share the one run session: every
/// crossing file of both lands in the same directory.
#[test]
fn proc_and_process_slot_share_a_session() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let desc = Consumer {
        seen: Rc::new(RefCell::new(Vec::new())),
        drain: true,
        init_counter: None,
        first_exec_init: None,
    }
    .instance_descriptor();
    let proc_sys = b.add_proc_cyclic(
        "proc-cons",
        desc,
        "/nonexistent.so".into(),
        "cons",
        Vec::new(),
    );
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: proc_sys,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let slot = push_slot_reg(&mut b, "seq-slot", true);
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: slot,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );

    let cons_edges = b.solve_edges().unwrap();
    let fan_out = b.count_fan_out(&cons_edges);
    let alloc = b.alloc_rings(&cons_edges, &fan_out).unwrap();
    let session_path = alloc
        .session
        .as_ref()
        .expect("mixed graph has a session")
        .path()
        .to_path_buf();
    assert!(alloc.ring_paths.contains_key(&(proc_sys.id, 0)));
    assert!(alloc.ring_paths.contains_key(&(slot.id, 0)));
    for path in alloc
        .ring_paths
        .values()
        .chain(alloc.host_input_paths.values())
    {
        assert!(
            path.starts_with(&session_path),
            "{} shares the session",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------------
// WiringManifest emission: the coordinator broadcasts the full target IR on
// its `wiring` channel at boot and re-fires it on a ReloadSequences request,
// sizing the ring from the concrete payload (the IR overruns the default cap).
// ---------------------------------------------------------------------------

use metor_proto::types::Msg as _;
use metor_proto_wkt::{ReloadSequences, WiringManifest};

/// Read every record currently readable on a coordinator message channel,
/// decoding each as a [`WiringManifest`]. The view must be claimed before the
/// run so it sits at the ring's live edge when the boot record lands.
#[cfg(not(miri))]
fn drain_wiring(view: &mut metor_fsw_ring::View<metor_fsw_ring::NoWake>) -> Vec<WiringManifest> {
    let mut out = Vec::new();
    let mut buf = Vec::new();
    while view.try_read_into(&mut buf).expect("readable wiring ring") {
        let (id, payload) = crate::message::split_record(&buf).expect("id-prefixed record");
        assert_eq!(id, WiringManifest::ID, "only WiringManifest on `wiring`");
        out.push(postcard::from_bytes(payload).expect("decode WiringManifest"));
    }
    out
}

#[cfg(not(miri))]
#[stellarator::test]
async fn wiring_manifest_emitted_at_boot() {
    let mut b = crate::coordinator::init::InitGraph::new(config());
    b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let json = r#"{"ir_version":1,"systems":["a","b"]}"#.to_string();
    b.set_wiring_manifest(WiringManifest {
        ir_version: 1,
        ir_json: json.clone(),
    });
    let mut coord = b.build().unwrap();
    let registry = coord.registry();
    let entry = registry
        .get(metor_proto::types::ComponentId::new("coordinator.wiring"))
        .expect("the wiring channel is registered");
    assert!(entry.telemetered(), "the manifest is recorded telemetry");
    let mut view = entry.view().expect("reader slot");

    coord.run_for(2).await;

    let seen = drain_wiring(&mut view);
    assert_eq!(seen.len(), 1, "one boot manifest");
    assert_eq!(seen[0].ir_version, 1);
    assert_eq!(seen[0].ir_json, json);
}

#[cfg(not(miri))]
#[stellarator::test]
async fn wiring_manifest_reemits_on_reload() {
    // A producer emits one ReloadSequences on its second cycle, edged into the
    // coordinator's reload fan-in; the manifest re-fires that cycle.
    let mut b = crate::coordinator::init::InitGraph::new(config());
    let prod = b.push_node(cyclic_node(
        ReloadProducer::NAME.into(),
        ReloadProducer { cycle: 0 },
    ));
    b.connect(
        PortRef {
            system: prod,
            port: PortId::Packet(ReloadSequences::ID),
        },
        PortRef {
            system: b.coordinator_handle(),
            port: PortId::Packet(ReloadSequences::ID),
        },
    );
    b.set_wiring_manifest(WiringManifest {
        ir_version: 1,
        ir_json: "{}".into(),
    });
    let mut coord = b.build().unwrap();
    let registry = coord.registry();
    let mut view = registry
        .get(metor_proto::types::ComponentId::new("coordinator.wiring"))
        .expect("wiring channel registered")
        .view()
        .expect("reader slot");

    coord.run_for(4).await;

    // Boot manifest plus one re-emit from the reload request.
    assert_eq!(drain_wiring(&mut view).len(), 2, "boot + reload re-emit");
}

/// Emits one [`ReloadSequences`] on its second cycle, then nothing.
struct ReloadProducer {
    cycle: u64,
}

#[derive(SystemOutput)]
struct ReloadOut {
    reload: MsgOut<ReloadSequences>,
}

impl System for ReloadProducer {
    type Input = NoIn;
    type Output = Out<ReloadOut>;
    const NAME: &'static str = "reload_producer";
}

impl CyclicSystem for ReloadProducer {
    fn execute(&mut self, _now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        self.cycle += 1;
        if self.cycle == 2 {
            let _ = o.reload.emit(&ReloadSequences {});
        }
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn wiring_manifest_emits_oversized_payload_intact() {
    // An IR larger than the default message cap sizes its own ring, so the boot
    // record fits and reads back byte-for-byte — proving the cap was not the
    // limit and nothing raised the global one.
    let mut b = crate::coordinator::init::InitGraph::new(config());
    b.push_node(cyclic_node(Producer::NAME.into(), Producer::new()));
    let big = "x".repeat(crate::message::MAX_MSG_BYTES + 4000);
    let json = format!("{{\"ir_version\":1,\"blob\":\"{big}\"}}");
    assert!(
        json.len() > crate::message::MAX_MSG_BYTES,
        "payload overruns the cap"
    );
    b.set_wiring_manifest(WiringManifest {
        ir_version: 1,
        ir_json: json.clone(),
    });
    let mut coord = b.build().unwrap();
    let mut view = coord
        .registry()
        .get(metor_proto::types::ComponentId::new("coordinator.wiring"))
        .expect("wiring channel registered")
        .view()
        .expect("reader slot");

    coord.run_for(2).await;

    let seen = drain_wiring(&mut view);
    assert_eq!(seen.len(), 1, "the oversized boot manifest was emitted");
    assert_eq!(
        seen[0].ir_json, json,
        "read back byte-for-byte, not truncated"
    );
}
