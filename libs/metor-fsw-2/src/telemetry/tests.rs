//! Acceptance tests for the downlink and uplink. They cover the output registry
//! queried by instance-qualified id, the end-to-end downlink against a deterministic
//! in-memory transport (one announce per tapped entry, then batched `Table`
//! packets), prefix disambiguation between two instances of one system type,
//! subset filtering, the non-blocking drop-on-full policy and its health
//! counter, non-coalescing message batching, and uplink subscription and
//! routing.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use metor_proto::types::{ComponentId, LenPacket, PacketId, Timestamp};
use metor_proto_wkt::{ComponentMetadata, VTableMsg};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    ClockMode, Coordinator, CoordinatorConfig, CyclicSystem, Input, Out, Output, PortRef, System,
    SystemHealth, SystemInput, SystemOutput, TelemetryConfig, TelemetryMode, TelemetrySystem,
    Transport, TransportError, UplinkSystem,
};

// ---------------------------------------------------------------------------
// Frames and systems under test (a producer -> consumer chain).
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

/// A cyclic producer writing an incrementing `Imu` (omega equals the cycle index).
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

/// A cyclic consumer that copies `imu.omega` into `nav.angle`.
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
// A deterministic in-memory mock transport for the end-to-end tests.
// ---------------------------------------------------------------------------

struct MockInner {
    announces: Mutex<Vec<(PacketId, Vec<ComponentMetadata>)>>,
    /// Each sent batch, split back into its individual length-prefixed packets
    /// so assertions can address one packet at a time.
    packets: Mutex<Vec<LenPacket>>,
    /// When set, `send` never completes, so the drop test can saturate the queue.
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

    async fn send(&mut self, buf: Vec<u8>) -> Result<Vec<u8>, TransportError> {
        if self.inner.block {
            // Park forever; the cycle keeps running and the batch queue fills.
            std::future::pending::<()>().await;
        }
        let mut packets = self.inner.packets.lock().unwrap();
        let mut rest = &buf[..];
        while !rest.is_empty() {
            let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
            let (pkt, tail) = rest.split_at(4 + len);
            packets.push(LenPacket { inner: pkt.to_vec() });
            rest = tail;
        }
        drop(packets);
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// A free-running simulated clock. `run_for` yields each cycle, so the spawned
/// sender task is scheduled deterministically.
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

/// A `Table` packet's id is the 2 bytes after the 4-byte length and 1 ty byte.
fn packet_id_of(pkt: &LenPacket) -> PacketId {
    [pkt.inner[5], pkt.inner[6]]
}

/// A `Table` packet's payload (the table bytes) begins after the 8-byte packet header.
fn packet_payload(pkt: &LenPacket) -> &[u8] {
    &pkt.inner[8..]
}

/// Drain a view to its newest record and return a copy of the bytes, if any.
fn drain_latest(
    view: &mut metor_fsw_ring::View<metor_fsw_ring::NoWake, metor_fsw_ring::NoWake>,
) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut last = None;
    while let Ok(true) = view.try_read_into(&mut buf) {
        last = Some(buf.clone());
    }
    last
}

// ---------------------------------------------------------------------------
// 1. Query the registry by instance-qualified id, claim a view, read the bytes.
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
    assert!(
        registry
            .get(ComponentId::new("coordinator.health"))
            .is_some()
    );

    // Claim a view before running (a fresh view only sees later commits), run, read.
    let mut view = entry.view().expect("reader slot");
    coord.run_for(5).await;
    let bytes = drain_latest(&mut view).expect("producer wrote at least one imu");
    let imu = Imu::read_from_prefix(&bytes).expect("imu bytes").0;
    assert_eq!(imu.omega, 5.0, "producer increments omega each cycle");
}

// ---------------------------------------------------------------------------
// 2. End-to-end in All mode. Announced prefixed schema, then per-cycle Table packets.
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
    b.add_cyclic(TelemetrySystem::new(TelemetryConfig {
        transport: mock,
        mode: TelemetryMode::All,
    }));
    let mut coord = b.build().unwrap();
    // Only Table entries are announced; Postcard entries (the coordinator's
    // `sequences` and `commands` channels) are self-describing and carry no announce.
    let n_taps = coord
        .registry()
        .entries()
        .iter()
        .filter(|e| matches!(e.schema, crate::EntrySchema::Table { .. }))
        .count();
    coord.run_for(30).await;

    let announces = rec.announces.lock().unwrap();

    // Every output is announced exactly once. That includes the user frames, every
    // system's implicit health/log, and the coordinator-owned buffers. The `log`
    // frames carry only a dynamic `FrameList`, so their announce has an empty
    // metadata set (the count proves they were announced); the scalar-bearing
    // frames carry their prefixed component names.
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
    assert!(
        has_prefix("coordinator.health"),
        "coordinator buffers announced"
    );

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
    assert!(
        !imu_pkts.is_empty(),
        "received Table packets for producer.imu"
    );
    let last = imu_pkts.last().unwrap();
    let imu = Imu::read_from_prefix(packet_payload(last))
        .expect("packet payload is the imu table")
        .0;
    assert!(imu.omega > 0.0, "streamed a real omega: {}", imu.omega);
}

// ---------------------------------------------------------------------------
// 3. Two instances of one type get distinct qualified ids and prefixed names.
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

    // Both entries share the unprefixed frame id, but their qualified keys differ
    // and their announced names carry distinct instance prefixes.
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
// 4. Subset mode taps only the configured instances and frames.
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
    b.add_cyclic(TelemetrySystem::new(TelemetryConfig {
        transport: mock,
        mode: TelemetryMode::Subset {
            instances: vec!["producer".to_string()],
            frames: Vec::new(),
        },
    }));
    let mut coord = b.build().unwrap();
    coord.run_for(20).await;

    let announces = rec.announces.lock().unwrap();
    assert!(!announces.is_empty(), "producer was tapped");
    // Only producer.* was tapped; no consumer, coordinator, or telemetry frames leaked.
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
// 5. Drop policy. A saturated transport never blocks the cycle; drops are surfaced.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn drop_policy_never_blocks_and_counts() {
    // A transport whose `send` never completes. After the first take the batch
    // queue stays full, and every later cycle's batch is dropped and counted.
    let mock = MockTransport::new(true);

    let mut b = Coordinator::builder(sim_config());
    b.add_cyclic_named("producer", Producer { n: 0.0 });
    b.add_cyclic(TelemetrySystem::new(TelemetryConfig {
        transport: mock,
        mode: TelemetryMode::All,
    }));
    let mut coord = b.build().unwrap();

    // Tap the telemetry system's own health frame to observe its error counter.
    let registry = coord.registry();
    let mut health = registry
        .get(ComponentId::new("telemetry.health"))
        .expect("telemetry.health")
        .view()
        .expect("reader slot");

    // The cycle never blocks on the stalled link, so `run_for` completes all cycles.
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
// 6. Message downlink. Every record is framed as a self-describing `Msg`
//    packet in FIFO order, never coalesced and never announced, and tables and
//    messages ride the one batch through the one sender.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn message_downlink_fifo_no_coalesce() {
    use metor_fsw_ring::{Config, NoWake, RingBuffer};
    use metor_proto::types::{Msg, OwnedPacket};
    use metor_proto_wkt::{
        SequenceChannelSpec, SequenceCommand, SequenceCommandKind, SequenceRegistry,
    };

    use crate::message::{LOG_DEPTH, MAX_MSG_BYTES, MsgOut};
    use crate::port::capacity_for;
    use crate::registry::{EntrySchema, RegistryEntry};

    use super::{BATCH_QUEUE_CAP, Wire, append_record, run_sender};

    // A by-hand message channel with the same shape the coordinator registers and
    // the telemetry message tap drains.
    let ring = RingBuffer::create_in_memory(Config {
        capacity: capacity_for(MAX_MSG_BYTES, LOG_DEPTH),
        max_readers: 4,
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
    // The ring enforces a single live writer, so the typed ports take it in turn
    // (each drop frees the claim) and the tap drains one interleaved stream.
    // The log holds three records, two of one Msg type and one of another,
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

    // Frame one cycle's batch exactly as the in-cycle stage does: a component
    // snapshot as a `Table`, then every drained message record in order.
    let mut batch = Vec::new();
    append_record(&mut batch, &Wire::Table { packet_id: [9, 9] }, &[1, 2, 3, 4]);
    let mut scratch = Vec::new();
    while let Ok(true) = view.try_read_into(&mut scratch) {
        append_record(&mut batch, &Wire::Msg, &scratch);
    }

    // Queue the batch and close the channel; the sender drains what is queued,
    // then exits, so the join below is deterministic. With no component taps
    // there are no announces.
    let (tx, rx) = thingbuf::mpsc::channel::<Vec<u8>>(BATCH_QUEUE_CAP);
    tx.try_send(batch).expect("queue has room");
    drop(tx);
    let mock = MockTransport::new(false);
    let rec = mock.inner.clone();
    let _ = stellarator::spawn(run_sender(mock, Vec::new(), rx)).await;

    // No message is announced (self-describing); only the component tap would
    // announce, and we passed none.
    assert!(
        rec.announces.lock().unwrap().is_empty(),
        "messages carry no announce"
    );

    let packets = rec.packets.lock().unwrap();
    // A `LenPacket`'s bytes carry a 4-byte length prefix the wire reader strips
    // before `OwnedPacket::parse` sees the packet header; skip it here too.
    let parsed: Vec<OwnedPacket<Vec<u8>>> = packets
        .iter()
        .map(|p| OwnedPacket::parse(p.inner[4..].to_vec()).expect("parse packet"))
        .collect();

    // The component snapshot came through as a `Table`, proving tables and
    // messages ride the one batch.
    assert_eq!(
        parsed
            .iter()
            .filter(|p| matches!(p, OwnedPacket::Table(_)))
            .count(),
        1,
        "the component snapshot was forwarded too"
    );

    // The three message records arrived as `Msg` packets in FIFO order, none
    // coalesced. Both `SequenceCommand`s are present despite sharing an id
    // (a snapshot would lose one).
    let msgs: Vec<_> = parsed
        .iter()
        .filter_map(|p| match p {
            OwnedPacket::Msg(m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(
        msgs.len(),
        3,
        "every message record forwarded, none coalesced"
    );
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
// 6b. A frame output with Log delivery downlinks every record through the FIFO
//     lane, each framed as a Table packet under one announce, none coalesced.
// ---------------------------------------------------------------------------

/// A producer whose `Imu` output declares `Delivery::Log`, an every-record frame
/// log. Writes three records per cycle; a snapshot tap would coalesce them.
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

impl crate::BindPorts for BurstLogOut {
    fn bind<S: crate::RingSource>(src: &mut S) -> Self {
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
    b.add_cyclic(TelemetrySystem::new(TelemetryConfig {
        transport: mock,
        mode: TelemetryMode::Subset {
            instances: Vec::new(),
            frames: vec!["imu".to_string()],
        },
    }));
    let mut coord = b.build().unwrap();
    let cycles = 4;
    coord.run_for(cycles).await;
    // Let the parked sender drain the FIFO tail.
    stellarator::sleep(Duration::from_millis(20)).await;

    // The Log frame entry is announced exactly once, as a valid Table announce...
    let announces = rec.announces.lock().unwrap();
    assert_eq!(announces.len(), 1, "one announce for the one tapped entry");
    assert!(
        announces[0]
            .1
            .iter()
            .any(|m| m.name == "burst_log.imu.omega"),
        "prefixed announce metadata"
    );
    let packet_id = announces[0].0;

    // ...and every record arrives as its own Table packet under that id. The FIFO
    // lane never coalesces (3 records x N cycles), and each omega is distinct.
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

/// The uplink subscribes to exactly its configured msg set. Subscription,
/// dispatch, and the minted ports all derive from the one `msgs` list, and there
/// is no fallback id, so an empty config subscribes to nothing.
#[cfg(not(miri))]
#[stellarator::test]
async fn uplink_subscribes_to_its_configured_ids() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use metor_proto::types::Msg;
    use metor_proto_wkt::{AlarmAck, ReloadSequences};
    use stellarator::buf::Slice;

    use crate::RecvTransport;

    /// Records the subscription and yields nothing, so the reader stops immediately.
    struct SubRecv {
        subscribed: Rc<RefCell<Option<Vec<PacketId>>>>,
    }

    impl RecvTransport for SubRecv {
        async fn recv(
            &mut self,
        ) -> Result<metor_proto::types::OwnedPacket<Slice<Vec<u8>>>, TransportError> {
            Err(TransportError::Disconnected)
        }

        fn subscribe(&mut self, ids: &[PacketId]) {
            *self.subscribed.borrow_mut() = Some(ids.to_vec());
        }
    }

    // Configured, so the subscription is exactly the two ids, in config order.
    let subscribed = Rc::new(RefCell::new(None));
    let mut b = Coordinator::builder(sim_config());
    b.add_async(
        UplinkSystem::new(SubRecv {
            subscribed: subscribed.clone(),
        })
        .with_msg::<ReloadSequences>()
        .with_msg::<AlarmAck>(),
    );
    let mut coord = b.build().unwrap();
    coord.run_for(20).await;
    assert_eq!(
        subscribed.borrow().clone(),
        Some(vec![ReloadSequences::ID, AlarmAck::ID]),
        "the subscription is the configured set, nothing more"
    );

    // Unconfigured means an empty subscription; the uplink warns rather than
    // guessing an id.
    let subscribed = Rc::new(RefCell::new(None));
    let mut b = Coordinator::builder(sim_config());
    b.add_async(UplinkSystem::new(SubRecv {
        subscribed: subscribed.clone(),
    }));
    let mut coord = b.build().unwrap();
    coord.run_for(20).await;
    assert_eq!(
        subscribed.borrow().clone(),
        Some(Vec::new()),
        "no msgs configured subscribes to nothing"
    );
}

/// A wire `AlarmAck` fed through the uplink routes onto its `acks` port and
/// reaches a consumer over an ordinary message edge, while a malformed postcard
/// payload under the same id is dropped without killing the link (the next
/// packet still arrives).
#[cfg(not(miri))]
#[stellarator::test]
async fn uplink_routes_alarm_acks() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use metor_proto::types::{IntoLenPacket, Msg};
    use metor_proto_wkt::AlarmAck;
    use stellarator::buf::Slice;

    use crate::{MsgIn, RecvTransport};

    /// A fixed script of pre-framed wire packets, then `Disconnected`.
    struct MockRecv {
        queue: std::collections::VecDeque<Vec<u8>>,
    }

    impl RecvTransport for MockRecv {
        async fn recv(
            &mut self,
        ) -> Result<metor_proto::types::OwnedPacket<Slice<Vec<u8>>>, TransportError> {
            use stellarator::buf::IoBuf;
            match self.queue.pop_front() {
                Some(bytes) => {
                    let slice = bytes.try_slice(..).expect("non-empty packet");
                    metor_proto::types::OwnedPacket::parse(slice)
                        .map_err(|e| TransportError::Io(Box::new(e)))
                }
                None => Err(TransportError::Disconnected),
            }
        }
    }

    /// Captures every ack that reaches it over the wired edge.
    struct AckSink {
        seen: Rc<RefCell<Vec<AlarmAck>>>,
    }

    #[derive(SystemInput)]
    struct AckSinkIn {
        acks: MsgIn<AlarmAck>,
    }

    #[derive(SystemOutput)]
    struct AckSinkOut {}

    impl System for AckSink {
        type Input = AckSinkIn;
        type Output = Out<AckSinkOut>;
        const NAME: &'static str = "ack_sink";
    }

    impl CyclicSystem for AckSink {
        fn execute(&mut self, _now: Timestamp, input: &mut AckSinkIn, _o: &mut Self::Output) {
            let seen = &self.seen;
            input.acks.drain(|a| seen.borrow_mut().push(a));
        }
    }

    let ack = AlarmAck {
        def_id: "RATE_HIGH".to_string(),
        occurrence: 42,
        operator: "op".to_string(),
        note: None,
    };
    // Frame one good ack around a malformed payload under the same id, stripping
    // the LenPacket 4-byte length prefix so the bytes match what arrives on the wire.
    let good = ack.into_len_packet().inner[4..].to_vec();
    let mut garbage = LenPacket::msg(AlarmAck::ID, 3);
    garbage.extend_from_slice(&[0xff, 0xff, 0xff]);
    let garbage = garbage.inner[4..].to_vec();

    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = Coordinator::builder(sim_config());
    let sink = b.add_cyclic(AckSink { seen: seen.clone() });
    let uplink = b.add_async(
        UplinkSystem::new(MockRecv {
            queue: vec![garbage, good].into(),
        })
        .with_msg::<AlarmAck>(),
    );
    b.connect(
        PortRef::msg::<AlarmAck>(uplink),
        PortRef::msg::<AlarmAck>(sink),
    )
    .unwrap();
    let mut coord = b.build().unwrap();

    coord.run_for(20).await;

    let got = seen.borrow();
    assert_eq!(
        got.len(),
        1,
        "the malformed payload is dropped, the ack lands"
    );
    assert_eq!(got[0].def_id, "RATE_HIGH");
    assert_eq!(got[0].occurrence, 42);
    assert_eq!(got[0].operator, "op");
}

/// A user-defined Msg type the framework has never seen (its id comes from the
/// blanket hash, with no registered entry anywhere) flows from the wire through
/// the uplink to a typed consumer with nothing but a `with_msg` entry and an
/// ordinary message edge.
#[cfg(not(miri))]
#[stellarator::test]
async fn uplink_relays_arbitrary_user_msgs() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use metor_proto::types::IntoLenPacket;
    use stellarator::buf::Slice;

    use crate::{MsgIn, RecvTransport};

    /// A mission-local command type, unknown to the framework.
    #[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug)]
    struct SetGain {
        gain: u32,
    }
    impl crate::NamedMsg for SetGain {
        const NAME: &'static str = "SetGain";
    }

    /// A fixed script of pre-framed wire packets, then `Disconnected`.
    struct MockRecv {
        queue: std::collections::VecDeque<Vec<u8>>,
    }

    impl RecvTransport for MockRecv {
        async fn recv(
            &mut self,
        ) -> Result<metor_proto::types::OwnedPacket<Slice<Vec<u8>>>, TransportError> {
            use stellarator::buf::IoBuf;
            match self.queue.pop_front() {
                Some(bytes) => {
                    let slice = bytes.try_slice(..).expect("non-empty packet");
                    metor_proto::types::OwnedPacket::parse(slice)
                        .map_err(|e| TransportError::Io(Box::new(e)))
                }
                None => Err(TransportError::Disconnected),
            }
        }
    }

    struct GainSink {
        seen: Rc<RefCell<Vec<u32>>>,
    }

    #[derive(SystemInput)]
    struct GainSinkIn {
        cmds: MsgIn<SetGain>,
    }

    #[derive(SystemOutput)]
    struct GainSinkOut {}

    impl System for GainSink {
        type Input = GainSinkIn;
        type Output = Out<GainSinkOut>;
        const NAME: &'static str = "gain_sink";
    }

    impl CyclicSystem for GainSink {
        fn execute(&mut self, _now: Timestamp, input: &mut GainSinkIn, _o: &mut Self::Output) {
            let seen = &self.seen;
            input.cmds.drain(|c| seen.borrow_mut().push(c.gain));
        }
    }

    // Strip the LenPacket 4-byte length prefix so the bytes match what arrives
    // on the wire.
    let wire = SetGain { gain: 7 }.into_len_packet().inner[4..].to_vec();

    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = Coordinator::builder(sim_config());
    let sink = b.add_cyclic(GainSink { seen: seen.clone() });
    let uplink = b.add_async(
        UplinkSystem::new(MockRecv {
            queue: vec![wire].into(),
        })
        .with_msg::<SetGain>(),
    );
    b.connect(
        PortRef::msg::<SetGain>(uplink),
        PortRef::msg::<SetGain>(sink),
    )
    .unwrap();
    let mut coord = b.build().unwrap();

    coord.run_for(20).await;

    assert_eq!(
        *seen.borrow(),
        vec![7],
        "the user msg landed at its consumer"
    );
}

/// The rule that the downlink registers last is enforced, not silently
/// reordered. A cyclic system registered after the receive-all downlink would
/// telemeter one cycle stale, so `build()` rejects it by name.
#[test]
fn cyclic_after_receive_all_is_a_build_error() {
    let mut b = Coordinator::builder(sim_config());
    b.add_cyclic_named("producer", Producer { n: 0.0 });
    b.add_cyclic(TelemetrySystem::new(TelemetryConfig {
        transport: MockTransport::new(false),
        mode: TelemetryMode::All,
    }));
    // A second producer (no inputs, so nothing else can fail first) after the downlink.
    b.add_cyclic_named("late", Producer { n: 0.0 });
    let err = b
        .build()
        .err()
        .expect("a late cyclic system fails the build");
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

/// Every config-minted uplink port is untelemetered (so the downlink never
/// echoes inbound commands back over the link), Log-delivery, and keyed and
/// named by its configured msg. The minted ports follow the static health/log
/// pair in config order, the same order [`MsgFanOut::bind`] pops rings.
#[test]
fn uplink_minted_ports_are_untelemetered() {
    use metor_proto::types::Msg;
    use metor_proto_wkt::{AlarmAck, ReloadSequences, SequenceCommand};
    use stellarator::buf::Slice;

    use crate::RecvTransport;

    struct NoRecv;
    impl RecvTransport for NoRecv {
        async fn recv(
            &mut self,
        ) -> Result<metor_proto::types::OwnedPacket<Slice<Vec<u8>>>, TransportError> {
            Err(TransportError::Disconnected)
        }
    }

    let uplink = UplinkSystem::new(NoRecv)
        .with_msg::<SequenceCommand>()
        .with_msg::<ReloadSequences>()
        .with_msg::<AlarmAck>()
        // Idempotent, so a repeat is one port, not a duplicate registry key.
        .with_msg::<AlarmAck>();
    let desc = crate::AsyncSystem::instance_descriptor(&uplink);

    // The static shape (health, log) first, then one port per configured msg.
    assert_eq!(desc.outputs.len(), 5);
    let minted = &desc.outputs[2..];
    for d in minted {
        assert!(!d.telemetered, "inbound commands are never downlinked");
        assert_eq!(d.delivery, crate::Delivery::Log);
    }
    assert_eq!(minted[0].id, crate::PortId::Packet(SequenceCommand::ID));
    assert_eq!(minted[0].name, "SequenceCommand");
    assert_eq!(minted[1].id, crate::PortId::Packet(ReloadSequences::ID));
    assert_eq!(minted[2].id, crate::PortId::Packet(AlarmAck::ID));
}

/// `DownlinkParams`' two optional subset lists project onto `TelemetryMode` as
/// documented. Both absent taps everything; either present selects a subset
/// with the missing list empty.
#[test]
fn downlink_params_mode_projection() {
    let all = super::DownlinkParams {
        addr: "127.0.0.1:2240".parse().unwrap(),
        instances: None,
        frames: None,
    };
    assert!(matches!(all.mode(), TelemetryMode::All));

    let subset = super::DownlinkParams {
        addr: "127.0.0.1:2240".parse().unwrap(),
        instances: Some(vec!["nav".to_string()]),
        frames: None,
    };
    match subset.mode() {
        TelemetryMode::Subset { instances, frames } => {
            assert_eq!(instances, vec!["nav".to_string()]);
            assert!(frames.is_empty());
        }
        other => panic!("expected Subset, got {other:?}"),
    }
}
