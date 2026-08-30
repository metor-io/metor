//! Tests for the system layer. They cover the cyclic execution path, the
//! self-describing descriptor and its compatibility check, the standard
//! health counters, and the backpressure a slow reader applies to a writer.
//! Every port is built by hand on in-memory rings, with no coordinator
//! involved.

use core::mem::offset_of;
use std::collections::HashMap;

use metor_component::Decomponentize;
use metor_fsw_ring::{Config, NoWake, RingBuffer, WriteError};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::descriptor::compatible;
use crate::{
    CyclicRunner, CyclicSystem, Frame, FrameList, HealthPort, Input, LogEvent, MsgOut, Out, Output,
    PortDesc, System, SystemInput, SystemKind, SystemOutput, SystemStatus, buffer_capacity,
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

/// A byte ring for the implicit `LogEvent` message tail.
fn log_ring_for(readers: usize) -> RingBuffer {
    RingBuffer::create_in_memory(Config {
        capacity: crate::capacity_for(crate::MAX_MSG_BYTES, 16),
        max_readers: readers,
    })
}

fn write_until_block(mut write: impl FnMut(u32) -> Result<(), WriteError>) -> u32 {
    for count in 0.. {
        match write(count) {
            Ok(()) => {}
            Err(WriteError::WouldBlock) => return count,
            Err(error) => panic!("unexpected {error:?}"),
        }
    }
    unreachable!()
}

fn latest_health(input: &mut Input<SystemStatus>) -> RecSink {
    let record = input
        .latest()
        .expect("ring readable")
        .expect("health published");
    let mut sink = RecSink::default();
    record.apply(&mut sink).unwrap().unwrap();
    sink
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
            Ok(Some(imu)) => {
                let s = imu.get();
                (s.timestamp, s.omega * self.gain, s.accel)
            }
            _ => {
                output.health().error("imu_missing");
                return;
            }
        };
        // Produce a NavEstimate with a dynamic `residuals` trailer.
        let frame = NavEstimate {
            timestamp,
            angle,
            residuals: FrameList::EMPTY,
        };
        let _ = output.nav.write_with(&frame, |fw| {
            fw.list(&frame.residuals, offset_of!(NavEstimate, residuals), |l| {
                l.push(Residual { value: angle });
                l.push(Residual { value: accel });
            })
            .unwrap();
        });
    }
}

#[test]
fn cyclic_filter_end_to_end() {
    let imu_ring = ring_for::<Imu>(8, 2);
    let nav_ring = ring_for::<NavEstimate>(8, 2);
    let health_ring = ring_for::<SystemStatus>(8, 1);
    let log_ring = log_ring_for(1);

    // Upstream producer and downstream consumer, both built by hand.
    let mut imu_w = Output::<Imu>::new(imu_ring.writer(NoWake).unwrap());
    let mut nav_in = Input::<NavEstimate>::new(nav_ring.view(NoWake).unwrap());

    let input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake).unwrap()),
    };
    let health = HealthPort::new(
        Output::new(health_ring.writer(NoWake).unwrap()),
        MsgOut::<LogEvent>::new(log_ring.writer(NoWake).unwrap()),
    );
    let output = Out::new(
        FilterOut {
            nav: Output::new(nav_ring.writer(NoWake).unwrap()),
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
    let nav = nav_in
        .latest()
        .expect("ring readable")
        .expect("nav produced");
    let est = nav.get();
    assert_eq!(est.angle, 3.0, "omega * gain");
    assert_eq!(est.timestamp, Timestamp(42), "timestamp carried through");
    let residuals: Vec<Residual> =
        crate::port::frame_list_iter(nav.table(), offset_of!(NavEstimate, residuals)).collect();
    assert_eq!(residuals.len(), 2);
    assert_eq!(residuals[0].value, 3.0);
    assert_eq!(residuals[1].value, -0.5);
}

// ---------------------------------------------------------------------------
// Backpressure: an idle reader stalls the writer instead of being overwritten,
// and `latest()` frees older records while pinning the newest.
// ---------------------------------------------------------------------------

#[test]
fn idle_input_backpressures_writer_and_latest_frees() {
    let imu_ring = ring_for::<Imu>(2, 1);
    let mut input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake).unwrap()),
    };
    let mut w = Output::<Imu>::new(imu_ring.writer(NoWake).unwrap());
    let imu = |omega: f64| Imu {
        timestamp: Timestamp(0),
        omega,
        accel: 0.0,
    };

    // Fill until the idle view stalls the writer.
    let wrote = write_until_block(|i| w.write(&imu(i as f64)));
    assert!(wrote >= 2, "the depth-2 ring holds at least two records");

    // `latest()` serves the newest committed record, consuming the older ones...
    assert_eq!(
        input
            .imu
            .latest()
            .expect("ring readable")
            .expect("newest")
            .get()
            .omega,
        (wrote - 1) as f64
    );
    // ...which frees room for the writer, but only up to the pinned newest
    // record, which keeps its slot until the next read.
    let wrote2 = write_until_block(|i| w.write(&imu(100.0 + i as f64)));
    assert!(wrote2 >= 1, "the freed records admitted new writes");
    assert!(wrote2 < wrote, "the pinned record still holds its slot");
    assert_eq!(
        input
            .imu
            .latest()
            .expect("ring readable")
            .expect("follow the edge")
            .get()
            .omega,
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
        desc.inputs[0].id().component().expect("table port"),
        Imu::FRAME_ID
    );
    // The user's nav port plus the two implicit health and log ports.
    assert_eq!(desc.outputs.len(), 3);
    assert_eq!(
        desc.outputs[0].id().component().expect("table port"),
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
    let health_ring = ring_for::<SystemStatus>(8, 1);
    let log_ring = log_ring_for(1);

    let mut health_in = Input::<SystemStatus>::new(health_ring.view(NoWake).unwrap());

    let input = FilterIn {
        imu: Input::new(imu_ring.view(NoWake).unwrap()),
    };
    let health = HealthPort::new(
        Output::new(health_ring.writer(NoWake).unwrap()),
        MsgOut::<LogEvent>::new(log_ring.writer(NoWake).unwrap()),
    );
    let output = Out::new(
        FilterOut {
            nav: Output::new(nav_ring.writer(NoWake).unwrap()),
        },
        health,
    );

    let mut runner = CyclicRunner::new(Filter { gain: 1.0 }, input, output);
    // No IMU is ever published, so every execute bumps the "imu_missing" error.
    for _ in 0..3 {
        runner.step(Timestamp::now());
    }

    // Read the freshest health record and apply its vtable.
    let sink = latest_health(&mut health_in);

    assert_eq!(sink.values[&ComponentId::new("system_status.cycles")], 3.0);
    assert_eq!(sink.values[&ComponentId::new("system_status.errors")], 3.0);
    assert_eq!(
        sink.values[&ComponentId::new("system_status.error_counts.imu_missing")],
        3.0,
        "named domain counter lands via the dynamic-frame path"
    );
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
    fn next_output<WD>(&mut self) -> (RingBuffer, WD)
    where
        WD: metor_fsw_ring::WakeSource + Default + Clone + 'static,
    {
        (self.pop(), WD::default())
    }

    fn next_input<RD>(&mut self) -> (RingBuffer, RD)
    where
        RD: metor_fsw_ring::WakeSink + Default + Clone + 'static,
    {
        (self.pop(), RD::default())
    }

    fn next_input_fanin<RD>(&mut self) -> Vec<(RingBuffer, RD)>
    where
        RD: metor_fsw_ring::WakeSink + Default + Clone + 'static,
    {
        // A single producer, so one ring per message input.
        vec![(self.pop(), RD::default())]
    }
}

// ---------------------------------------------------------------------------
// An infallible publish onto an undersized ring counts the drop, and the
// runner folds it into a `publish_dropped` health error.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Chatter;

#[derive(SystemOutput)]
struct ChatterOut {
    imu: Output<Imu>,
}

impl System for Chatter {
    type Input = ();
    type Output = Out<ChatterOut>;
    const NAME: &'static str = "chatter";
}

impl CyclicSystem for Chatter {
    fn execute(&mut self, now: Timestamp, (): &mut (), output: &mut Self::Output) {
        // An `Imu` record can never fit the 16-byte ring below, so every
        // publish fails with `InsufficientCapacity` (a sizing bug) and is
        // counted rather than returned.
        output.imu.publish(&Imu {
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
    let health_ring = ring_for::<SystemStatus>(8, 1);
    let log_ring = log_ring_for(1);
    let mut health_in = Input::<SystemStatus>::new(health_ring.view(NoWake).unwrap());

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

    let sink = latest_health(&mut health_in);
    assert_eq!(sink.values[&ComponentId::new("system_status.errors")], 1.0);
    assert_eq!(
        sink.values[&ComponentId::new("system_status.error_counts.publish_dropped")],
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
    let ins = <GuardIn as SystemInput>::decls().ports;
    assert_eq!(ins.len(), 1);
    assert_eq!(
        ins[0].delivery,
        crate::Delivery::Log,
        "axes keep the MsgIn defaults"
    );
    assert_eq!(ins[0].fan_in, crate::FanIn::Many);

    let outs = <QuietOut as SystemOutput>::decls().ports;
    assert_eq!(outs.len(), 2);
    assert!(
        !outs[0].telemetered,
        "#[fsw(telemetered = false)] on a FRAME output"
    );
    assert_eq!(
        outs[0].id(),
        crate::PortId::Component(NavEstimate::FRAME_ID)
    );
    assert!(
        !outs[1].telemetered,
        "the CommandOut token is the same opt-out"
    );
    assert_eq!(
        outs[1].id(),
        crate::PortId::Packet(<SequenceCommand as metor_proto::types::Msg>::ID)
    );
}

/// A full message ring makes the emitter see `WouldBlock`, every accepted
/// record arrives in order, and draining frees space for the next emit.
#[test]
fn msg_log_never_loses_records() {
    let ring = msg_ring(1);
    let mut inbox: crate::MsgIn<SequenceCommand> = crate::MsgIn::new(ring.view(NoWake).unwrap());
    let mut w: crate::MsgOut<SequenceCommand> = crate::MsgOut::new(ring.writer(NoWake).unwrap());

    // Fill until the idle inbox stalls the emitter.
    let sent = write_until_block(|i| w.emit(&cmd(&format!("c{i}"))));
    assert!(sent >= 1);

    // Every accepted record arrives in order; nothing was overwritten.
    let mut got = Vec::new();
    inbox.drain(|c| got.push(c.channel)).unwrap();
    let expected: Vec<String> = (0..sent).map(|i| format!("c{i}")).collect();
    assert_eq!(got, expected, "the log lost or reordered records");

    // The drain freed space, so the emitter proceeds.
    w.emit(&cmd("after")).unwrap();
    let mut got = 0;
    inbox.drain(|_| got += 1).unwrap();
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
            input.cmds.drain(|_| seen.set(seen.get() + 1)).unwrap();
        }
    }

    let ring = msg_ring(1);
    let health_ring = ring_for::<SystemStatus>(8, 1);
    let log_ring = log_ring_for(1);

    let input: GuardIn = crate::BindPorts::bind(&mut TestSource {
        rings: vec![ring.clone()],
        next: 0,
    });
    let output = Out::new(
        NothingOut {},
        HealthPort::new(
            Output::new(health_ring.writer(NoWake).unwrap()),
            MsgOut::<LogEvent>::new(log_ring.writer(NoWake).unwrap()),
        ),
    );
    let mut w: crate::MsgOut<SequenceCommand> = crate::MsgOut::new(ring.writer(NoWake).unwrap());
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
