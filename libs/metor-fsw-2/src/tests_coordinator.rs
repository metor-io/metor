//! WP5 acceptance tests (coordinator.md "Tests"): the two-cyclic-system graph end
//! to end, the lapped hard-stop, an async system wired through a private copy-in
//! buffer, build-time wiring validation, and the init barrier. Systems are
//! registered and wired through the `Coordinator` builder — no hand-built ports.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use metor_fsw_ring::{BoxBacking, Notifier};
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    AsyncSystem, Coordinator, CoordinatorConfig, CyclicSystem, Input, Out, Output, PortRef,
    StopReason, System, SystemInput, SystemOutput, WireError,
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
    fn shutdown(&mut self, _o: &mut Self::Output) {}
}

impl CyclicSystem for Producer {
    fn execute(&mut self, _in: &mut NoIn, o: &mut Self::Output) {
        for _ in 0..self.burst {
            self.n += 1.0;
            let _ = o.imu.write(&Imu {
                timestamp: Timestamp(self.n as i64),
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
    fn shutdown(&mut self, _o: &mut Self::Output) {}
}

impl CyclicSystem for Consumer {
    fn execute(&mut self, input: &mut ConsIn, _o: &mut Self::Output) {
        if let Some(c) = &self.first_exec_init {
            // Record (once) how many inits had run before the first execute.
            if let Some(ic) = &self.init_counter {
                let _ = c.compare_exchange(0, ic.load(Relaxed), Relaxed, Relaxed);
            }
        }
        if !self.drain {
            return; // ignore the input so it eventually laps
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
    fn init(&mut self, _o: &mut Self::Output) {}
    fn shutdown(&mut self, _o: &mut Self::Output) {}
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
