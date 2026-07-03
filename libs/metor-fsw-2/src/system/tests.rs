//! System acceptance tests (system.md "Tests"): the cyclic & async system paths, the
//! self-descriptor + compatibility check, the standard health counters, and the
//! cyclic lapped-input semantics. Ports are built by hand, without a coordinator.

use core::mem::offset_of;
use std::collections::HashMap;

use metor_fsw::Decomponentize;
use metor_fsw_ring::{BoxBacking, Config, NoWake, Notifier, Overrun, RingBuffer};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    AsyncSystem, CyclicRunner, CyclicSystem, Frame, FrameList, HealthPort, Input, Out, Output,
    PortDesc, SystemHealth, SystemInput, SystemKind, SystemLog, SystemOutput, System,
    buffer_capacity, compatible,
};

// ---------------------------------------------------------------------------
// Frames under test: an `Imu` input, a `NavEstimate` output with a dynamic member.
// ---------------------------------------------------------------------------

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
    accel: f64,
}

#[derive(crate::AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
struct Residual {
    value: f64,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "nav")]
struct NavEstimate {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    angle: f64,
    residuals: FrameList<Residual, 4>,
}

/// Records every scalar component it sees as `f64` — for reading a port via its vtable.
#[derive(Default)]
struct RecSink {
    values: HashMap<ComponentId, f64>,
}

impl Decomponentize for RecSink {
    type Error = core::convert::Infallible;
    fn apply_value(
        &mut self,
        id: ComponentId,
        value: ComponentView<'_>,
        _t: Option<Timestamp>,
    ) -> Result<(), Self::Error> {
        self.values.insert(id, value.to_f64());
        Ok(())
    }
}

fn overwrite_ring<F: crate::Frame>(depth: usize, readers: usize) -> RingBuffer<BoxBacking> {
    RingBuffer::create_in_memory(Config {
        capacity: buffer_capacity::<F>(depth),
        max_readers: readers,
        overrun: Overrun::Overwrite,
    })
}

// ---------------------------------------------------------------------------
// A sample cyclic system: a unit-gain filter consuming `Imu`, producing `NavEstimate`.
// ---------------------------------------------------------------------------

struct Filter {
    gain: f64,
}

#[derive(SystemInput)]
struct FilterIn {
    imu: Input<Imu>,
}

#[derive(SystemOutput)]
struct FilterOut {
    nav: Output<NavEstimate>,
}

impl System for Filter {
    type Input = FilterIn;
    type Output = Out<FilterOut>;
    const NAME: &'static str = "filter";

    fn init(&mut self, output: &mut Out<FilterOut>) {
        // Publish an initial (default) estimate before the first execute.
        let _ = output.nav.write(&NavEstimate {
            timestamp: Timestamp(0),
            angle: 0.0,
            residuals: FrameList::EMPTY,
        });
    }
}

impl CyclicSystem for Filter {
    // Carries the input's timestamp through (not `now`), so the test can assert the
    // sample stamp survives the cycle.
    fn execute(&mut self, _now: Timestamp, input: &mut FilterIn, output: &mut Out<FilterOut>) {
        // Read the freshest IMU sample; report a health error when starved.
        let (timestamp, angle, accel) = match input.imu.latest() {
            Some(imu) => {
                let s = imu.get();
                (s.timestamp, s.omega * self.gain, s.accel)
            }
            _ => {
                output.health().error("imu_missing");
                return;
            }
        };
        // Produce a NavEstimate with a dynamic `residuals` trailer.
        let _ = output.nav.write_with(
            &NavEstimate {
                timestamp,
                angle,
                residuals: FrameList::EMPTY,
            },
            |fw| {
                fw.list(offset_of!(NavEstimate, residuals), |l| {
                    l.push(Residual { value: angle });
                    l.push(Residual { value: accel });
                });
            },
        );
    }
}

#[test]
fn cyclic_filter_end_to_end() {
    let imu_ring = overwrite_ring::<Imu>(8, 2);
    let nav_ring = overwrite_ring::<NavEstimate>(8, 2);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    // Upstream producer + downstream consumer, both built by hand.
    let mut imu_w = Output::<Imu>::new(imu_ring.writer(NoWake, NoWake).unwrap());
    let mut nav_in = Input::<NavEstimate>::new(nav_ring.view(NoWake, NoWake).unwrap());

    let input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake, NoWake).unwrap()),
    };
    let health = HealthPort::new(
        Output::new(health_ring.writer(NoWake, NoWake).unwrap()),
        Output::new(log_ring.writer(NoWake, NoWake).unwrap()),
    );
    let output = Out::new(
        FilterOut {
            nav: Output::new(nav_ring.writer(NoWake, NoWake).unwrap()),
        },
        health,
    );

    let mut runner = CyclicRunner::new(Filter { gain: 2.0 }, input, output);
    runner.init();

    imu_w
        .write(&Imu {
            timestamp: Timestamp(42),
            omega: 1.5,
            accel: -0.5,
        })
        .unwrap();
    runner.step(Timestamp::now());

    // The consumer reads the produced frame: fixed region zero-copy + dynamic member.
    let nav = nav_in.latest().expect("nav produced");
    let est = nav.get();
    assert_eq!(est.angle, 3.0, "omega * gain");
    assert_eq!(est.timestamp, Timestamp(42), "timestamp carried through");
    let residuals = nav.list::<Residual>(offset_of!(NavEstimate, residuals));
    assert_eq!(residuals.len(), 2);
    assert_eq!(residuals.get(0).unwrap().value, 3.0);
    assert_eq!(residuals.get(1).unwrap().value, -0.5);
}

// ---------------------------------------------------------------------------
// is_lapped: a lapped cyclic input is observable (the stop policy lives in the coordinator).
// ---------------------------------------------------------------------------

#[test]
fn cyclic_input_lap_is_observable() {
    let imu_ring = overwrite_ring::<Imu>(2, 1);
    let input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake, NoWake).unwrap()),
    };
    let mut w = imu_ring.writer(NoWake, NoWake).unwrap();

    assert!(!input.imu.lap_fault());
    assert!(!input.any_lapped());

    // Overrun the small buffer far past capacity without the view advancing.
    for i in 0..32 {
        w.try_write(
            Imu {
                timestamp: Timestamp(i),
                omega: 0.0,
                accel: 0.0,
            }
            .as_bytes(),
        )
        .unwrap();
    }

    assert!(input.imu.lap_fault(), "writer lapped the idle view");
    assert!(input.any_lapped(), "bundle surfaces the lapped port");
}

// ---------------------------------------------------------------------------
// SystemDescriptor + compatibility (subset / ty-shape).
// ---------------------------------------------------------------------------

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuSubset {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64, // a strict subset of Imu's {omega, accel}
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuWrongTy {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f32, // same ids as Imu, different ty (no padding: two f32s fill 8 bytes)
    accel: f32,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuExtra {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
    accel: f64,
    extra: f64, // a field the producer never emits
}

#[test]
fn descriptor_and_compatibility() {
    let desc = <Filter as CyclicSystem>::descriptor();
    assert_eq!(desc.name, "filter");
    assert_eq!(desc.kind, SystemKind::Cyclic);
    assert_eq!(desc.inputs.len(), 1);
    assert_eq!(desc.inputs[0].id.component().expect("table port"), Imu::FRAME_ID);
    // user nav port + the two implicit health/log ports.
    assert_eq!(desc.outputs.len(), 3);
    assert_eq!(desc.outputs[0].id.component().expect("table port"), NavEstimate::FRAME_ID);

    let producer = PortDesc::of::<Imu>();
    // A matching subset consumer is compatible.
    assert!(compatible(&producer, &PortDesc::of::<ImuSubset>()));
    // Exact match is compatible.
    assert!(compatible(&producer, &PortDesc::of::<Imu>()));
    // A ty mismatch on a shared component fails.
    assert!(!compatible(&producer, &PortDesc::of::<ImuWrongTy>()));
    // A consumer requiring a field the producer lacks fails.
    assert!(!compatible(&producer, &PortDesc::of::<ImuExtra>()));
    // A different frame id fails.
    assert!(!compatible(&producer, &PortDesc::of::<NavEstimate>()));
}

// ---------------------------------------------------------------------------
// Health: standard counters + a named error counter land on the health port.
// ---------------------------------------------------------------------------

#[test]
fn health_counters_published() {
    let imu_ring = overwrite_ring::<Imu>(8, 1);
    let nav_ring = overwrite_ring::<NavEstimate>(8, 1);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    let mut health_in = Input::<SystemHealth>::new(health_ring.view(NoWake, NoWake).unwrap());

    let input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake, NoWake).unwrap()),
    };
    let health = HealthPort::new(
        Output::new(health_ring.writer(NoWake, NoWake).unwrap()),
        Output::new(log_ring.writer(NoWake, NoWake).unwrap()),
    );
    let output = Out::new(
        FilterOut {
            nav: Output::new(nav_ring.writer(NoWake, NoWake).unwrap()),
        },
        health,
    );

    let mut runner = CyclicRunner::new(Filter { gain: 1.0 }, input, output);
    // No IMU is ever published, so every execute bumps the "imu_missing" error.
    for _ in 0..3 {
        runner.step(Timestamp::now());
    }

    // Read the freshest health record and apply its vtable.
    let record = health_in.latest().expect("health published");
    let mut sink = RecSink::default();
    record.apply(&mut sink).unwrap().unwrap();

    assert_eq!(sink.values[&ComponentId::new("health.cycles")], 3.0);
    assert_eq!(sink.values[&ComponentId::new("health.errors")], 3.0);
    assert_eq!(sink.values[&ComponentId::new("health.lapped_inputs")], 0.0);
    assert_eq!(
        sink.values[&ComponentId::new("health.error_counts.imu_missing")],
        3.0,
        "named domain counter lands via the dynamic-frame path"
    );
}

// ---------------------------------------------------------------------------
// A sample async system: awaits one IMU via the ring `Notifier`, produces a nav.
// ---------------------------------------------------------------------------

struct AsyncFilter;

#[derive(SystemInput)]
struct AsyncIn {
    imu: Input<Imu, BoxBacking, Notifier, Notifier>,
}

#[derive(SystemOutput)]
struct AsyncOut {
    nav: Output<NavEstimate, BoxBacking, Notifier, Notifier>,
}

impl System for AsyncFilter {
    type Input = AsyncIn;
    type Output = Out<AsyncOut, BoxBacking, Notifier, Notifier>;
    const NAME: &'static str = "async_filter";
}

impl AsyncSystem for AsyncFilter {
    async fn run(&mut self, input: &mut Self::Input, output: &mut Self::Output) {
        // One end-to-end cycle: await the next IMU, then publish a nav estimate.
        let nav = {
            let Ok(imu) = input.imu.recv().await else {
                return;
            };
            let s = imu.get();
            NavEstimate {
                timestamp: s.timestamp,
                angle: s.omega,
                residuals: FrameList::EMPTY,
            }
        };
        let _ = output.nav.write_async(&nav).await;
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn async_filter_one_cycle() {
    let imu_ring = overwrite_ring::<Imu>(8, 2);
    let nav_ring = overwrite_ring::<NavEstimate>(8, 2);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    let imu_data = Notifier::default();
    let imu_space = Notifier::default();
    let nav_data = Notifier::default();
    let nav_space = Notifier::default();

    let mut input = AsyncIn {
        imu: Input::new(
            imu_ring
                .view(imu_data.clone(), imu_space.clone())
                .unwrap(),
        ),
    };
    let mut nav_in = Input::<NavEstimate>::new(nav_ring.view(NoWake, NoWake).unwrap());
    let health = HealthPort::new(
        Output::new(health_ring.writer(Notifier::default(), Notifier::default()).unwrap()),
        Output::new(log_ring.writer(Notifier::default(), Notifier::default()).unwrap()),
    );
    let mut output = Out::new(
        AsyncOut {
            nav: Output::new(nav_ring.writer(nav_data.clone(), nav_space.clone()).unwrap()),
        },
        health,
    );

    // Feed one IMU sample from a spawned task; the system's `run` awaits it.
    let writer = {
        let imu_ring = imu_ring.clone();
        let imu_data = imu_data.clone();
        let imu_space = imu_space.clone();
        stellarator::spawn(async move {
            let mut w = imu_ring.writer(imu_data, imu_space).unwrap();
            w.write(
                Imu {
                    timestamp: Timestamp(7),
                    omega: 2.0,
                    accel: 0.0,
                }
                .as_bytes(),
            )
            .await
            .unwrap();
        })
    };

    let mut sys = AsyncFilter;
    sys.run(&mut input, &mut output).await;
    let _ = writer.await;

    let nav = nav_in.latest().expect("async system produced a nav");
    assert_eq!(nav.get().angle, 2.0);
    assert_eq!(nav.get().timestamp, Timestamp(7));
}

// ---------------------------------------------------------------------------
// #[system] parity: the hand-written trait impls and the attribute macro produce
// an identical descriptor and identical run behavior (design-system-macro.md §11.2).
// ---------------------------------------------------------------------------

/// The hand-written form: bundles + System + CyclicSystem + BuildSystem spelled out.
struct HandDoubler {
    gain: f64,
}

#[derive(SystemInput)]
struct HandDoublerIn {
    imu: Input<Imu>,
}

#[derive(SystemOutput)]
struct HandDoublerOut {
    nav: Output<NavEstimate>,
}

impl System for HandDoubler {
    type Input = HandDoublerIn;
    type Output = Out<HandDoublerOut>;
    const NAME: &'static str = "doubler";
}

impl CyclicSystem for HandDoubler {
    fn execute(&mut self, now: Timestamp, input: &mut HandDoublerIn, output: &mut Self::Output) {
        match input.imu.latest() {
            Some(imu) => {
                let angle = imu.get().omega * self.gain;
                output.nav.publish(&NavEstimate {
                    timestamp: now,
                    angle,
                    residuals: FrameList::EMPTY,
                });
            }
            None => output.health().error("imu_missing"),
        }
    }
}

impl crate::BuildSystem for HandDoubler {
    type Params = f64;
    fn new(gain: f64) -> Self {
        Self { gain }
    }
}

/// The macro form: same name, same ports in the same order, same body — everything
/// else generated.
struct MacroDoubler {
    gain: f64,
}

#[crate::system(name = "doubler")]
impl MacroDoubler {
    fn new(gain: f64) -> Self {
        Self { gain }
    }

    fn execute(
        &mut self,
        now: Timestamp,
        imu: &mut Input<Imu>,
        nav: &mut Output<NavEstimate>,
        health: &mut HealthPort,
    ) {
        match imu.latest() {
            Some(imu) => {
                let angle = imu.get().omega * self.gain;
                nav.publish(&NavEstimate {
                    timestamp: now,
                    angle,
                    residuals: FrameList::EMPTY,
                });
            }
            None => health.error("imu_missing"),
        }
    }
}

/// A tiny 3-cycle harness: feed `samples` (None ⇒ starve the cycle), return every
/// produced angle and the final health record's scalar components.
fn run_doubler<S, O>(system: S, samples: &[Option<f64>]) -> (Vec<f64>, HashMap<ComponentId, f64>)
where
    S: CyclicSystem<Output = Out<O>>,
    S::Input: crate::BindPorts<BoxBacking>,
    O: SystemOutput + crate::BindPorts<BoxBacking>,
{
    let imu_ring = overwrite_ring::<Imu>(8, 2);
    let nav_ring = overwrite_ring::<NavEstimate>(8, 2);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    let mut imu_w = Output::<Imu>::new(imu_ring.writer(NoWake, NoWake).unwrap());
    let mut nav_in = Input::<NavEstimate>::new(nav_ring.view(NoWake, NoWake).unwrap());
    let mut health_in = Input::<SystemHealth>::new(health_ring.view(NoWake, NoWake).unwrap());

    let input = crate::BindPorts::bind(&mut TestSource {
        rings: vec![imu_ring.clone()],
        next: 0,
    });
    let output = crate::BindPorts::bind(&mut TestSource {
        rings: vec![nav_ring.clone(), health_ring.clone(), log_ring.clone()],
        next: 0,
    });

    let mut runner = CyclicRunner::new(system, input, output);
    let mut angles = Vec::new();
    for (i, s) in samples.iter().enumerate() {
        if let Some(omega) = s {
            imu_w
                .write(&Imu {
                    timestamp: Timestamp(i as i64),
                    omega: *omega,
                    accel: 0.0,
                })
                .unwrap();
        }
        runner.step(Timestamp(i as i64));
        if let Some(nav) = nav_in.latest() {
            angles.push(nav.get().angle);
        }
    }

    let record = health_in.latest().expect("health published");
    let mut sink = RecSink::default();
    record.apply(&mut sink).unwrap().unwrap();
    (angles, sink.values)
}

/// A positional `RingSource` over pre-created rings (the coordinator's job, by hand).
struct TestSource {
    rings: Vec<RingBuffer<BoxBacking>>,
    next: usize,
}

impl TestSource {
    fn pop(&mut self) -> RingBuffer<BoxBacking> {
        let ring = self.rings[self.next].clone();
        self.next += 1;
        ring
    }
}

impl crate::RingSource for TestSource {
    type B = BoxBacking;

    fn next_output<WD, WS>(&mut self) -> (RingBuffer<BoxBacking>, WD, WS)
    where
        WD: metor_fsw_ring::WakeSource + Default + Clone + 'static,
        WS: metor_fsw_ring::WakeSink + Default + Clone + 'static,
    {
        (self.pop(), WD::default(), WS::default())
    }

    fn next_input<RD, RS>(&mut self) -> (RingBuffer<BoxBacking>, RD, RS)
    where
        RD: metor_fsw_ring::WakeSink + Default + Clone + 'static,
        RS: metor_fsw_ring::WakeSource + Default + Clone + 'static,
    {
        (self.pop(), RD::default(), RS::default())
    }

    fn next_input_fanin<RD, RS>(&mut self) -> Vec<(RingBuffer<BoxBacking>, RD, RS)>
    where
        RD: metor_fsw_ring::WakeSink + Default + Clone + 'static,
        RS: metor_fsw_ring::WakeSource + Default + Clone + 'static,
    {
        // Single-producer fan-in: pop one ring per message input.
        vec![(self.pop(), RD::default(), RS::default())]
    }
}

#[test]
fn system_macro_matches_hand_written() {
    // 1. Identical descriptors (name, kind, port order/ids/sizes — via the stable
    //    Debug rendering, PortDesc carries a non-Eq announce closure).
    let hand = <HandDoubler as CyclicSystem>::descriptor();
    let mac = <MacroDoubler as CyclicSystem>::descriptor();
    assert_eq!(format!("{hand:?}"), format!("{mac:?}"));

    // 2. Identical BuildSystem params + construction.
    let h: HandDoubler = crate::BuildSystem::new(2.0);
    let m: MacroDoubler = crate::BuildSystem::new(2.0);

    // 3. Identical behavior over 3 cycles, including the starved first cycle's
    //    health error (later cycles re-serve the freshest read record, so only a
    //    never-fed input counts as missing).
    let samples = [None, Some(1.5), Some(-2.0)];
    let (ha, mut hh) = run_doubler(h, &samples);
    let (ma, mut mh) = run_doubler(m, &samples);
    assert_eq!(ha, ma, "same outputs");
    assert_eq!(ha, vec![3.0, -4.0], "gain applied once samples flow");
    // The execute duration is wall time — the one legitimately different component.
    hh.remove(&ComponentId::new("health.last_execute_micros"));
    mh.remove(&ComponentId::new("health.last_execute_micros"));
    assert_eq!(hh, mh, "same health counters");
    assert_eq!(hh[&ComponentId::new("health.cycles")], 3.0);
    assert_eq!(hh[&ComponentId::new("health.error_counts.imu_missing")], 1.0);
}

// ---------------------------------------------------------------------------
// E6: an infallible publish onto an undersized ring counts the drop, and the
// runner folds it into a `publish_dropped` health error.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Chatter;

#[crate::system(name = "chatter")]
impl Chatter {
    fn execute(&mut self, now: Timestamp, imu: &mut Output<Imu>) {
        // An `Imu` record can never fit the 16-byte ring below: the publish fails
        // with `InsufficientCapacity` (a sizing bug) and is counted, not returned.
        imu.publish(&Imu {
            timestamp: now,
            omega: 1.0,
            accel: 0.0,
        });
    }
}

#[test]
fn publish_drop_folds_to_health() {
    // 16 bytes cannot hold one Imu record (24-byte frame + record header).
    let tiny = RingBuffer::create_in_memory(Config {
        capacity: 16,
        max_readers: 1,
        overrun: Overrun::Overwrite,
    });
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);
    let mut health_in = Input::<SystemHealth>::new(health_ring.view(NoWake, NoWake).unwrap());

    let input = crate::BindPorts::bind(&mut TestSource { rings: vec![], next: 0 });
    let output = crate::BindPorts::bind(&mut TestSource {
        rings: vec![tiny, health_ring.clone(), log_ring.clone()],
        next: 0,
    });
    let mut runner = CyclicRunner::new(Chatter::default(), input, output);
    runner.step(Timestamp(1));

    let record = health_in.latest().expect("health published");
    let mut sink = RecSink::default();
    record.apply(&mut sink).unwrap().unwrap();
    assert_eq!(sink.values[&ComponentId::new("health.errors")], 1.0);
    assert_eq!(
        sink.values[&ComponentId::new("health.error_counts.publish_dropped")],
        1.0,
        "the port's counted drop is telemetered by the runner (E6)"
    );
    assert!(matches!(runner.state(), crate::SlotState::Running), "a drop is an error, not a stop");
}

// ---------------------------------------------------------------------------
// E3: a lap *during* execute is resync-latched by `latest()` (the body keeps the
// freshest record) and charged post-execute: health + a permanent stop.
// ---------------------------------------------------------------------------

/// Writes into its own input ring mid-execute (standing in for an async producer
/// racing the drain), then re-reads: `latest()` must resync over the lap and serve
/// the newest record while latching `is_lapped` for the runner. The `Option` writer
/// keeps the (macro-required) `Default` construction path compiling; the test
/// installs the real writer before running.
#[derive(Default)]
struct SelfFlooder {
    writer: Option<Output<Imu>>,
    seen: Vec<f64>,
}

#[crate::system(name = "self_flooder")]
impl SelfFlooder {
    fn execute(&mut self, now: Timestamp, imu: &mut Input<Imu>) {
        let _ = imu.latest();
        // Lap the (depth-2) view: far more records than the ring holds.
        let writer = self.writer.as_mut().expect("test installs the writer");
        for i in 0..32 {
            let _ = writer.write(&Imu {
                timestamp: now,
                omega: i as f64,
                accel: 0.0,
            });
        }
        // The E3 contract: no Err — the lap is latched and the view resyncs to the
        // live edge (unread records, including the flood, are abandoned).
        assert!(imu.latest().is_none(), "nothing read before the lap to re-serve");
        assert!(imu.lap_fault(), "the resynced-over lap stays latched");
        // Reads resume normally after the resync: a fresh record is served.
        let _ = writer.write(&Imu {
            timestamp: now,
            omega: 99.0,
            accel: 0.0,
        });
        let newest = imu.latest().expect("post-resync reads resume");
        self.seen.push(newest.get().omega);
    }
}

#[test]
fn mid_execute_lap_latches_and_stops() {
    let imu_ring = overwrite_ring::<Imu>(2, 1);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);
    let mut health_in = Input::<SystemHealth>::new(health_ring.view(NoWake, NoWake).unwrap());

    let input = crate::BindPorts::bind(&mut TestSource {
        rings: vec![imu_ring.clone()],
        next: 0,
    });
    let output = crate::BindPorts::bind(&mut TestSource {
        rings: vec![health_ring.clone(), log_ring.clone()],
        next: 0,
    });
    let system = SelfFlooder {
        writer: Some(Output::new(imu_ring.writer(NoWake, NoWake).unwrap())),
        seen: Vec::new(),
    };
    let mut runner = CyclicRunner::new(system, input, output);
    runner.step(Timestamp(1));

    assert!(
        matches!(runner.state(), crate::SlotState::Stopped { reason: crate::StopReason::LappedInput }),
        "a mid-execute lap permanently stops the system the same cycle"
    );
    let record = health_in.latest().expect("final health published");
    let mut sink = RecSink::default();
    record.apply(&mut sink).unwrap().unwrap();
    assert_eq!(
        sink.values[&ComponentId::new("health.lapped_inputs")],
        1.0,
        "the latched lap is charged to health (E3)"
    );

    // A stopped slot never executes again.
    runner.step(Timestamp(2));
    let record = health_in.latest().expect("health record");
    let mut sink = RecSink::default();
    record.apply(&mut sink).unwrap().unwrap();
    assert_eq!(sink.values[&ComponentId::new("health.cycles")], 1.0);
}

// ---------------------------------------------------------------------------
// #[fsw(...)] attribute lowering + the Log × Stop fourth cell (A5/A6, C5).
// ---------------------------------------------------------------------------

use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};

/// A command log that must never lose a record — the Log × Stop fourth cell the
/// unification made expressible (`docs/design-port-unification.md` §2.3).
#[derive(crate::SystemInput)]
struct GuardIn {
    #[fsw(on_lap = "stop")]
    cmds: crate::MsgIn<SequenceCommand>,
}

#[derive(crate::SystemOutput)]
struct QuietOut {
    /// A frame output opted out of the downlink (A6 — frames get the opt-out too).
    #[fsw(telemetered = false)]
    nav: Output<NavEstimate>,
    /// The `CommandOut` token: pure `MsgOut` sugar the derive lowers to
    /// `.untelemetered()` (the alias itself carries no flag).
    cmds: crate::CommandOut<SequenceCommand>,
}

/// A tiny Log-sized message ring (small enough to lap with a handful of records).
fn msg_ring(readers: usize) -> RingBuffer<BoxBacking> {
    RingBuffer::create_in_memory(Config {
        capacity: crate::capacity_for(64, 2),
        max_readers: readers,
        overrun: Overrun::Overwrite,
    })
}

fn cmd(channel: &str) -> SequenceCommand {
    SequenceCommand {
        channel: channel.to_string(),
        command: SequenceCommandKind::Start,
    }
}

/// `#[fsw(...)]` overrides land on the generated descriptors: `on_lap = "stop"`
/// flips exactly the lap axis; `telemetered = false` and the `CommandOut` token
/// flip exactly the telemetry flag.
#[test]
fn fsw_attrs_lower_onto_descriptors() {
    let ins = <GuardIn as SystemInput>::port_descs();
    assert_eq!(ins.len(), 1);
    assert_eq!(ins[0].on_lap, crate::OnLap::Stop, "the attribute override");
    assert_eq!(ins[0].delivery, crate::Delivery::Log, "other axes keep the MsgIn defaults");
    assert_eq!(ins[0].fan_in, crate::FanIn::Many);

    let outs = <QuietOut as SystemOutput>::port_descs();
    assert_eq!(outs.len(), 2);
    assert!(!outs[0].telemetered, "#[fsw(telemetered = false)] on a FRAME output");
    assert_eq!(outs[0].id, crate::PortId::Component(NavEstimate::FRAME_ID));
    assert!(!outs[1].telemetered, "the CommandOut token is the same opt-out");
    assert_eq!(
        outs[1].id,
        crate::PortId::Packet(<SequenceCommand as metor_proto::types::Msg>::ID)
    );
}

/// The `on_lap` attribute reaches the BOUND port too (descriptor and runtime can
/// never disagree): a lapped Stop input reports `lap_fault`, while the default
/// Resync policy keeps flowing and reports none.
#[test]
fn fsw_on_lap_governs_the_bound_port() {
    let ring = msg_ring(2);
    // Bind through the derive walk (the fan-in cursor), so the chained
    // `.with_on_lap(Stop)` in the generated `bind` is what's under test.
    let input: GuardIn = crate::BindPorts::bind(&mut TestSource {
        rings: vec![ring.clone()],
        next: 0,
    });
    let mut stop_in = input.cmds;
    // A hand-built twin on the same ring with the default (Resync) policy.
    let mut resync_in: crate::MsgIn<SequenceCommand> =
        crate::MsgIn::new(ring.view(NoWake, NoWake).unwrap());

    let mut w: crate::MsgOut<SequenceCommand> =
        crate::MsgOut::new(ring.writer(NoWake, NoWake).unwrap());
    for i in 0..32 {
        w.emit(&cmd(&format!("c{i}"))).unwrap();
    }

    // Live lap observation, before any drain (the runner's pre-execute gate).
    assert!(stop_in.lap_fault(), "Stop policy: a live lap is a fault");
    assert!(!resync_in.lap_fault(), "Resync policy: laps are not faults");

    // Stop: the drain stops at the lap (no records) and the fault latches.
    let mut got = 0;
    stop_in.drain(|_| got += 1);
    assert_eq!(got, 0, "a Stop port does not read past a lap");
    assert!(stop_in.lap_fault());

    // Resync: the drain skips to the live edge (abandoning the flood, best-effort)
    // and the port keeps flowing — records emitted after the lap arrive normally.
    let mut got = 0;
    resync_in.drain(|_| got += 1);
    assert_eq!(got, 0, "resync abandons unread records up to the live edge");
    assert!(!resync_in.lap_fault());
    w.emit(&cmd("after")).unwrap();
    let mut got = 0;
    resync_in.drain(|_| got += 1);
    assert_eq!(got, 1, "a Resync port keeps flowing after the lap");
    assert!(!resync_in.lap_fault());
}

/// The fourth cell end-to-end: a cyclic consumer of a Log × Stop command input is
/// permanently hard-stopped by the runner when the channel laps — the same doctrine
/// as a lapped frame input, now policy-derived instead of frame-hard-coded.
#[test]
fn log_stop_input_hard_stops_cyclic_consumer() {
    #[derive(crate::SystemOutput)]
    struct NothingOut {}

    struct Guard {
        seen: usize,
    }
    impl System for Guard {
        type Input = GuardIn;
        type Output = Out<NothingOut>;
        const NAME: &'static str = "guard";
    }
    impl CyclicSystem for Guard {
        fn execute(&mut self, _now: Timestamp, input: &mut GuardIn, _o: &mut Self::Output) {
            input.cmds.drain(|_| self.seen += 1);
        }
    }

    let ring = msg_ring(1);
    let health_ring = overwrite_ring::<SystemHealth>(8, 1);
    let log_ring = overwrite_ring::<SystemLog>(8, 1);

    let input: GuardIn = crate::BindPorts::bind(&mut TestSource {
        rings: vec![ring.clone()],
        next: 0,
    });
    let output = Out::new(
        NothingOut {},
        HealthPort::new(
            Output::new(health_ring.writer(NoWake, NoWake).unwrap()),
            Output::new(log_ring.writer(NoWake, NoWake).unwrap()),
        ),
    );
    let mut w: crate::MsgOut<SequenceCommand> =
        crate::MsgOut::new(ring.writer(NoWake, NoWake).unwrap());
    let mut runner = CyclicRunner::new(Guard { seen: 0 }, input, output);

    // A normal cycle: one command in, one command drained.
    w.emit(&cmd("mode")).unwrap();
    runner.step(Timestamp(1));
    assert!(matches!(runner.state(), crate::SlotState::Running));

    // Flood far past the ring depth: the idle view is lapped.
    for i in 0..32 {
        w.emit(&cmd(&format!("c{i}"))).unwrap();
    }
    runner.step(Timestamp(2));
    assert!(
        matches!(runner.state(), crate::SlotState::Stopped { reason: crate::StopReason::LappedInput }),
        "a lapped Stop command log permanently stops the consumer"
    );
}
