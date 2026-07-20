//! Acceptance tests for the downlink and uplink over the shared link server.
//! They cover the output registry queried by instance-qualified id, the
//! end-to-end downlink over a real loopback connection (per-connection
//! announce replay, then batched `Table` packets), prefix disambiguation
//! between two instances of one system type, subset filtering, the
//! stalled-consumer drop policy and its health counter, non-coalescing
//! message batching, and uplink routing off the server's inbound queue.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use metor_proto::types::{ComponentId, Msg as _, OwnedPacket, PacketId, Timestamp};
use metor_proto_stellar::PacketStream;
use metor_proto_wkt::{ComponentMetadata, SetComponentMetadata, SetMsgMetadata, VTableMsg};
use stellarator::net::TcpStream;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::{
    ClockMode, CoordinatorConfig, CyclicSystem, Input, Out, Output, Shared, System, SystemHealth,
    SystemInput, SystemOutput, TelemetryConfig, TelemetryMode, TelemetrySystem, UplinkSystem,
};

use super::LinkState;
use crate::coordinator::PortRef;
use crate::coordinator::init::cyclic_node;
use crate::descriptor::PortId;
use crate::frame::Frame as _;

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
        if let Ok(Some(imu)) = input.imu.latest() {
            let _ = o.nav.write(&Nav {
                timestamp: now,
                angle: imu.get().omega,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers: a bound link, a real loopback wire tap, and announce decoding.
// ---------------------------------------------------------------------------

/// A free-running simulated clock. `run_for` yields each cycle, so the
/// server's tasks are scheduled deterministically alongside it.
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

/// A wall clock paced fast: the loopback tests need the cycle loop to sleep
/// out its budget, because only a parked loop lets the io reactor poll — a
/// simulated clock never sleeps, so socket io would starve until the run
/// ends (the same starvation a live sim run accepts).
fn wall_config() -> CoordinatorConfig {
    CoordinatorConfig {
        cycle_rate: 1000.0,
        default_depth: 8,
        clock: ClockMode::Wall,
        ..CoordinatorConfig::default()
    }
}

/// A constructed link state on an ephemeral port. `start` spawns its accept
/// loop by hand — these tests register the systems directly (no pack), so no
/// attached lifecycle runs it.
fn test_link(start: bool) -> Shared<LinkState> {
    let link = Shared::new("TcpServer");
    link.set(LinkState::bind("127.0.0.1:0".parse().unwrap()).expect("bind"))
        .ok();
    if start {
        crate::SharedLifecycle::start(&mut *link.get());
    }
    link
}

fn downlink(link: &Shared<LinkState>, mode: TelemetryMode) -> TelemetrySystem {
    TelemetrySystem::new(TelemetryConfig {
        link: link.clone(),
        mode,
    })
}

/// One packet off the wire, owned.
#[derive(Debug)]
enum WirePkt {
    Table { id: PacketId, payload: Vec<u8> },
    Msg { id: PacketId, payload: Vec<u8> },
}

/// A real loopback client: connects to the link, reads packets forever, and
/// accumulates them for assertions.
struct WireTap {
    pkts: Rc<RefCell<Vec<WirePkt>>>,
    _reader: stellarator::JoinHandleDropGuard<()>,
}

impl WireTap {
    async fn connect(link: &Shared<LinkState>) -> Self {
        use stellarator::io::SplitExt;
        let addr = link.get().local_addr();
        let stream = TcpStream::connect(addr).await.expect("connect the tap");
        let (rx, tx) = stream.split();
        let pkts: Rc<RefCell<Vec<WirePkt>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = pkts.clone();
        let reader = stellarator::spawn(async move {
            // The write half rides in the task so the socket stays fully open.
            let _tx = tx;
            let mut stream = PacketStream::new(rx);
            let mut buf = vec![0u8; 1024];
            loop {
                match stream.next_grow(buf).await {
                    Ok(pkt) => {
                        match &pkt {
                            OwnedPacket::Table(t) => sink.borrow_mut().push(WirePkt::Table {
                                id: t.id,
                                payload: t.buf[..].to_vec(),
                            }),
                            OwnedPacket::Msg(m) => sink.borrow_mut().push(WirePkt::Msg {
                                id: m.id,
                                payload: m.buf[..].to_vec(),
                            }),
                            _ => {}
                        }
                        buf = pkt.into_buf().into_inner();
                    }
                    Err(_) => return,
                }
            }
        })
        .drop_guard();
        Self {
            pkts,
            _reader: reader,
        }
    }

    /// Wait (bounded) until `pred` holds over the received packets.
    async fn settle(&self, pred: impl Fn(&[WirePkt]) -> bool) {
        for _ in 0..500 {
            if pred(&self.pkts.borrow()) {
                return;
            }
            stellarator::sleep(Duration::from_millis(1)).await;
        }
        panic!("wire tap never settled; got {:?}", self.pkts.borrow());
    }

    /// The table announces received, reconstructed from the replay stream:
    /// one `(packet id, component metadata)` group per `VTableMsg`.
    fn announces(&self) -> Vec<(PacketId, Vec<ComponentMetadata>)> {
        let mut groups: Vec<(PacketId, Vec<ComponentMetadata>)> = Vec::new();
        for pkt in self.pkts.borrow().iter() {
            if let WirePkt::Msg { id, payload } = pkt {
                if *id == VTableMsg::ID {
                    let vt: VTableMsg = postcard::from_bytes(payload).expect("VTableMsg decodes");
                    groups.push((vt.id, Vec::new()));
                } else if *id == SetMsgMetadata::ID {
                    continue;
                } else if *id == SetComponentMetadata::ID {
                    let meta: SetComponentMetadata =
                        postcard::from_bytes(payload).expect("SetComponentMetadata decodes");
                    groups
                        .last_mut()
                        .expect("component metadata follows its VTableMsg")
                        .1
                        .push(meta.0);
                }
            }
        }
        groups
    }

    /// The message-channel schema announces received.
    fn msg_announces(&self) -> Vec<SetMsgMetadata> {
        self.pkts
            .borrow()
            .iter()
            .filter_map(|pkt| match pkt {
                WirePkt::Msg { id, payload } if *id == SetMsgMetadata::ID => {
                    Some(postcard::from_bytes(payload).expect("SetMsgMetadata decodes"))
                }
                _ => None,
            })
            .collect()
    }

    /// The data `Table` packets under `id`, in arrival order.
    fn tables(&self, id: PacketId) -> Vec<Vec<u8>> {
        self.pkts
            .borrow()
            .iter()
            .filter_map(|pkt| match pkt {
                WirePkt::Table { id: pid, payload } if *pid == id => Some(payload.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Drain a view to its newest record and return a copy of the bytes, if any.
fn drain_latest(view: &mut metor_fsw_ring::View<metor_fsw_ring::NoWake>) -> Option<Vec<u8>> {
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
    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    let p = b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
    let c = b.push_node(cyclic_node("consumer".into(), Consumer));
    b.connect(
        PortRef {
            system: p,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: c,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let mut coord = b.build().unwrap();

    // The registry indexes every output by `ComponentId::new("<instance>.<frame>")`.
    let registry = coord.registry();
    let entry = registry
        .get(ComponentId::new("producer.imu"))
        .expect("producer.imu in the registry");
    assert!(matches!(
        entry.desc.schema,
        crate::PortSchema::Table { frame_id, .. } if frame_id == ComponentId::new("imu")
    ));
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

/// A mission namespace shifts every registry key and announced leaf id under
/// the `<namespace>.` prefix — user frames and the coordinator's own buffers
/// alike — while the port's own frame id and the unprefixed identity are left
/// for the un-namespaced case (pinned by `registry_query_view_and_read`).
#[cfg(not(miri))]
#[stellarator::test]
async fn namespace_prefixes_registry_keys_and_announce() {
    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    b.namespace = Some("sat1".into());
    let p = b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
    let c = b.push_node(cyclic_node("consumer".into(), Consumer));
    b.connect(
        PortRef {
            system: p,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: c,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    let coord = b.build().unwrap();
    let registry = coord.registry();

    // The key is the namespace-qualified id; the bare id no longer resolves.
    let entry = registry
        .get(ComponentId::new("sat1.producer.imu"))
        .expect("sat1.producer.imu in the registry");
    assert!(registry.get(ComponentId::new("producer.imu")).is_none());
    assert_eq!(&*entry.instance, "sat1.producer");
    // The port's own frame id is namespace-independent: only the qualified
    // key and the announced leaves carry the prefix.
    assert!(matches!(
        entry.desc.schema,
        crate::PortSchema::Table { frame_id, .. } if frame_id == ComponentId::new("imu")
    ));

    // The announce form nests every leaf under the qualified instance.
    let (_, metadata) = entry.announce().expect("Table entry announces");
    let omega = metadata
        .iter()
        .find(|m| m.name == "sat1.producer.imu.omega")
        .expect("announced omega leaf under the namespace");
    assert_eq!(
        omega.component_id,
        ComponentId::new("sat1.producer.imu.omega")
    );

    // The coordinator's own reserved buffers are qualified through the same
    // seam, so its `Node::name` stays `"coordinator"` for wiring while its
    // telemetry keys move under the namespace.
    assert!(
        registry
            .get(ComponentId::new("sat1.coordinator.health"))
            .is_some()
    );
    assert!(registry.get(ComponentId::new("coordinator.health")).is_none());
}

// ---------------------------------------------------------------------------
// 2. End-to-end in All mode over a real connection: the announce replay
//    first, then per-cycle Table packets.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn telemetry_end_to_end_all() {
    let link = test_link(true);
    let tap = WireTap::connect(&link).await;

    let mut b = crate::coordinator::init::InitGraph::new(wall_config());
    let p = b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
    let c = b.push_node(cyclic_node("consumer".into(), Consumer));
    b.connect(
        PortRef {
            system: p,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: c,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    b.push_node(cyclic_node(
        "telemetry".into(),
        downlink(&link, TelemetryMode::All),
    ));
    let mut coord = b.build().unwrap();
    // Only Table entries are announced; Postcard entries (the coordinator's
    // `sequences` channel) are self-describing and carry no announce.
    let n_taps = coord
        .registry()
        .entries()
        .iter()
        .filter(|e| matches!(e.desc.schema, crate::PortSchema::Table { .. }))
        .count();
    coord.run_for(30).await;
    tap.settle(|pkts| {
        pkts.iter()
            .any(|p| matches!(p, WirePkt::Table { .. }))
    })
    .await;

    let announces = tap.announces();

    // Every output is announced exactly once, ahead of any data. That
    // includes the user frames, every system's implicit health/log, and the
    // coordinator-owned buffers. The `log` frames carry only a dynamic
    // `FrameList`, so their announce has an empty metadata set (the count
    // proves they were announced); the scalar-bearing frames carry their
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
    assert!(
        has_prefix("coordinator.health"),
        "coordinator buffers announced"
    );
    // The downlink's own link gauge is a frame like any other.
    assert!(has_prefix("telemetry.link_status"), "gauge announced");

    // The producer.imu tap's `Table` packets carry the committed frame bytes verbatim.
    let imu_pid = announces
        .iter()
        .find(|(_, meta)| meta.iter().any(|m| m.name == "producer.imu.omega"))
        .map(|(id, _)| *id)
        .expect("producer.imu announced with an id");
    let imu_pkts = tap.tables(imu_pid);
    assert!(
        !imu_pkts.is_empty(),
        "received Table packets for producer.imu"
    );
    let imu = Imu::read_from_prefix(imu_pkts.last().unwrap())
        .expect("packet payload is the imu table")
        .0;
    assert!(imu.omega > 0.0, "streamed a real omega: {}", imu.omega);
}

// ---------------------------------------------------------------------------
// 2a. Message channels announce their payload schema, deduped by packet id.
// ---------------------------------------------------------------------------

/// Every tapped Postcard port carries its payload schema; the downlink
/// announces one `SetMsgMetadata` per DISTINCT packet id (both systems' `log`
/// ports share `LogEvent::ID`), named by the schema's type name.
#[cfg(not(miri))]
#[stellarator::test]
async fn msg_schemas_announce_once_per_id() {
    use metor_proto_wkt::{LogEvent, SequenceRegistry};

    let link = test_link(true);
    let tap = WireTap::connect(&link).await;
    let mut b = crate::coordinator::init::InitGraph::new(wall_config());
    b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
    b.push_node(cyclic_node("producer_b".into(), Producer { n: 0.0 }));
    b.push_node(cyclic_node(
        "telemetry".into(),
        downlink(&link, TelemetryMode::All),
    ));
    let mut coord = b.build().unwrap();
    coord.run_for(2).await;
    tap.settle(|pkts| !pkts.is_empty()).await;

    let msgs = tap.msg_announces();
    let log_events: Vec<_> = msgs.iter().filter(|m| m.id == LogEvent::ID).collect();
    assert_eq!(
        log_events.len(),
        1,
        "one announce for the shared LogEvent id, not one per system"
    );
    assert_eq!(log_events[0].metadata.name, "LogEvent");
    assert_eq!(log_events[0].metadata.schema.name, "LogEvent");
    // The coordinator's own telemetered message channels announce too; its
    // untelemetered `commands` channel must not.
    assert!(msgs.iter().any(|m| m.id == SequenceRegistry::ID));
    assert!(
        msgs.iter()
            .all(|m| m.id != metor_proto_wkt::SequenceCommand::ID),
        "untelemetered channels are never announced"
    );
}

// ---------------------------------------------------------------------------
// 2b. Health-port log lines downlink as self-describing LogEvent Msg packets.
// ---------------------------------------------------------------------------

/// A system that logs one line through its health port each cycle.
struct Chatty;

#[derive(SystemOutput)]
struct ChattyOut {}

impl System for Chatty {
    type Input = NoIn;
    type Output = Out<ChattyOut>;
    const NAME: &'static str = "chatty";
}

impl CyclicSystem for Chatty {
    fn execute(&mut self, _now: Timestamp, _in: &mut NoIn, o: &mut Self::Output) {
        o.health().log(crate::LogLevel::Warn, "thruster hiccup");
    }
}

/// The implicit log port is an ordinary message channel, so the downlink's
/// receive-all tap forwards each queued line as a `Msg` packet keyed
/// `LogEvent::ID`, stamped with the emitting instance's name.
#[cfg(not(miri))]
#[stellarator::test]
async fn health_log_lines_downlink_as_log_events() {
    use metor_proto_wkt::{LogEvent, LogLevel};

    let link = test_link(true);
    let tap = WireTap::connect(&link).await;
    let mut b = crate::coordinator::init::InitGraph::new(wall_config());
    b.push_node(cyclic_node("chatty".into(), Chatty));
    b.push_node(cyclic_node(
        "telemetry".into(),
        downlink(&link, TelemetryMode::All),
    ));
    let mut coord = b.build().unwrap();
    coord.run_for(10).await;

    let is_chatty_event = |pkts: &[WirePkt]| {
        pkts.iter().any(|p| {
            matches!(p, WirePkt::Msg { id, payload } if *id == LogEvent::ID
                && postcard::from_bytes::<LogEvent>(payload)
                    .is_ok_and(|ev| ev.source == "chatty"))
        })
    };
    tap.settle(is_chatty_event).await;

    let pkts = tap.pkts.borrow();
    let ev = pkts
        .iter()
        .filter_map(|p| match p {
            WirePkt::Msg { id, payload } if *id == LogEvent::ID => {
                postcard::from_bytes::<LogEvent>(payload).ok()
            }
            _ => None,
        })
        .find(|ev| ev.source == "chatty")
        .expect("chatty's line arrives keyed to its instance name");
    assert_eq!(ev.level, LogLevel::Warn);
    assert_eq!(ev.message, "thruster hiccup");
    assert_eq!(ev.target, "", "health-port lines carry no tracing target");
    assert_eq!(ev.span, None);
}

// ---------------------------------------------------------------------------
// 3. Two instances of one type get distinct qualified ids and prefixed names.
// ---------------------------------------------------------------------------

#[test]
fn two_instances_distinct_prefixes() {
    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    // Two producers of the same type; neither has inputs, so the graph builds.
    b.push_node(cyclic_node("imu_left".into(), Producer { n: 0.0 }));
    b.push_node(cyclic_node("imu_right".into(), Producer { n: 0.0 }));
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
    let crate::PortSchema::Table {
        frame_id: left_id, ..
    } = &left.desc.schema
    else {
        panic!("frame entry carries a Table schema");
    };
    let crate::PortSchema::Table {
        frame_id: right_id, ..
    } = &right.desc.schema
    else {
        panic!("frame entry carries a Table schema");
    };
    let (_, left_meta) = left.announce().expect("Table entry announces");
    let (_, right_meta) = right.announce().expect("Table entry announces");
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
    let link = test_link(true);
    let tap = WireTap::connect(&link).await;

    let mut b = crate::coordinator::init::InitGraph::new(wall_config());
    let p = b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
    let c = b.push_node(cyclic_node("consumer".into(), Consumer));
    b.connect(
        PortRef {
            system: p,
            port: PortId::Component(Imu::FRAME_ID),
        },
        PortRef {
            system: c,
            port: PortId::Component(Imu::FRAME_ID),
        },
    );
    b.push_node(cyclic_node(
        "telemetry".into(),
        downlink(
            &link,
            TelemetryMode::Subset {
                instances: vec!["producer".to_string()],
                frames: Vec::new(),
            },
        ),
    ));
    let mut coord = b.build().unwrap();
    coord.run_for(20).await;
    tap.settle(|pkts| pkts.iter().any(|p| matches!(p, WirePkt::Table { .. })))
        .await;

    let announces = tap.announces();
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
// 5. Drop policy. A stalled consumer never blocks the cycle; drops surface.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn drop_policy_never_blocks_and_counts() {
    // A connection whose pending buffer sits at its cap: every broadcast to
    // it drops and is counted, while the cycle and its sibling consumers run on.
    let link = test_link(false);
    let stalled = link.get().push_test_conn();
    stalled.stall();

    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    let prod = b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
    // A downstream consumer of the same output telemetry taps: the stalled
    // link must not starve it (regression: an undrained tap view stalls the
    // producer's ring for EVERY consumer, freezing the whole mission).
    let cons = b.push_node(cyclic_node(Consumer::NAME.into(), Consumer));
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
    b.push_node(cyclic_node(
        "telemetry".into(),
        downlink(&link, TelemetryMode::All),
    ));
    let mut coord = b.build().unwrap();

    // Tap the telemetry system's own health frame to observe its error counter,
    // and the consumer's output to observe fresh data flowing end-to-end.
    let registry = coord.registry();
    let mut health = registry
        .get(ComponentId::new("telemetry.health"))
        .expect("telemetry.health")
        .view()
        .expect("reader slot");
    let nav_view = registry
        .get(ComponentId::new("consumer.nav"))
        .expect("consumer.nav")
        .view()
        .expect("reader slot");

    // Sample `consumer.nav` every cycle (a held-but-idle view would itself
    // stall the nav ring — the very failure mode under test).
    let last_angle = Rc::new(RefCell::new(0.0f64));
    let captured = last_angle.clone();
    let sampler = stellarator::spawn(async move {
        let mut nav: crate::port::Input<Nav> = crate::port::Input::new(nav_view);
        loop {
            stellarator::yield_now().await;
            if let Ok(Some(n)) = nav.latest() {
                *captured.borrow_mut() = n.get().angle;
            }
        }
    })
    .drop_guard();

    // The cycle never blocks on the stalled connection, so `run_for` completes.
    coord.run_for(50).await;
    drop(sampler);

    let bytes = drain_latest(&mut health).expect("telemetry published a health frame");
    let h = SystemHealth::read_from_prefix(&bytes)
        .expect("health bytes")
        .0;
    assert!(
        h.errors > 0,
        "stalled connection should have surfaced link_conn_dropped (errors={})",
        h.errors
    );
    // The consumer kept receiving fresh records through the stalled link: its
    // final output carries the producer's last value, not an early frozen one
    // (regression: the no-room path once returned without draining the
    // taps, freezing every ring at depth).
    let angle = *last_angle.borrow();
    assert!(
        angle >= 49.0,
        "consumer starved by the stalled downlink: last angle {angle}"
    );
}

/// A coordinator whose link has no listener started and no connections tears
/// down cleanly, twice on one runtime — the no-ground-station state must
/// leave nothing behind that poisons the next run.
#[stellarator::test]
async fn idle_link_coordinator_teardown_is_clean() {
    for round in 0..2 {
        let link = test_link(false);
        let mut b = crate::coordinator::init::InitGraph::new(sim_config());
        b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
        b.push_node(cyclic_node(
            "telemetry".into(),
            downlink(&link, TelemetryMode::All),
        ));
        let mut coord = b.build().unwrap();
        coord.run_for(20).await;
        assert!(
            coord.stopped().is_empty(),
            "round {round}: no system stopped"
        );
    }
    for _ in 0..10 {
        stellarator::yield_now().await;
    }
}

// ---------------------------------------------------------------------------
// 6. Message downlink. Every record is framed as a self-describing `Msg`
//    packet in FIFO order, never coalesced and never announced, and tables and
//    messages ride the one batch.
// ---------------------------------------------------------------------------

#[cfg(not(miri))]
#[stellarator::test]
async fn message_downlink_fifo_no_coalesce() {
    use metor_fsw_ring::{Config, NoWake, RingBuffer};
    use metor_proto_wkt::{
        SequenceChannelSpec, SequenceCommand, SequenceCommandKind, SequenceRegistry,
    };
    use std::sync::Arc;

    use crate::message::{LOG_DEPTH, MAX_MSG_BYTES, MsgOut};
    use crate::port::capacity_for;
    use crate::registry::RegistryEntry;

    use super::{Wire, append_record};

    // A by-hand message channel with the same shape the coordinator registers and
    // the telemetry message tap drains.
    let ring = RingBuffer::create_in_memory(Config {
        capacity: capacity_for(MAX_MSG_BYTES, LOG_DEPTH),
        max_readers: 4,
    });
    let entry = RegistryEntry {
        key: ComponentId::new("mode.events"),
        instance: Arc::from("mode"),
        desc: crate::PortDesc::msg_named::<SequenceRegistry>("events"),
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
            MsgOut::new(ring.writer(NoWake).expect("first writer"));
        cmd_out
            .emit(&SequenceCommand {
                channel: "mode".to_string(),
                command: SequenceCommandKind::Start,
            })
            .expect("emit start");
    }
    {
        let mut reg_out: MsgOut<SequenceRegistry> =
            MsgOut::new(ring.writer(NoWake).expect("claim freed on drop"));
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
        MsgOut::new(ring.writer(NoWake).expect("claim freed on drop"));
    cmd_out
        .emit(&SequenceCommand {
            channel: "mode".to_string(),
            command: SequenceCommandKind::Abort,
        })
        .expect("emit abort");

    // Frame one cycle's batch exactly as the in-cycle stage does: a component
    // snapshot as a `Table`, then every drained message record in order.
    let mut batch = Vec::new();
    append_record(
        &mut batch,
        &Wire::Table { packet_id: [9, 9] },
        &[1, 2, 3, 4],
    );
    let mut scratch = Vec::new();
    while let Ok(true) = view.try_read_into(&mut scratch) {
        append_record(&mut batch, &Wire::Msg, &scratch);
    }

    // Broadcast the batch to a test connection and read its buffered bytes
    // back as packets.
    let link = test_link(false);
    let (conn, seeded) = {
        let state = link.get();
        state.set_announces(&[]).ok();
        let conn = state.push_test_conn();
        let seeded = conn.pending_bytes().len();
        state.broadcast(&batch);
        (conn, seeded)
    };
    let bytes = conn.pending_bytes()[seeded..].to_vec();
    let mut parsed: Vec<OwnedPacket<Vec<u8>>> = Vec::new();
    let mut rest = &bytes[..];
    while !rest.is_empty() {
        let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        let (pkt, tail) = rest.split_at(4 + len);
        parsed.push(OwnedPacket::parse(pkt[4..].to_vec()).expect("parse packet"));
        rest = tail;
    }

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
    fn decls() -> crate::Declarations {
        vec![crate::PortDesc::of::<Imu>().with_delivery(crate::Delivery::Log)].into()
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
    let link = test_link(true);
    let tap = WireTap::connect(&link).await;

    let mut b = crate::coordinator::init::InitGraph::new(wall_config());
    b.push_node(cyclic_node(
        BurstLogProducer::NAME.into(),
        BurstLogProducer { n: 0.0 },
    ));
    b.push_node(cyclic_node(
        "telemetry".into(),
        downlink(
            &link,
            TelemetryMode::Subset {
                instances: Vec::new(),
                frames: vec!["imu".to_string()],
            },
        ),
    ));
    let mut coord = b.build().unwrap();
    let cycles = 4;
    coord.run_for(cycles).await;

    // The Log frame entry is announced exactly once, as a valid Table announce...
    tap.settle(|pkts| {
        pkts.iter().any(|p| matches!(p, WirePkt::Table { payload, .. }
            if Imu::read_from_prefix(payload).is_ok_and(|(imu, _)| imu.omega == (3 * cycles) as f64)))
    })
    .await;
    let announces = tap.announces();
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
    let omegas: Vec<f64> = tap
        .tables(packet_id)
        .iter()
        .map(|payload| {
            Imu::read_from_prefix(payload)
                .expect("imu bytes")
                .0
                .omega
        })
        .collect();
    // The connection joins a cycle or two into the run (batches are only
    // framed for live connections), so the stream is a contiguous tail:
    // whole leading cycles may be absent, but nothing within it may be
    // coalesced or reordered, and it ends at the final record.
    assert!(
        omegas.len() >= 3,
        "at least one full cycle downlinked: {omegas:?}"
    );
    for pair in omegas.windows(2) {
        assert_eq!(pair[1] - pair[0], 1.0, "no coalescing within the tail: {omegas:?}");
    }
    assert_eq!(*omegas.last().unwrap(), (3 * cycles) as f64, "runs to the last record");
}

// ---------------------------------------------------------------------------
// 7. Uplink routing off the server's inbound queue.
// ---------------------------------------------------------------------------

/// A cyclic sink that captures every `AlarmAck` reaching it over a wired edge.
struct AckSink {
    seen: Rc<RefCell<Vec<metor_proto_wkt::AlarmAck>>>,
}

#[derive(SystemInput)]
struct AckSinkIn {
    acks: crate::MsgIn<metor_proto_wkt::AlarmAck>,
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
        input.acks.drain(|a| seen.borrow_mut().push(a)).unwrap();
    }
}

/// A queued `AlarmAck` routes onto the uplink's `acks` port and reaches a
/// consumer over an ordinary message edge; a malformed postcard payload
/// under the same id is dropped by the consumer's own drain without wedging
/// anything (the next msg still arrives), and an id outside the configured
/// set bumps `uplink_unroutable` instead of vanishing silently.
#[cfg(not(miri))]
#[stellarator::test]
async fn uplink_routes_and_survives_garbage() {
    use metor_proto_wkt::AlarmAck;

    let ack = AlarmAck {
        def_id: "RATE_HIGH".to_string(),
        occurrence: 42,
        operator: "op".to_string(),
        note: None,
    };
    let link = test_link(false);
    {
        let state = link.get();
        state.push_inbound(AlarmAck::ID, &[0xff, 0xff, 0xff]);
        state.push_inbound(AlarmAck::ID, &postcard::to_allocvec(&ack).unwrap());
        state.push_inbound([0x7f, 0x7f], b"lost");
    }

    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    // The uplink steps before its consumer, so the queued msgs land the same
    // cycle they are republished.
    let uplink = b.push_node(cyclic_node(
        "uplink".into(),
        UplinkSystem::new().attach(link.clone()).with_msg::<AlarmAck>(),
    ));
    let sink = b.push_node(cyclic_node(
        AckSink::NAME.into(),
        AckSink { seen: seen.clone() },
    ));
    b.connect(
        PortRef {
            system: uplink,
            port: PortId::Packet(AlarmAck::ID),
        },
        PortRef {
            system: sink,
            port: PortId::Packet(AlarmAck::ID),
        },
    );
    let mut coord = b.build().unwrap();

    let mut uplink_health = coord
        .registry()
        .get(ComponentId::new("uplink.health"))
        .expect("uplink.health")
        .view()
        .expect("reader slot");

    coord.run_for(5).await;

    let got = seen.borrow();
    assert_eq!(
        got.len(),
        1,
        "the malformed payload is dropped, the ack lands"
    );
    assert_eq!(got[0].def_id, "RATE_HIGH");
    assert_eq!(got[0].occurrence, 42);
    assert_eq!(got[0].operator, "op");

    let bytes = drain_latest(&mut uplink_health).expect("uplink health published");
    let h = SystemHealth::read_from_prefix(&bytes).expect("health").0;
    assert!(h.errors > 0, "the unroutable id was counted");
}

/// A user-defined Msg type the framework has never seen (its id comes from the
/// blanket hash, with no registered entry anywhere) flows off the link through
/// the uplink to a typed consumer with nothing but a `with_msg` entry and an
/// ordinary message edge.
#[cfg(not(miri))]
#[stellarator::test]
async fn uplink_relays_arbitrary_user_msgs() {
    use crate::MsgIn;

    /// A mission-local command type, unknown to the framework.
    #[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug)]
    struct SetGain {
        gain: u32,
    }
    impl crate::NamedMsg for SetGain {
        const NAME: &'static str = "SetGain";
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
            input
                .cmds
                .drain(|c| seen.borrow_mut().push(c.gain))
                .unwrap();
        }
    }

    let link = test_link(false);
    link.get()
        .push_inbound(SetGain::ID, &postcard::to_allocvec(&SetGain { gain: 7 }).unwrap());

    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    let uplink = b.push_node(cyclic_node(
        "uplink".into(),
        UplinkSystem::new().attach(link.clone()).with_msg::<SetGain>(),
    ));
    let sink = b.push_node(cyclic_node(
        GainSink::NAME.into(),
        GainSink { seen: seen.clone() },
    ));
    b.connect(
        PortRef {
            system: uplink,
            port: PortId::Packet(SetGain::ID),
        },
        PortRef {
            system: sink,
            port: PortId::Packet(SetGain::ID),
        },
    );
    let mut coord = b.build().unwrap();

    coord.run_for(5).await;

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
    let link = test_link(false);
    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
    b.push_node(cyclic_node(
        "telemetry".into(),
        downlink(&link, TelemetryMode::All),
    ));
    // A second producer (no inputs, so nothing else can fail first) after the downlink.
    b.push_node(cyclic_node("late".into(), Producer { n: 0.0 }));
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
    use metor_proto_wkt::{AlarmAck, ReloadSequences, SequenceCommand};

    let uplink = UplinkSystem::new()
        .with_msg::<SequenceCommand>()
        .with_msg::<ReloadSequences>()
        .with_msg::<AlarmAck>()
        // Idempotent, so a repeat is one port, not a duplicate registry key.
        .with_msg::<AlarmAck>();
    let desc = crate::CyclicSystem::instance_descriptor(&uplink);

    // The static shape (health, log) first, then one port per configured msg.
    assert_eq!(desc.outputs.len(), 5);
    let minted = &desc.outputs[2..];
    for d in minted {
        assert!(!d.telemetered, "inbound commands are never downlinked");
        assert_eq!(d.delivery, crate::Delivery::Log);
    }
    assert_eq!(minted[0].id(), crate::PortId::Packet(SequenceCommand::ID));
    assert_eq!(minted[0].name, "SequenceCommand");
    assert_eq!(minted[1].id(), crate::PortId::Packet(ReloadSequences::ID));
    assert_eq!(minted[2].id(), crate::PortId::Packet(AlarmAck::ID));
}

/// A late connection gets its own announce replay before data — and the
/// retained snapshot channels with it: the boot `SequenceRegistry` was
/// broadcast once at cycle 1, long before this connection existed, yet the
/// downlink's retention hands it to the newcomer.
#[cfg(not(miri))]
#[stellarator::test]
async fn late_connection_gets_replay_and_retained_snapshots() {
    use metor_proto_wkt::SequenceRegistry;

    let link = test_link(true);

    let mut b = crate::coordinator::init::InitGraph::new(sim_config());
    b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
    b.push_node(cyclic_node(
        "telemetry".into(),
        downlink(&link, TelemetryMode::All),
    ));
    let mut coord = b.build().unwrap();
    coord.run_for(10).await;

    // Connect only now, mid-mission (the coordinator has already run).
    let tap = WireTap::connect(&link).await;
    // The server still fans out nothing new (no cycles run), but the replay
    // arrives immediately on accept.
    tap.settle(|pkts| {
        pkts.iter()
            .any(|p| matches!(p, WirePkt::Msg { id, .. } if *id == SequenceRegistry::ID))
    })
    .await;
    let announces = tap.announces();
    assert!(
        announces
            .iter()
            .any(|(_, meta)| meta.iter().any(|m| m.name == "producer.imu.omega")),
        "the late connection decoded the replay"
    );
    // The retained boot registry decodes like a live record.
    let pkts = tap.pkts.borrow();
    let registry = pkts
        .iter()
        .find_map(|p| match p {
            WirePkt::Msg { id, payload } if *id == SequenceRegistry::ID => {
                postcard::from_bytes::<SequenceRegistry>(payload).ok()
            }
            _ => None,
        })
        .expect("the retained boot registry replayed");
    assert!(registry.channels.is_empty(), "no slots in this mission");
}

/// The identity packet leads every connection and advertises the uplink's
/// configured command set; a client's `GetDbInfo` probe is dropped at the
/// server's read side and never surfaces as `uplink_unroutable`.
#[cfg(not(miri))]
#[stellarator::test]
async fn identity_advertises_uplink_msgs_and_tolerates_probes() {
    use metor_proto::types::IntoLenPacket;
    use metor_proto_wkt::{AlarmAck, GetDbInfo, LinkInfo, SequenceCommand};
    use stellarator::io::{AsyncWrite, SplitExt};

    let link = test_link(true);
    let mut b = crate::coordinator::init::InitGraph::new(wall_config());
    b.push_node(cyclic_node(
        "uplink".into(),
        UplinkSystem::new()
            .attach(link.clone())
            .with_msg::<SequenceCommand>()
            .with_msg::<AlarmAck>(),
    ));
    b.push_node(cyclic_node("producer".into(), Producer { n: 0.0 }));
    b.push_node(cyclic_node(
        "telemetry".into(),
        downlink(&link, TelemetryMode::All),
    ));
    let mut coord = b.build().unwrap();

    let mut uplink_health = coord
        .registry()
        .get(ComponentId::new("uplink.health"))
        .expect("uplink.health")
        .view()
        .expect("reader slot");

    // Connect mid-run setup, read the identity, and probe like a unified
    // ground client would.
    let addr = link.get().local_addr();
    let probe = stellarator::spawn(async move {
        let stream = TcpStream::connect(addr).await.expect("connect");
        let (rx, tx) = stream.split();
        let mut packets = PacketStream::new(rx);
        let pkt = packets.next_grow(vec![0u8; 1024]).await.expect("identity");
        let OwnedPacket::Msg(m) = &pkt else {
            panic!("expected the identity push")
        };
        assert_eq!(m.id, LinkInfo::ID, "identity precedes the replay");
        let info: LinkInfo = postcard::from_bytes(&m.buf).expect("decodes");
        assert_eq!(
            info.command_ids,
            vec![SequenceCommand::ID, AlarmAck::ID],
            "the uplink's configured set, in config order"
        );
        tx.write_all((&GetDbInfo).into_len_packet().inner)
            .await
            .0
            .expect("probe");
    })
    .drop_guard();

    coord.run_for(100).await;
    drop(probe);

    let bytes = drain_latest(&mut uplink_health).expect("uplink health published");
    let h = SystemHealth::read_from_prefix(&bytes).expect("health").0;
    assert_eq!(h.errors, 0, "the probe never counted as unroutable");
}
