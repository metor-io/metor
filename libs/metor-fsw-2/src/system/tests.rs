//! Tests for the system layer. They cover the cyclic and async execution
//! paths, the self-describing descriptor and its compatibility check, the
//! standard health counters, and the backpressure a slow reader applies to a
//! writer. Every port is built by hand on in-memory rings, with no
//! coordinator involved.

use core::mem::offset_of;
use std::collections::HashMap;

use metor_fsw::Decomponentize;
use metor_fsw_ring::{Config, NoWake, Notifier, RingBuffer, WriteError};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::descriptor::compatible;
use crate::{
    AsyncSystem, CyclicRunner, CyclicSystem, Frame, FrameList, HealthPort, Input, Out, Output,
    PortDesc, System, SystemHealth, SystemInput, SystemKind, SystemLog, SystemOutput,
    buffer_capacity,
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

/// A [`Decomponentize`] sink that collects every scalar component a record's
/// vtable reports, as `f64`.
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

fn ring_for<F: crate::Frame>(depth: usize, readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: buffer_capacity::<F>(depth),
        max_readers: readers,
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
        // Publish an initial default estimate before the first execute.
        let _ = output.nav.write(&NavEstimate {
            timestamp: Timestamp(0),
            angle: 0.0,
            residuals: FrameList::EMPTY,
        });
    }
}

impl CyclicSystem for Filter {
    // Carries the input's timestamp through rather than `now`, so the test can
    // assert the sample stamp survives the cycle.
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
    let imu_ring = ring_for::<Imu>(8, 2);
    let nav_ring = ring_for::<NavEstimate>(8, 2);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = ring_for::<SystemLog>(8, 1);

    // Upstream producer and downstream consumer, both built by hand.
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

    // The consumer reads the produced frame, both the fixed region and the
    // dynamic member.
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
// Backpressure: an idle reader stalls the writer instead of being overwritten,
// and `latest()` frees older records while pinning the newest.
// ---------------------------------------------------------------------------

#[test]
fn idle_input_backpressures_writer_and_latest_frees() {
    let imu_ring = ring_for::<Imu>(2, 1);
    let mut input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake, NoWake).unwrap()),
    };
    let mut w = Output::<Imu>::new(imu_ring.writer(NoWake, NoWake).unwrap());
    let imu = |omega: f64| Imu {
        timestamp: Timestamp(0),
        omega,
        accel: 0.0,
    };

    // Fill until the idle view stalls the writer.
    let mut wrote = 0u32;
    loop {
        match w.write(&imu(wrote as f64)) {
            Ok(()) => wrote += 1,
            Err(WriteError::WouldBlock) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert!(wrote >= 2, "the depth-2 ring holds at least two records");

    // `latest()` serves the newest committed record, consuming the older ones...
    assert_eq!(
        input.imu.latest().expect("newest").get().omega,
        (wrote - 1) as f64
    );
    // ...which frees room for the writer, but only up to the pinned newest
    // record, which keeps its slot until the next read.
    let mut wrote2 = 0u32;
    loop {
        match w.write(&imu(100.0 + wrote2 as f64)) {
            Ok(()) => wrote2 += 1,
            Err(WriteError::WouldBlock) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert!(wrote2 >= 1, "the freed records admitted new writes");
    assert!(wrote2 < wrote, "the pinned record still holds its slot");
    assert_eq!(
        input.imu.latest().expect("follow the edge").get().omega,
        100.0 + (wrote2 - 1) as f64
    );
}

// ---------------------------------------------------------------------------
// SystemDescriptor + compatibility (subset / ty-shape).
// ---------------------------------------------------------------------------

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuSubset {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64, // a strict subset of Imu's {omega, accel}
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct ImuWrongTy {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f32, // same ids as Imu, different ty (no padding: two f32s fill 8 bytes)
    accel: f32,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
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
    assert_eq!(
        desc.inputs[0].id.component().expect("table port"),
        Imu::FRAME_ID
    );
    // The user's nav port plus the two implicit health and log ports.
    assert_eq!(desc.outputs.len(), 3);
    assert_eq!(
        desc.outputs[0].id.component().expect("table port"),
        NavEstimate::FRAME_ID
    );

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
    let imu_ring = ring_for::<Imu>(8, 1);
    let nav_ring = ring_for::<NavEstimate>(8, 1);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = ring_for::<SystemLog>(8, 1);

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
    imu: Input<Imu, Notifier, Notifier>,
}

#[derive(SystemOutput)]
struct AsyncOut {
    nav: Output<NavEstimate, Notifier, Notifier>,
}

impl System for AsyncFilter {
    type Input = AsyncIn;
    type Output = Out<AsyncOut, Notifier, Notifier>;
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
    let imu_ring = ring_for::<Imu>(8, 2);
    let nav_ring = ring_for::<NavEstimate>(8, 2);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = ring_for::<SystemLog>(8, 1);

    let imu_data = Notifier::default();
    let imu_space = Notifier::default();
    let nav_data = Notifier::default();
    let nav_space = Notifier::default();

    let mut input = AsyncIn {
        imu: Input::new(imu_ring.view(imu_data.clone(), imu_space.clone()).unwrap()),
    };
    let mut nav_in = Input::<NavEstimate>::new(nav_ring.view(NoWake, NoWake).unwrap());
    let health = HealthPort::new(
        Output::new(
            health_ring
                .writer(Notifier::default(), Notifier::default())
                .unwrap(),
        ),
        Output::new(
            log_ring
                .writer(Notifier::default(), Notifier::default())
                .unwrap(),
        ),
    );
    let mut output = Out::new(
        AsyncOut {
            nav: Output::new(
                nav_ring
                    .writer(nav_data.clone(), nav_space.clone())
                    .unwrap(),
            ),
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
// #[system] parity: the hand-written trait impls and the attribute macro
// produce an identical descriptor and identical run behavior.
// ---------------------------------------------------------------------------

/// A cyclic system that scales IMU omega into a nav angle, with the port
/// bundles and every trait impl spelled out by hand.
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

/// The same system as [`HandDoubler`], declared through the `#[system]`
/// attribute. Same name, same ports in the same order, same body; everything
/// else is generated.
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

/// Runs `system` for one cycle per sample (`None` starves the cycle) and
/// returns every produced angle plus the final health record's scalar
/// components.
fn run_doubler<S, O>(system: S, samples: &[Option<f64>]) -> (Vec<f64>, HashMap<ComponentId, f64>)
where
    S: CyclicSystem<Output = Out<O>>,
    S::Input: crate::BindPorts,
    O: SystemOutput + crate::BindPorts,
{
    let imu_ring = ring_for::<Imu>(8, 2);
    let nav_ring = ring_for::<NavEstimate>(8, 2);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = ring_for::<SystemLog>(8, 1);

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

/// A [`RingSource`](crate::RingSource) that hands out pre-created rings in
/// order, standing in for a coordinator.
struct TestSource {
    rings: Vec<RingBuffer>,
    next: usize,
}

impl TestSource {
    fn pop(&mut self) -> RingBuffer {
        let ring = self.rings[self.next].clone();
        self.next += 1;
        ring
    }
}

impl crate::RingSource for TestSource {
    fn next_output<WD, WS>(&mut self) -> (RingBuffer, WD, WS)
    where
        WD: metor_fsw_ring::WakeSource + Default + Clone + 'static,
        WS: metor_fsw_ring::WakeSink + Default + Clone + 'static,
    {
        (self.pop(), WD::default(), WS::default())
    }

    fn next_input<RD, RS>(&mut self) -> (RingBuffer, RD, RS)
    where
        RD: metor_fsw_ring::WakeSink + Default + Clone + 'static,
        RS: metor_fsw_ring::WakeSource + Default + Clone + 'static,
    {
        (self.pop(), RD::default(), RS::default())
    }

    fn next_input_fanin<RD, RS>(&mut self) -> Vec<(RingBuffer, RD, RS)>
    where
        RD: metor_fsw_ring::WakeSink + Default + Clone + 'static,
        RS: metor_fsw_ring::WakeSource + Default + Clone + 'static,
    {
        // A single producer, so one ring per message input.
        vec![(self.pop(), RD::default(), RS::default())]
    }
}

#[test]
fn system_macro_matches_hand_written() {
    // 1. Identical descriptors: name, kind, port order, ids, and sizes. They
    //    are compared through the Debug rendering because PortDesc carries a
    //    non-Eq announce closure.
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
    // The execute duration is wall time, the one legitimately different component.
    hh.remove(&ComponentId::new("health.last_execute_micros"));
    mh.remove(&ComponentId::new("health.last_execute_micros"));
    assert_eq!(hh, mh, "same health counters");
    assert_eq!(hh[&ComponentId::new("health.cycles")], 3.0);
    assert_eq!(
        hh[&ComponentId::new("health.error_counts.imu_missing")],
        1.0
    );
}

// ---------------------------------------------------------------------------
// An infallible publish onto an undersized ring counts the drop, and the
// runner folds it into a `publish_dropped` health error.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Chatter;

#[crate::system(name = "chatter")]
impl Chatter {
    fn execute(&mut self, now: Timestamp, imu: &mut Output<Imu>) {
        // An `Imu` record can never fit the 16-byte ring below, so every
        // publish fails with `InsufficientCapacity` (a sizing bug) and is
        // counted rather than returned.
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
    });
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = ring_for::<SystemLog>(8, 1);
    let mut health_in = Input::<SystemHealth>::new(health_ring.view(NoWake, NoWake).unwrap());

    let input = crate::BindPorts::bind(&mut TestSource {
        rings: vec![],
        next: 0,
    });
    let output = crate::BindPorts::bind(&mut TestSource {
        rings: vec![tiny, health_ring.clone(), log_ring.clone()],
        next: 0,
    });
    let mut runner = CyclicRunner::new(Chatter, input, output);
    runner.step(Timestamp(1));

    let record = health_in.latest().expect("health published");
    let mut sink = RecSink::default();
    record.apply(&mut sink).unwrap().unwrap();
    assert_eq!(sink.values[&ComponentId::new("health.errors")], 1.0);
    assert_eq!(
        sink.values[&ComponentId::new("health.error_counts.publish_dropped")],
        1.0,
        "the port's counted drop lands as a runner health error"
    );
    assert!(
        matches!(runner.state(), crate::SlotState::Running),
        "a drop is an error, not a stop"
    );
}

// ---------------------------------------------------------------------------
// #[fsw(...)] attribute lowering + message-log delivery guarantees.
// ---------------------------------------------------------------------------

use metor_proto_wkt::{SequenceCommand, SequenceCommandKind};

/// An input bundle whose one port is a [`MsgIn`](crate::MsgIn) command log.
/// A full ring holds the emitter back instead of dropping records.
#[derive(crate::SystemInput)]
struct GuardIn {
    cmds: crate::MsgIn<SequenceCommand>,
}

#[derive(crate::SystemOutput)]
struct QuietOut {
    /// A frame output opted out of the downlink.
    #[fsw(telemetered = false)]
    nav: Output<NavEstimate>,
    /// [`CommandOut`](crate::CommandOut) is [`MsgOut`](crate::MsgOut) sugar
    /// that the derive lowers to the same telemetry opt-out; the alias itself
    /// carries no flag.
    cmds: crate::CommandOut<SequenceCommand>,
}

/// A message ring small enough to fill with a handful of records.
fn msg_ring(readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: crate::capacity_for(64, 2),
        max_readers: readers,
    })
}

fn cmd(channel: &str) -> SequenceCommand {
    SequenceCommand {
        channel: channel.to_string(),
        command: SequenceCommandKind::Start,
    }
}

/// `#[fsw(telemetered = false)]` and the `CommandOut` token flip exactly the
/// telemetry flag on the generated descriptors; the other axes keep their
/// defaults.
#[test]
fn fsw_attrs_lower_onto_descriptors() {
    let ins = <GuardIn as SystemInput>::port_descs();
    assert_eq!(ins.len(), 1);
    assert_eq!(
        ins[0].delivery,
        crate::Delivery::Log,
        "axes keep the MsgIn defaults"
    );
    assert_eq!(ins[0].fan_in, crate::FanIn::Many);

    let outs = <QuietOut as SystemOutput>::port_descs();
    assert_eq!(outs.len(), 2);
    assert!(
        !outs[0].telemetered,
        "#[fsw(telemetered = false)] on a FRAME output"
    );
    assert_eq!(outs[0].id, crate::PortId::Component(NavEstimate::FRAME_ID));
    assert!(
        !outs[1].telemetered,
        "the CommandOut token is the same opt-out"
    );
    assert_eq!(
        outs[1].id,
        crate::PortId::Packet(<SequenceCommand as metor_proto::types::Msg>::ID)
    );
}

/// A full message ring makes the emitter see `WouldBlock`, every accepted
/// record arrives in order, and draining frees space for the next emit.
#[test]
fn msg_log_never_loses_records() {
    let ring = msg_ring(1);
    let mut inbox: crate::MsgIn<SequenceCommand> =
        crate::MsgIn::new(ring.view(NoWake, NoWake).unwrap());
    let mut w: crate::MsgOut<SequenceCommand> =
        crate::MsgOut::new(ring.writer(NoWake, NoWake).unwrap());

    // Fill until the idle inbox stalls the emitter.
    let mut sent = 0u32;
    loop {
        match w.emit(&cmd(&format!("c{sent}"))) {
            Ok(()) => sent += 1,
            Err(WriteError::WouldBlock) => break,
            Err(e) => panic!("unexpected {e:?}"),
        }
    }
    assert!(sent >= 1);

    // Every accepted record arrives in order; nothing was overwritten.
    let mut got = Vec::new();
    inbox.drain(|c| got.push(c.channel));
    let expected: Vec<String> = (0..sent).map(|i| format!("c{i}")).collect();
    assert_eq!(got, expected, "the log lost or reordered records");

    // The drain freed space, so the emitter proceeds.
    w.emit(&cmd("after")).unwrap();
    let mut got = 0;
    inbox.drain(|_| got += 1);
    assert_eq!(got, 1);
}

/// A cyclic consumer of a command log sees every emitted record across
/// cycles. A full ring holds the emitter back rather than dropping, and the
/// consumer stays `Running`.
#[test]
fn log_input_guaranteed_delivery_through_runner() {
    #[derive(crate::SystemOutput)]
    struct NothingOut {}

    #[derive(Default)]
    struct Guard {
        seen: std::rc::Rc<core::cell::Cell<usize>>,
    }
    impl System for Guard {
        type Input = GuardIn;
        type Output = Out<NothingOut>;
        const NAME: &'static str = "guard";
    }
    impl CyclicSystem for Guard {
        fn execute(&mut self, _now: Timestamp, input: &mut GuardIn, _o: &mut Self::Output) {
            let seen = self.seen.clone();
            input.cmds.drain(|_| seen.set(seen.get() + 1));
        }
    }

    let ring = msg_ring(1);
    let health_ring = ring_for::<SystemHealth>(8, 1);
    let log_ring = ring_for::<SystemLog>(8, 1);

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
    let seen = std::rc::Rc::new(core::cell::Cell::new(0));
    let mut runner = CyclicRunner::new(Guard { seen: seen.clone() }, input, output);

    // Emit across cycles, pausing whenever the ring is full; every record is
    // eventually delivered and the consumer never stops.
    let mut sent = 0;
    while sent < 32 {
        while sent < 32 && w.emit(&cmd(&format!("c{sent}"))).is_ok() {
            sent += 1;
        }
        runner.step(Timestamp(sent as i64));
        assert!(matches!(runner.state(), crate::SlotState::Running));
    }
    runner.step(Timestamp(33));
    assert_eq!(seen.get(), 32, "every emitted command was delivered");
}
