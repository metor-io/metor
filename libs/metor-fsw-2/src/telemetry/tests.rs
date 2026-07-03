//! Telemetry acceptance tests (telemetry.md "Tests"): the general output registry queried
//! by instance-qualified id, the telemetry downlink end-to-end against a deterministic
//! in-memory mock transport (announced prefixed vtables/metadata + `Table` packets),
//! two-instance prefix disambiguation, subset filtering, and the non-blocking
//! drop-on-full policy with its `telemetry.dropped` health counter.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use metor_proto::types::{ComponentId, LenPacket, PacketId, Timestamp};
use metor_proto_wkt::{ComponentMetadata, VTableMsg};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    ClockMode, Coordinator, CoordinatorConfig, CyclicSystem, Input, Out, Output, PortRef, System,
    SystemHealth, SystemInput, SystemOutput, TelemetryConfig, TelemetryMode, Transport,
    TransportError,
};

// ---------------------------------------------------------------------------
// Frames + systems under test (a producer -> consumer chain).
// ---------------------------------------------------------------------------

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "imu")]
struct Imu {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    omega: f64,
}

#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default)]
#[repr(C)]
#[metor_fsw(name = "nav")]
struct Nav {
    #[metor_fsw(timestamp)]
    timestamp: Timestamp,
    angle: f64,
}

#[derive(SystemInput)]
struct NoIn {}

/// A cyclic producer writing an incrementing `Imu` (omega = cycle index).
struct Producer {
    n: f64,
}

#[derive(SystemOutput)]
struct ProdOut {
    imu: Output<Imu>,
}

impl System for Producer {
    type Input = NoIn;
    type Output = Out<ProdOut>;
    const NAME: &'static str = "producer";
}

impl CyclicSystem for Producer {
    fn execute(&mut self, now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        self.n += 1.0;
        let _ = o.imu.write(&Imu {
            timestamp: now,
            omega: self.n,
        });
    }
}

/// A cyclic consumer: `nav.angle = imu.omega`.
struct Consumer;

#[derive(SystemInput)]
struct ConsIn {
    imu: Input<Imu>,
}

#[derive(SystemOutput)]
struct ConsOut {
    nav: Output<Nav>,
}

impl System for Consumer {
    type Input = ConsIn;
    type Output = Out<ConsOut>;
    const NAME: &'static str = "consumer";
}

impl CyclicSystem for Consumer {
    fn execute(&mut self, now: Timestamp, input: &mut ConsIn, o: &mut Self::Output) {
        if let Some(imu) = input.imu.latest() {
            let _ = o.nav.write(&Nav {
                timestamp: now,
                angle: imu.get().omega,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// A deterministic in-memory mock transport (telemetry.md §7) for the e2e tests.
// ---------------------------------------------------------------------------

struct MockInner {
    announces: Mutex<Vec<(PacketId, Vec<ComponentMetadata>)>>,
    packets: Mutex<Vec<LenPacket>>,
    /// When set, `send` never completes — saturates the queue for the drop test.
    block: bool,
}

#[derive(Clone)]
struct MockTransport {
    inner: Arc<MockInner>,
}

impl MockTransport {
    fn new(block: bool) -> Self {
        Self {
            inner: Arc::new(MockInner {
                announces: Mutex::new(Vec::new()),
                packets: Mutex::new(Vec::new()),
                block,
            }),
        }
    }
}

impl Transport for MockTransport {
    async fn announce(
        &mut self,
        msg: &VTableMsg,
        meta: &[ComponentMetadata],
    ) -> Result<(), TransportError> {
        self.inner
            .announces
            .lock()
            .unwrap()
            .push((msg.id, meta.to_vec()));
        Ok(())
    }

    async fn send(&mut self, pkt: LenPacket) -> Result<(), TransportError> {
        if self.inner.block {
            // Never resolves: the sender parks here, the cycle keeps running, slots fill.
            std::future::pending::<()>().await;
        }
        self.inner.packets.lock().unwrap().push(pkt);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// A free-running simulated clock: `run_for` yields each cycle, so the spawned sender
/// task is scheduled deterministically.
fn sim_config() -> CoordinatorConfig {
    CoordinatorConfig {
        cycle_rate: 1000.0,
        default_depth: 8,
        clock: ClockMode::Simulated {
            dt: Duration::from_millis(1),
        },
        ..CoordinatorConfig::default()
    }
}

/// A `Table` packet's announced id is the 2 bytes after the 4-byte length + 1 ty byte.
fn packet_id_of(pkt: &LenPacket) -> PacketId {
    [pkt.inner[5], pkt.inner[6]]
}

/// A `Table` packet's payload (the table bytes) begins after the 8-byte packet header.
fn packet_payload(pkt: &LenPacket) -> &[u8] {
    &pkt.inner[8..]
}

/// Drain a view to its newest record and return a copy of the bytes, if any.
fn drain_latest(view: &mut metor_fsw_ring::View<metor_fsw_ring::BoxBacking, metor_fsw_ring::NoWake, metor_fsw_ring::NoWake>) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut last = None;
    while let Ok(true) = view.try_read_into(&mut buf) {
        last = Some(buf.clone());
    }
    last
}

// ---------------------------------------------------------------------------
// 1. Registry: query by instance-qualified id, claim a view, read the bytes.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn registry_query_view_and_read() {
    let mut b = Coordinator::builder(sim_config());
    let p = b.add_cyclic_named("producer", Producer { n: 0.0 });
    let c = b.add_cyclic_named("consumer", Consumer);
    b.connect(PortRef::new::<Imu>(p), PortRef::new::<Imu>(c))
        .unwrap();
    let mut coord = b.build().unwrap();

    // The registry indexes every output by `ComponentId::new("<instance>.<frame>")`.
    let registry = coord.registry();
    let entry = registry
        .get(ComponentId::new("producer.imu"))
        .expect("producer.imu in the registry");
    let crate::EntrySchema::Table { frame_id, .. } = &entry.schema else {
        panic!("frame entry carries a Table schema");
    };
    assert_eq!(*frame_id, ComponentId::new("imu"));
    assert_eq!(&*entry.instance, "producer");
    // The consumer's frame and the coordinator's own buffers are indexed too.
    assert!(registry.get(ComponentId::new("consumer.nav")).is_some());
    assert!(registry.get(ComponentId::new("coordinator.health")).is_some());

    // Claim a view *before* running (a fresh view only sees later commits), run, read.
    let mut view = entry.view().expect("reader slot");
    coord.run_for(5).await;
    let bytes = drain_latest(&mut view).expect("producer wrote at least one imu");
    let imu = Imu::read_from_prefix(&bytes).expect("imu bytes").0;
    assert_eq!(imu.omega, 5.0, "producer increments omega each cycle");
}

// ---------------------------------------------------------------------------
// 2. End-to-end (all mode): announced prefixed schema + per-cycle Table packets.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn telemetry_end_to_end_all() {
    let mock = MockTransport::new(false);
    let rec = mock.inner.clone();

    let mut b = Coordinator::builder(sim_config());
    let p = b.add_cyclic_named("producer", Producer { n: 0.0 });
    let c = b.add_cyclic_named("consumer", Consumer);
    b.connect(PortRef::new::<Imu>(p), PortRef::new::<Imu>(c))
        .unwrap();
    b.add_telemetry(TelemetryConfig {
        transport: mock,
        mode: TelemetryMode::All,
    });
    let mut coord = b.build().unwrap();
    // Only Table entries are announced (Postcard entries — the coordinator's
    // `sequences`/`commands` channels — are self-describing, no announce).
    let n_taps = coord
        .registry()
        .entries()
        .iter()
        .filter(|e| matches!(e.schema, crate::EntrySchema::Table { .. }))
        .count();
    coord.run_for(30).await;

    let announces = rec.announces.lock().unwrap();

    // Every output is announced exactly once — the user frames, every system's implicit
    // health/log, and the coordinator-owned buffers (telemetry.md §3/§5). The `log`
    // frames carry only a dynamic `FrameList`, so their announce has an empty metadata
    // set (the count proves they were announced); the scalar-bearing frames carry their
    // prefixed component names.
    assert_eq!(announces.len(), n_taps, "one announce per registry entry");
    let has_name = |name: &str| {
        announces
            .iter()
            .any(|(_, meta)| meta.iter().any(|m| m.name == name))
    };
    let has_prefix = |prefix: &str| {
        announces
            .iter()
            .any(|(_, meta)| meta.iter().any(|m| m.name.starts_with(prefix)))
    };
    assert!(has_name("producer.imu.omega"), "producer.imu announced");
    assert!(has_name("consumer.nav.angle"), "consumer.nav announced");
    assert!(has_prefix("producer.health"), "producer.health announced");
    assert!(has_prefix("consumer.health"), "consumer.health announced");
    assert!(has_prefix("coordinator.health"), "coordinator buffers announced");

    // The producer.imu tap's `Table` packets carry the committed frame bytes verbatim.
    let imu_pid = announces
        .iter()
        .find(|(_, meta)| meta.iter().any(|m| m.name == "producer.imu.omega"))
        .map(|(id, _)| *id)
        .expect("producer.imu announced with an id");
    drop(announces);

    let packets = rec.packets.lock().unwrap();
    let imu_pkts: Vec<&LenPacket> = packets
        .iter()
        .filter(|pkt| packet_id_of(pkt) == imu_pid)
        .collect();
    assert!(!imu_pkts.is_empty(), "received Table packets for producer.imu");
    let last = imu_pkts.last().unwrap();
    let imu = Imu::read_from_prefix(packet_payload(last))
        .expect("packet payload is the imu table")
        .0;
    assert!(imu.omega > 0.0, "streamed a real omega: {}", imu.omega);
}

// ---------------------------------------------------------------------------
// 3. Two instances of one type → distinct qualified ids + distinct prefixed names.
// ---------------------------------------------------------------------------

#[test]
fn two_instances_distinct_prefixes() {
    let mut b = Coordinator::builder(sim_config());
    // Two producers of the same type; neither has inputs, so the graph builds.
    b.add_cyclic_named("imu_left", Producer { n: 0.0 });
    b.add_cyclic_named("imu_right", Producer { n: 0.0 });
    let coord = b.build().unwrap();
    let registry = coord.registry();

    let left = registry
        .get(ComponentId::new("imu_left.imu"))
        .expect("imu_left.imu");
    let right = registry
        .get(ComponentId::new("imu_right.imu"))
        .expect("imu_right.imu");

    // Same unprefixed frame id, distinct qualified keys — the headline collision, now
    // disambiguated; their announced names carry distinct instance prefixes.
    let crate::EntrySchema::Table {
        frame_id: left_id,
        metadata: left_meta,
        ..
    } = &left.schema
    else {
        panic!("frame entry carries a Table schema");
    };
    let crate::EntrySchema::Table {
        frame_id: right_id,
        metadata: right_meta,
        ..
    } = &right.schema
    else {
        panic!("frame entry carries a Table schema");
    };
    assert_eq!(left_id, right_id);
    assert_ne!(left.key, right.key);
    assert!(left_meta.iter().all(|m| m.name.starts_with("imu_left.")));
    assert!(right_meta.iter().all(|m| m.name.starts_with("imu_right.")));
    assert!(left_meta.iter().any(|m| m.name == "imu_left.imu.omega"));
    assert!(right_meta.iter().any(|m| m.name == "imu_right.imu.omega"));
}

// ---------------------------------------------------------------------------
// 4. Subset mode taps only the configured instances/frames.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn subset_mode_filters() {
    let mock = MockTransport::new(false);
    let rec = mock.inner.clone();

    let mut b = Coordinator::builder(sim_config());
    let p = b.add_cyclic_named("producer", Producer { n: 0.0 });
    let c = b.add_cyclic_named("consumer", Consumer);
    b.connect(PortRef::new::<Imu>(p), PortRef::new::<Imu>(c))
        .unwrap();
    b.add_telemetry(TelemetryConfig {
        transport: mock,
        mode: TelemetryMode::Subset {
            instances: vec!["producer".to_string()],
            frames: Vec::new(),
        },
    });
    let mut coord = b.build().unwrap();
    coord.run_for(20).await;

    let announces = rec.announces.lock().unwrap();
    assert!(!announces.is_empty(), "producer was tapped");
    // Only producer.* was tapped — no consumer, coordinator, or telemetry frames leaked.
    for (_, meta) in announces.iter() {
        for m in meta {
            assert!(
                m.name.starts_with("producer."),
                "subset leaked a non-producer tap: {}",
                m.name
            );
        }
    }
    assert!(
        announces
            .iter()
            .any(|(_, meta)| meta.iter().any(|m| m.name == "producer.imu.omega")),
        "producer.imu still tapped"
    );
}

// ---------------------------------------------------------------------------
// 5. Drop policy: a saturated transport never blocks the cycle; drops are surfaced.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn drop_policy_never_blocks_and_counts() {
    // A transport whose `send` never completes: after the first take, slots stay full
    // and every later snapshot overwrites an un-sent one (a drop).
    let mock = MockTransport::new(true);

    let mut b = Coordinator::builder(sim_config());
    b.add_cyclic_named("producer", Producer { n: 0.0 });
    b.add_telemetry(TelemetryConfig {
        transport: mock,
        mode: TelemetryMode::All,
    });
    let mut coord = b.build().unwrap();

    // Tap the telemetry system's own health frame to observe its error counter.
    let registry = coord.registry();
    let mut health = registry
        .get(ComponentId::new("telemetry.health"))
        .expect("telemetry.health")
        .view()
        .expect("reader slot");

    // The cycle never blocks on the stalled link: `run_for` completes all cycles.
    coord.run_for(50).await;

    let bytes = drain_latest(&mut health).expect("telemetry published a health frame");
    let h = SystemHealth::read_from_prefix(&bytes)
        .expect("health bytes")
        .0;
    assert!(
        h.errors > 0,
        "saturated transport should have surfaced telemetry.dropped (errors={})",
        h.errors
    );
}

// ---------------------------------------------------------------------------
// 6. Message downlink: every record forwarded as a self-describing `Msg` packet, in
//    FIFO order, never coalesced, no announce — and the message FIFO and the component
//    hand-off share the one sender independently (`docs/messages.md` §3).
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn message_downlink_fifo_no_coalesce() {
    use std::sync::atomic::{AtomicBool, Ordering::Release};

    use metor_fsw_ring::{Config, NoWake, Overrun, RingBuffer};
    use metor_proto::types::{Msg, OwnedPacket};
    use metor_proto_wkt::{
        SequenceChannelSpec, SequenceCommand, SequenceCommandKind, SequenceRegistry,
    };
    use stellarator::sync::WaitQueue;

    use crate::message::{LOG_DEPTH, MAX_MSG_BYTES, MsgOut, split_record};
    use crate::port::capacity_for;
    use crate::registry::{EntrySchema, RegistryEntry};

    use super::{HandOff, run_sender};

    // A by-hand message channel — the exact shape the coordinator registers (W4) and the
    // telemetry `execute` message-tap drains.
    let ring = RingBuffer::create_in_memory(Config {
        capacity: capacity_for(MAX_MSG_BYTES, LOG_DEPTH),
        max_readers: 4,
        overrun: Overrun::Overwrite,
    });
    let entry = RegistryEntry {
        key: ComponentId::new("mode.events"),
        instance: Arc::from("mode"),
        name: Arc::from("events"),
        schema: EntrySchema::Postcard,
        delivery: crate::Delivery::Log,
        telemetered: true,
        ring: ring.clone(),
    };
    // Claim the tap before any write (an overwrite-ring view starts at the live edge).
    let mut view = entry.view().expect("reader slot");
    // Typed ports — a heterogeneous channel is N ports now (`docs/message-wiring.md` §2.1);
    // the ring enforces a single live writer, so the ports take it in turn (each drop
    // frees the claim) and the tap drains one interleaved stream.
    // An event/command *log* of three records — two of one Msg type, one of another,
    // interleaved. A snapshot would coalesce the two `SequenceCommand`s; a log must not.
    {
        let mut cmd_out: MsgOut<SequenceCommand> =
            MsgOut::new(ring.writer(NoWake, NoWake).expect("first writer"));
        cmd_out
            .emit(&SequenceCommand {
                channel: "mode".to_string(),
                command: SequenceCommandKind::Start,
            })
            .expect("emit start");
    }
    {
        let mut reg_out: MsgOut<SequenceRegistry> =
            MsgOut::new(ring.writer(NoWake, NoWake).expect("claim freed on drop"));
        reg_out
            .emit(&SequenceRegistry {
                channels: vec![SequenceChannelSpec {
                    name: "mode".to_string(),
                    available: vec!["commissioning".to_string()],
                }],
            })
            .expect("emit registry");
    }
    let mut cmd_out: MsgOut<SequenceCommand> =
        MsgOut::new(ring.writer(NoWake, NoWake).expect("claim freed on drop"));
    cmd_out
        .emit(&SequenceCommand {
            channel: "mode".to_string(),
            command: SequenceCommandKind::Abort,
        })
        .expect("emit abort");

    // The one two-lane hand-off (one sender drains both lanes).
    let wq = Arc::new(WaitQueue::new());
    let handoff = Arc::new(HandOff::new(1, wq.clone()));

    // A component snapshot through the latest-wins hand-off, to prove the two queues are
    // independent and the one sender forwards both.
    let mut table = LenPacket::table([9, 9], 4);
    table.extend_from_slice(&[1, 2, 3, 4]);
    handoff.push_snapshot(0, table);

    // Drain *every* message record (the `execute` message-tap logic) into the FIFO.
    let mut scratch = Vec::new();
    while let Ok(true) = view.try_read_into(&mut scratch) {
        let (id, payload) = split_record(&scratch).expect("split record");
        let mut pkt = LenPacket::msg(id, payload.len());
        pkt.extend_from_slice(payload);
        handoff.push_log(pkt);
    }

    // Drive the async sender: no announces (no component taps), forward each queued packet.
    let mock = MockTransport::new(false);
    let rec = mock.inner.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = stellarator::spawn(run_sender(
        mock,
        Vec::new(),
        handoff,
        wq.clone(),
        stop.clone(),
    ));

    // Let the sender drain (it sends synchronously, then parks), then stop + join.
    for _ in 0..32 {
        if rec.packets.lock().unwrap().len() >= 4 {
            break;
        }
        stellarator::yield_now().await;
    }
    stop.store(true, Release);
    wq.wake_all();
    let _ = handle.await;

    // No message is announced (self-describing); only the component tap would announce,
    // and we passed none.
    assert!(
        rec.announces.lock().unwrap().is_empty(),
        "messages carry no announce"
    );

    let packets = rec.packets.lock().unwrap();
    // A `LenPacket`'s bytes carry a 4-byte length prefix the wire reader strips before
    // `OwnedPacket::parse` sees the packet header; skip it here too.
    let parsed: Vec<OwnedPacket<Vec<u8>>> = packets
        .iter()
        .map(|p| OwnedPacket::parse(p.inner[4..].to_vec()).expect("parse packet"))
        .collect();

    // The component snapshot came through as a `Table`, proving the shared sender drains
    // both hand-offs.
    assert_eq!(
        parsed
            .iter()
            .filter(|p| matches!(p, OwnedPacket::Table(_)))
            .count(),
        1,
        "the component snapshot was forwarded too"
    );

    // The three message records arrived as `Msg` packets, in FIFO order, none coalesced —
    // both `SequenceCommand`s are present despite sharing an id (a snapshot would lose one).
    let msgs: Vec<_> = parsed
        .iter()
        .filter_map(|p| match p {
            OwnedPacket::Msg(m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(msgs.len(), 3, "every message record forwarded, none coalesced");
    assert_eq!(
        msgs.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![
            SequenceCommand::ID,
            SequenceRegistry::ID,
            SequenceCommand::ID
        ],
        "FIFO order preserved"
    );
    let first: SequenceCommand = msgs[0].parse().expect("parse start");
    assert!(matches!(first.command, SequenceCommandKind::Start));
    let third: SequenceCommand = msgs[2].parse().expect("parse abort");
    assert_eq!(third.channel, "mode");
    assert!(matches!(third.command, SequenceCommandKind::Abort));
}

// ---------------------------------------------------------------------------
// 6b. The generic Table × Log combination (the axis product's fourth cell): an
//     every-record FRAME log downlinks each record via the FIFO lane, framed as an
//     announced Table packet — no coalescing, one announce.
// ---------------------------------------------------------------------------

/// A producer whose `Imu` output declares `Delivery::Log` — an every-record frame
/// log. Writes THREE records per cycle; a Snapshot tap would coalesce them.
struct BurstLogProducer {
    n: f64,
}

struct BurstLogOut {
    imu: Output<Imu>,
}

impl SystemOutput for BurstLogOut {
    fn decls() -> Vec<crate::PortDecl> {
        vec![crate::PortDecl::Port(
            crate::PortDesc::of::<Imu>().with_delivery(crate::Delivery::Log),
        )]
    }
}

impl crate::BindPorts<crate::ring::BoxBacking> for BurstLogOut {
    fn bind<S: crate::RingSource<B = crate::ring::BoxBacking>>(src: &mut S) -> Self {
        Self {
            imu: Output::bind(src),
        }
    }
}

impl System for BurstLogProducer {
    type Input = NoIn;
    type Output = Out<BurstLogOut>;
    const NAME: &'static str = "burst_log";
}

impl CyclicSystem for BurstLogProducer {
    fn execute(&mut self, now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        for _ in 0..3 {
            self.n += 1.0;
            let _ = o.imu.write(&Imu {
                timestamp: now,
                omega: self.n,
            });
        }
    }
}

#[cfg(not(miri))]
#[stellarator::test]
async fn table_log_entry_downlinks_every_record() {
    let mock = MockTransport::new(false);
    let rec = mock.inner.clone();

    let mut b = Coordinator::builder(sim_config());
    b.add_cyclic(BurstLogProducer { n: 0.0 });
    b.add_telemetry(TelemetryConfig {
        transport: mock,
        mode: TelemetryMode::Subset {
            instances: Vec::new(),
            frames: vec!["imu".to_string()],
        },
    });
    let mut coord = b.build().unwrap();
    let cycles = 4;
    coord.run_for(cycles).await;
    // Let the parked sender drain the FIFO tail.
    stellarator::sleep(Duration::from_millis(20)).await;

    // The Log frame entry is announced exactly once (a valid Table announce)...
    let announces = rec.announces.lock().unwrap();
    assert_eq!(announces.len(), 1, "one announce for the one tapped entry");
    assert!(
        announces[0].1.iter().any(|m| m.name == "burst_log.imu.omega"),
        "prefixed announce metadata"
    );
    let packet_id = announces[0].0;

    // ...and EVERY record arrives as its own Table packet under that id — the
    // FIFO lane never coalesces (3 records x N cycles), and each omega is distinct.
    let packets = rec.packets.lock().unwrap();
    let omegas: Vec<f64> = packets
        .iter()
        .filter(|p| packet_id_of(p) == packet_id)
        .map(|p| {
            Imu::read_from_prefix(packet_payload(p))
                .expect("imu bytes")
                .0
                .omega
        })
        .collect();
    assert_eq!(
        omegas.len(),
        3 * cycles,
        "every record downlinked, none coalesced: {omegas:?}"
    );
    let want: Vec<f64> = (1..=(3 * cycles)).map(|i| i as f64).collect();
    assert_eq!(omegas, want, "in order");
}

// ---------------------------------------------------------------------------
// 7. The message FIFO is bounded: past capacity it drops the *oldest* record (never the
//    newest) and counts each drop — the non-coalescing event-log overflow policy.
// ---------------------------------------------------------------------------

#[test]
fn message_handoff_drops_oldest_on_overflow() {
    use std::sync::atomic::Ordering::Relaxed;

    use stellarator::sync::WaitQueue;

    use super::{HandOff, LOG_HANDOFF_CAP};

    let wq = Arc::new(WaitQueue::new());
    let handoff = HandOff::new(0, wq);

    // Push two past capacity; each packet carries a unique id byte so we can identify it.
    let overflow = 2usize;
    for i in 0..(LOG_HANDOFF_CAP + overflow) {
        handoff.push_log(LenPacket::msg([(i & 0xff) as u8, (i >> 8) as u8], 0));
    }

    assert_eq!(
        handoff.dropped_logs.load(Relaxed) as usize,
        overflow,
        "two records past cap were dropped"
    );

    let (snapshots, drained) = handoff.drain();
    assert!(snapshots.is_empty(), "nothing on the snapshot lane");
    assert_eq!(drained.len(), LOG_HANDOFF_CAP, "the FIFO is bounded to its cap");
    // The *oldest* (records 0,1) were dropped; the queue holds records `overflow..`, in
    // order — the newest survive, the front is shed.
    let first_id = [drained[0].inner[5], drained[0].inner[6]];
    assert_eq!(
        first_id,
        [(overflow & 0xff) as u8, (overflow >> 8) as u8],
        "the oldest records were dropped, not the newest"
    );
}

/// The uplink derives its ground subscription from its declared message-output ports — the
/// one table dispatch also derives from (A8), not a hardcoded id: declaring a second command
/// output (ReloadSequences) put its id in the subscription with no other change.
#[test]
fn uplink_subscribes_to_its_declared_command_ids() {
    use metor_proto::types::Msg;
    use metor_proto_wkt::{ReloadSequences, SequenceCommand};
    assert_eq!(
        super::uplink_subscribe_ids(),
        vec![SequenceCommand::ID, ReloadSequences::ID]
    );
}

/// A11(b): "the telemetry downlink registers last" is enforced, not silently reordered —
/// a cyclic system registered *after* the receive-all downlink would telemeter one cycle
/// stale, so `build()` rejects it by name.
#[test]
fn cyclic_after_receive_all_is_a_build_error() {
    let mut b = Coordinator::builder(sim_config());
    b.add_cyclic_named("producer", Producer { n: 0.0 });
    b.add_telemetry(TelemetryConfig {
        transport: MockTransport::new(false),
        mode: TelemetryMode::All,
    });
    // A second producer (no inputs, so nothing else can fail first) after the downlink.
    b.add_cyclic_named("late", Producer { n: 0.0 });
    let err = b.build().err().expect("a late cyclic system fails the build");
    match err {
        crate::WireError::ReceiveAllNotLast {
            system,
            receive_all,
        } => {
            assert_eq!(system, "late");
            assert_eq!(receive_all, "telemetry");
        }
        other => panic!("expected ReceiveAllNotLast, got {other:?}"),
    }
}

/// The uplink's command port is UNTELEMETERED through its real spelling — the
/// `CommandOut` token in the derived `UplinkPorts` bundle (a pure `MsgOut` alias;
/// the flag lives on the descriptor via the derive's token lowering). Guards the
/// A6 regression where the downlink would echo inbound commands back to the panel.
#[test]
fn uplink_command_ports_are_untelemetered() {
    use metor_proto::types::Msg;
    let descs = <super::UplinkPorts as crate::SystemOutput>::port_descs();
    assert_eq!(descs.len(), 2);
    for d in &descs {
        assert!(!d.telemetered, "inbound commands are never downlinked");
        assert_eq!(d.delivery, crate::Delivery::Log);
    }
    assert_eq!(
        descs[0].id,
        crate::PortId::Packet(metor_proto_wkt::SequenceCommand::ID)
    );
    assert_eq!(
        descs[1].id,
        crate::PortId::Packet(metor_proto_wkt::ReloadSequences::ID)
    );
}
