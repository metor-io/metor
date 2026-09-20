//! A running target's links: what a client sees on a `Publish`, and what a
//! `Subscribe` does with what it is sent.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use metor_fsw_3::coordinator::{
    CoordinatorConfig, InputConfig, OutputConfig, PortRef, SystemConfig, SystemTable,
};
use metor_fsw_3::link::register_builtins;
use metor_fsw_3::{Clock, Frame, Input, LinkStatus, Output, Timestamp, system};
use metor_proto::types::{IntoLenPacket, LenPacket, Msg, OwnedPacket};
use metor_proto_stellar::{PacketSink, PacketStream, Peer, identify};
use metor_proto_wkt::LogEvent;
use metor_proto_wkt::{LinkInfo, SetComponentMetadata, VTableMsg};
use serde::{Deserialize, Serialize};
use serde_json::json;
use stellarator::io::{AsyncWrite, OwnedReader, SplitExt};
use stellarator::net::TcpStream;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// How long a test waits for a target's thread before it fails.
const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(name = "imu")]
#[repr(C)]
struct Imu {
    #[frame(timestamp)]
    timestamp: Timestamp,
    sample: f64,
}

#[derive(
    metor_fsw_3::Record, metor_fsw_3::Schema, Serialize, Deserialize, Clone, Copy, Debug, PartialEq,
)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(max_len = 8)]
struct Ping {
    n: u32,
}

/// A frame whose vtable and metadata announce as several kilobytes.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(name = "wide")]
#[repr(C)]
struct Wide {
    #[frame(timestamp)]
    timestamp: Timestamp,
    samples: [f64; 256],
}

/// Publishes one wide frame per cycle.
#[derive(Default)]
struct WideSource;

#[system]
impl WideSource {
    fn execute(&mut self, now: Timestamp, wide: &mut Output<Wide>) {
        let _ = wide.write(&Wide {
            timestamp: now,
            samples: [1.0; 256],
        });
    }
}

/// Publishes one sample per cycle, counting up.
#[derive(Default)]
struct Source(i64);

#[system]
impl Source {
    fn execute(&mut self, imu: &mut Output<Imu>) {
        self.0 += 1;
        let _ = imu.write(&Imu {
            timestamp: Timestamp(self.0),
            sample: self.0 as f64,
        });
    }
}

/// Records every status its link publishes.
struct StatusSink(Arc<Mutex<Vec<LinkStatus>>>);

#[system]
impl StatusSink {
    fn execute(&mut self, link_status: &mut Input<LinkStatus>) {
        for status in link_status.drain().flatten() {
            // PANIC Safety: no test holds this lock across a panic.
            self.0.lock().expect("an unpoisoned sink").push(*status);
        }
    }
}

/// Publishes one ping per cycle, counting up.
#[derive(Default)]
struct Pinger(u32);

#[system]
impl Pinger {
    fn execute(&mut self, ping: &mut Output<Ping>) {
        self.0 += 1;
        let _ = ping.write(&Ping { n: self.0 });
    }
}

/// Records every ping it is wired to.
struct PingSink(Arc<Mutex<Vec<u32>>>);

#[system]
impl PingSink {
    fn execute(&mut self, ping: &mut Input<Ping>) {
        for ping in ping.drain().flatten() {
            // PANIC Safety: no test holds this lock across a panic.
            self.0.lock().expect("an unpoisoned sink").push(ping.n);
        }
    }
}

/// Records the fault kinds its link logs.
struct LogSink(Arc<Mutex<Vec<String>>>);

#[system]
impl LogSink {
    fn execute(&mut self, log: &mut Input<LogEvent>) {
        for event in log.drain().flatten() {
            for (_, value) in event.fields.iter().filter(|(name, _)| name == "kind") {
                // PANIC Safety: no test holds this lock across a panic.
                self.0
                    .lock()
                    .expect("an unpoisoned sink")
                    .push(value.to_string());
            }
        }
    }
}

/// What a test's systems report back to it.
#[derive(Clone, Default)]
struct Seen {
    statuses: Arc<Mutex<Vec<LinkStatus>>>,
    pings: Arc<Mutex<Vec<u32>>>,
    faults: Arc<Mutex<Vec<String>>>,
    cycles: Arc<AtomicU64>,
}

impl Seen {
    fn status(&self) -> Option<LinkStatus> {
        // PANIC Safety: no test holds this lock across a panic.
        self.statuses
            .lock()
            .expect("an unpoisoned sink")
            .last()
            .copied()
    }

    fn pings(&self) -> Vec<u32> {
        // PANIC Safety: as above.
        self.pings.lock().expect("an unpoisoned sink").clone()
    }

    fn faulted(&self, kind: &str) -> bool {
        // PANIC Safety: as above.
        let faults = self.faults.lock().expect("an unpoisoned sink");
        faults.iter().any(|fault| fault == kind)
    }

    fn cycles(&self) -> u64 {
        self.cycles.load(Ordering::Relaxed)
    }
}

/// The systems every test in this file may name, plus the built-in links.
fn table(seen: &Seen) -> SystemTable {
    let mut table = SystemTable::new();
    table
        .register("source", Source::default)
        .expect("valid records");
    table
        .register("wide", WideSource::default)
        .expect("valid records");
    table
        .register("pinger", Pinger::default)
        .expect("valid records");
    let statuses = seen.statuses.clone();
    table
        .register("status_sink", move || StatusSink(statuses.clone()))
        .expect("valid records");
    let pings = seen.pings.clone();
    table
        .register("ping_sink", move || PingSink(pings.clone()))
        .expect("valid records");
    let faults = seen.faults.clone();
    table
        .register("log_sink", move || LogSink(faults.clone()))
        .expect("valid records");
    register_builtins(&mut table).expect("valid records");
    table
}

/// A port nothing listens on, so a link may take it.
fn free_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.local_addr().expect("bound")
}

/// One system reading one port of another.
fn reading(id: &str, ty: &str, port: &str, from: PortRef) -> SystemConfig {
    SystemConfig {
        inputs: vec![InputConfig {
            port: port.into(),
            from: vec![from],
        }],
        ..SystemConfig::new(id, ty)
    }
}

/// A `Publish` over `transport`, serving `imu.imu`.
fn publish(transport: serde_json::Value, pending_cap: usize) -> SystemConfig {
    SystemConfig {
        params: json!({
            "transport": transport,
            "namespace": "cube_sat",
            "link": "pub",
            "pending_cap": pending_cap,
        }),
        ..reading("pub", "fsw.publish", "imu.imu", PortRef::new("imu", "imu"))
    }
}

/// A target on its own thread, stopped and joined when the handle drops.
struct Target {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Target {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Builds and runs `systems` on a thread of its own, cycling at `rate`.
fn spawn(systems: Vec<SystemConfig>, seen: &Seen, rate: f64) -> Target {
    spawn_on(systems, seen, Clock::Wall { rate })
}

/// Builds and runs `systems` on a thread of its own, under `clock`.
fn spawn_on(systems: Vec<SystemConfig>, seen: &Seen, clock: Clock) -> Target {
    let stop = Arc::new(AtomicBool::new(false));
    let (flag, seen) = (stop.clone(), seen.clone());
    let cycles = seen.cycles.clone();
    let thread = std::thread::spawn(move || {
        let config = CoordinatorConfig {
            clock,
            systems,
            ..Default::default()
        };
        let mut coordinator = config.build(&table(&seen)).expect("valid config");
        stellarator::run(|| async move { coordinator.run(Until(flag, cycles)).await });
    });
    Target {
        stop,
        thread: Some(thread),
    }
}

/// Ready once the target's handle asks it to stop, counting the cycles until then.
struct Until(Arc<AtomicBool>, Arc<AtomicU64>);

impl Future for Until {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        self.1.fetch_add(1, Ordering::Relaxed);
        match self.0.load(Ordering::Acquire) {
            true => Poll::Ready(()),
            false => Poll::Pending,
        }
    }
}

/// Dials until the target's listener is up.
async fn dial(addr: SocketAddr) -> Peer {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match identify(addr).await {
            Ok(peer) => return peer,
            Err(e) => assert!(Instant::now() < deadline, "never came up: {e}"),
        }
        stellarator::sleep(Duration::from_millis(5)).await;
    }
}

/// Connects until the target's listener is up, without a handshake.
async fn raw_dial(addr: SocketAddr) -> TcpStream {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => return stream,
            Err(e) => assert!(Instant::now() < deadline, "never came up: {e}"),
        }
        stellarator::sleep(Duration::from_millis(5)).await;
    }
}

/// The next packet, growing the buffer the previous one left.
async fn next(
    rx: &mut PacketStream<OwnedReader<TcpStream>>,
    buf: &mut Vec<u8>,
) -> OwnedPacket<stellarator::buf::Slice<Vec<u8>>> {
    let taken = core::mem::take(buf);
    let packet = rx.next_grow(taken).await.expect("the link stays up");
    *buf = vec![0u8; 1024];
    packet
}

/// Waits until `done`, failing the test at the deadline.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + DEADLINE;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        stellarator::sleep(Duration::from_millis(2)).await;
    }
}

#[stellarator::test]
async fn a_listening_publish_announces_itself_then_streams_its_records() {
    let addr = free_port();
    let seen = Seen::default();
    let _target = spawn(
        vec![
            SystemConfig::new("imu", "source"),
            publish(json!({ "listen": { "addr": addr.to_string() } }), 1 << 20),
        ],
        &seen,
        500.0,
    );

    let Peer::Fsw {
        info, mut rx, buf, ..
    } = dial(addr).await
    else {
        panic!("a publish is an fsw link")
    };
    assert_eq!(info.protocol_version, 2);
    assert_eq!(info.namespace.as_deref(), Some("cube_sat"));
    assert_eq!(
        (info.link.as_str(), info.command_ids.as_slice()),
        ("pub", &[][..])
    );

    let mut buf = buf;
    let table_id = match next(&mut rx, &mut buf).await {
        OwnedPacket::Msg(m) if m.id == VTableMsg::ID => {
            m.parse::<VTableMsg>().expect("a vtable announce").id
        }
        _ => panic!("the vtable announces first"),
    };
    match next(&mut rx, &mut buf).await {
        OwnedPacket::Msg(m) if m.id == SetComponentMetadata::ID => {
            let announce = m.parse::<SetComponentMetadata>().expect("a component");
            assert_eq!(announce.0.name, "cube_sat.imu.imu.sample");
        }
        _ => panic!("the components announce after the vtable"),
    }

    let mut stamps = Vec::new();
    while stamps.len() < 3 {
        if let OwnedPacket::Table(t) = next(&mut rx, &mut buf).await {
            assert_eq!(t.id, table_id);
            let imu = Imu::read_from_bytes(&t.buf).expect("a frame");
            stamps.push(imu.timestamp.0);
        }
    }
    let steps: Vec<i64> = stamps.windows(2).map(|w| w[1] - w[0]).collect();
    assert_eq!(steps, vec![1, 1], "{stamps:?}");
}

#[stellarator::test]
async fn a_dialing_publish_finds_a_listener_that_comes_up_late() {
    let addr = free_port();
    let seen = Seen::default();
    let _target = spawn(
        vec![
            SystemConfig::new("imu", "source"),
            publish(json!({ "connect": { "addr": addr.to_string() } }), 1 << 20),
        ],
        &seen,
        500.0,
    );
    // The link dials while nothing answers, so it backs off and retries.
    stellarator::sleep(Duration::from_millis(50)).await;
    let listener = stellarator::net::TcpListener::bind(addr).expect("the port is still free");

    let (rx, _tx) = listener.accept().await.expect("the link dials").split();
    let mut rx = PacketStream::new(rx);
    let mut buf = vec![0u8; 1024];
    let mut seen = Vec::new();
    while seen.len() < 4 {
        match next(&mut rx, &mut buf).await {
            OwnedPacket::Msg(m) => seen.push(m.id),
            OwnedPacket::Table(_) => seen.push([0, 0]),
            _ => {}
        }
    }
    assert_eq!(
        &seen[..3],
        &[
            metor_proto_wkt::LinkInfo::ID,
            VTableMsg::ID,
            SetComponentMetadata::ID
        ]
    );
    assert_eq!(seen[3], [0, 0], "records follow the announce");
}

#[stellarator::test]
async fn a_client_that_never_reads_drops_its_own_batches_only() {
    let addr = free_port();
    let seen = Seen::default();
    let _target = spawn(
        vec![
            SystemConfig::new("imu", "source"),
            publish(json!({ "listen": { "addr": addr.to_string() } }), 256),
            reading(
                "status",
                "status_sink",
                "link_status",
                PortRef::new("pub", "link_status"),
            ),
        ],
        &seen,
        50_000.0,
    );

    let _slow = match dial(addr).await {
        Peer::Fsw { rx, tx, .. } => (rx, tx),
        _ => panic!("a publish is an fsw link"),
    };
    let Peer::Fsw { mut rx, buf, .. } = dial(addr).await else {
        panic!("a publish is an fsw link")
    };

    let mut buf = buf;
    let mut tables = 0;
    while tables < 3 {
        if let OwnedPacket::Table(_) = next(&mut rx, &mut buf).await {
            tables += 1;
        }
    }

    until("the link never reported two connections", || {
        seen.status()
            .is_some_and(|s| s.connections == 2 && s.batches_dropped > 0)
    })
    .await;
    assert_eq!(seen.status().expect("a status").connections, 2);
}

/// A `Subscribe` over `transport`, publishing what it is sent on `ping`.
fn subscribe(transport: serde_json::Value) -> SystemConfig {
    SystemConfig {
        params: json!({
            "transport": transport,
            "namespace": "cube_sat",
            "link": "cmds",
        }),
        outputs: vec![OutputConfig {
            port: "ping".into(),
            record: "ping".into(),
        }],
        ..SystemConfig::new("cmds", "fsw.subscribe")
    }
}

#[stellarator::test]
async fn an_idle_subscribe_reports_its_completed_identity_write_once() {
    let addr = free_port();
    let seen = Seen::default();
    let _target = spawn(
        vec![
            subscribe(json!({ "listen": { "addr": addr.to_string() } })),
            reading(
                "status",
                "status_sink",
                "link_status",
                PortRef::new("cmds", "link_status"),
            ),
        ],
        &seen,
        500.0,
    );

    let peer = raw_dial(addr).await;
    let (rx, _tx) = peer.split();
    let mut rx = PacketStream::new(rx);
    let packet = next(&mut rx, &mut vec![0u8; 1024]).await;
    let OwnedPacket::Msg(message) = packet else {
        panic!("the first packet is the link identity");
    };
    let info = message.parse::<LinkInfo>().expect("a link identity");
    let bytes_out = (&info).into_len_packet().inner.len() as u64;
    until(
        "the idle subscriber never reported its identity bytes",
        || {
            seen.status()
                .is_some_and(|status| status.connections == 1 && status.bytes_out == bytes_out)
        },
    )
    .await;

    let reported = seen.status();
    stellarator::sleep(Duration::from_millis(30)).await;
    assert_eq!(seen.status(), reported, "idle status is not repeated");
}

#[stellarator::test]
async fn a_listening_subscribe_names_its_commands_and_routes_what_it_is_sent() {
    let addr = free_port();
    let seen = Seen::default();
    let _target = spawn(
        vec![
            subscribe(json!({ "listen": { "addr": addr.to_string() } })),
            reading("sink", "ping_sink", "ping", PortRef::new("cmds", "ping")),
            reading(
                "status",
                "status_sink",
                "link_status",
                PortRef::new("cmds", "link_status"),
            ),
        ],
        &seen,
        500.0,
    );

    let Peer::Fsw { info, tx, .. } = dial(addr).await else {
        panic!("a subscribe is an fsw link")
    };
    assert_eq!(info.command_ids, vec![<Ping as Msg>::ID]);
    assert_eq!(info.link, "cmds");

    tx.send((&Ping { n: 7 }).into_len_packet())
        .await
        .0
        .expect("the link takes commands");
    until("the ping never reached the consumer", || {
        seen.pings().contains(&7)
    })
    .await;

    // A table is not routed in this slice, and the connection stays up.
    tx.send(LenPacket::table([1, 2], 8)).await.0.expect("up");
    until("the table was never counted", || {
        seen.status().is_some_and(|s| s.inbound_dropped > 0)
    })
    .await;
    tx.send((&Ping { n: 9 }).into_len_packet())
        .await
        .0
        .expect("the link is still up");
    until("the link stopped routing after a table", || {
        seen.pings().contains(&9)
    })
    .await;
}

#[stellarator::test]
async fn a_dialing_subscribe_receives_the_records_a_publish_serves() {
    let addr = free_port();
    let (server, client) = (Seen::default(), Seen::default());
    let _server = spawn(
        vec![
            SystemConfig::new("src", "pinger"),
            SystemConfig {
                params: json!({
                    "transport": { "listen": { "addr": addr.to_string() } },
                    "link": "pub",
                }),
                ..reading(
                    "pub",
                    "fsw.publish",
                    "src.ping",
                    PortRef::new("src", "ping"),
                )
            },
        ],
        &server,
        500.0,
    );
    let _client = spawn(
        vec![
            subscribe(json!({ "connect": { "addr": addr.to_string() } })),
            reading("sink", "ping_sink", "ping", PortRef::new("cmds", "ping")),
        ],
        &client,
        500.0,
    );

    until("the subscriber never received a record", || {
        client.pings().len() >= 3
    })
    .await;
    let pings = client.pings();
    let steps: Vec<u32> = pings.windows(2).map(|w| w[1] - w[0]).collect();
    assert!(steps.iter().all(|step| *step == 1), "{pings:?}");
}

#[stellarator::test]
async fn a_dialing_subscribe_reads_past_an_announce_larger_than_its_records() {
    let addr = free_port();
    let (server, client) = (Seen::default(), Seen::default());
    let mut publish = reading(
        "pub",
        "fsw.publish",
        "src.ping",
        PortRef::new("src", "ping"),
    );
    publish.inputs.push(InputConfig {
        port: "wide.wide".into(),
        from: vec![PortRef::new("wide", "wide")],
    });
    publish.params = json!({
        "transport": { "listen": { "addr": addr.to_string() } },
        "link": "pub",
    });
    let _server = spawn(
        vec![
            SystemConfig::new("src", "pinger"),
            SystemConfig::new("wide", "wide"),
            publish,
        ],
        &server,
        500.0,
    );
    let _client = spawn(
        vec![
            subscribe(json!({ "connect": { "addr": addr.to_string() } })),
            reading("sink", "ping_sink", "ping", PortRef::new("cmds", "ping")),
        ],
        &client,
        500.0,
    );

    until("the subscriber never got past the announce", || {
        client.pings().len() >= 3
    })
    .await;
}

#[stellarator::test]
async fn a_length_prefix_past_the_receive_cap_closes_the_connection() {
    let addr = free_port();
    let seen = Seen::default();
    let _target = spawn(
        vec![
            SystemConfig::new("imu", "source"),
            publish(json!({ "listen": { "addr": addr.to_string() } }), 1 << 20),
            reading(
                "status",
                "status_sink",
                "link_status",
                PortRef::new("pub", "link_status"),
            ),
        ],
        &seen,
        500.0,
    );

    let peer = raw_dial(addr).await;
    until("the link never took the connection", || {
        seen.status().is_some_and(|s| s.connections == 1)
    })
    .await;

    // Four gigabytes the link must refuse rather than allocate.
    let (written, _) = peer.write_all(u32::MAX.to_le_bytes().to_vec()).await;
    written.expect("the link takes the prefix");
    until("the link kept a connection it cannot serve", || {
        seen.status().is_some_and(|s| s.connections == 0)
    })
    .await;
}

#[stellarator::test]
async fn a_peer_that_hangs_up_frees_the_only_slot_for_the_next_one() {
    let addr = free_port();
    let seen = Seen::default();
    let _target = spawn(
        vec![
            subscribe(json!({ "listen": { "addr": addr.to_string(), "max_connections": 1 } })),
            reading("sink", "ping_sink", "ping", PortRef::new("cmds", "ping")),
        ],
        &seen,
        500.0,
    );

    // A peer that takes the slot and leaves without ever sending a command.
    let Peer::Fsw { rx, tx, .. } = dial(addr).await else {
        panic!("a subscribe is an fsw link")
    };
    drop((rx, tx));

    // Nothing else stirs the link, so only the hang-up can free the slot.
    let Peer::Fsw { tx, .. } = dial(addr).await else {
        panic!("a subscribe is an fsw link")
    };
    tx.send((&Ping { n: 3 }).into_len_packet())
        .await
        .0
        .expect("the link takes commands");
    until("the second peer never reached the consumer", || {
        seen.pings().contains(&3)
    })
    .await;
}

#[stellarator::test]
async fn a_dialing_subscribe_redials_a_peer_that_comes_back() {
    let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let addr = listener.local_addr().expect("bound");
    let seen = Seen::default();
    let _target = spawn(
        vec![
            subscribe(json!({ "connect": { "addr": addr.to_string() } })),
            reading("sink", "ping_sink", "ping", PortRef::new("cmds", "ping")),
        ],
        &seen,
        500.0,
    );

    drop(listener.accept().await.expect("the link dials"));
    drop(listener);

    // The peer comes back on the same address; only a redial reaches it.
    let listener = stellarator::net::TcpListener::bind(addr).expect("the peer's address");
    let peer = futures_lite::future::or(async { listener.accept().await.ok() }, async {
        stellarator::sleep(DEADLINE).await;
        None
    })
    .await
    .expect("the link never redialed");
    let (_rx, tx) = peer.split();
    PacketSink::new(tx)
        .send((&Ping { n: 5 }).into_len_packet())
        .await
        .0
        .expect("the redialed link takes commands");
    until("the redialed link never routed a command", || {
        seen.pings().contains(&5)
    })
    .await;
}

/// A `Publish` dialing `addr`, serving `imu.imu` and logging onto `pub.log`.
fn dialing_publish(addr: SocketAddr) -> Vec<SystemConfig> {
    vec![
        SystemConfig::new("imu", "source"),
        publish(json!({ "connect": { "addr": addr.to_string() } }), 1 << 20),
        reading("log", "log_sink", "log", PortRef::new("pub", "log")),
    ]
}

#[stellarator::test]
async fn a_simulated_clock_outruns_its_link_without_losing_the_peer() {
    // The peer listens before the target exists, so its connection predates
    // the first cycle.
    let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let addr = listener.local_addr().expect("bound");
    let seen = Seen::default();
    let _target = spawn_on(
        dialing_publish(addr),
        &seen,
        Clock::Simulated {
            dt: Duration::from_millis(1),
        },
    );

    let (rx, _tx) = listener.accept().await.expect("the link dials").split();
    let mut rx = PacketStream::new(rx);
    let mut buf = vec![0u8; 1024];
    let mut announced = Vec::new();
    let mut records = 0;
    while records < 3 {
        match next(&mut rx, &mut buf).await {
            OwnedPacket::Msg(m) => announced.push(m.id),
            OwnedPacket::Table(_) => records += 1,
            _ => {}
        }
    }
    assert_eq!(
        &announced[..3],
        &[
            metor_proto_wkt::LinkInfo::ID,
            VTableMsg::ID,
            SetComponentMetadata::ID
        ]
    );

    until("the cycles never outran a wall clock", || {
        seen.cycles() > 50_000
    })
    .await;
    until("the link's mirror never overflowed", || {
        seen.faulted("mirror_dropped")
    })
    .await;
}

/// How many cycles the fixture target runs before it exits on its own; the test
/// kills it long before that.
const FIXTURE_CYCLES: &str = "600000";

/// The fixture's target under `metor run`, killed and reaped when it drops.
struct Fixture {
    child: Child,
    ports: Vec<(String, SocketAddr)>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Fixture {
    /// Runs the fixture target with `--print-ports` and reads what it bound.
    fn start() -> Self {
        let target =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-pack/target.py");
        let mut child = Command::new(env!("CARGO_BIN_EXE_metor"))
            .args([
                "run",
                &target.display().to_string(),
                "--cycles",
                FIXTURE_CYCLES,
                "--print-ports",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .expect("metor runs");
        // PANIC Safety: the child was just spawned with a piped stdout.
        let stdout = child.stdout.take().expect("a piped stdout");
        let ports = BufReader::new(stdout)
            .lines()
            .take(2)
            .map_while(Result::ok)
            .filter_map(|line| {
                let (link, addr) = line.split_once(' ')?;
                Some((link.to_string(), addr.parse().ok()?))
            })
            .collect();
        Self { child, ports }
    }

    fn addr(&self, link: &str) -> SocketAddr {
        let found = self.ports.iter().find(|(name, _)| name == link);
        // PANIC Safety: a target that did not bind both links fails the test.
        found.expect("the target printed both links").1
    }
}

#[stellarator::test]
async fn the_fixture_answers_over_its_links_what_it_was_sent() {
    let fixture = Fixture::start();
    // The publisher takes the connection first, so the answer cannot precede it.
    let Peer::Fsw { mut rx, buf, .. } = dial(fixture.addr("pub")).await else {
        panic!("a publish is an fsw link")
    };
    let Peer::Fsw { info, tx, .. } = dial(fixture.addr("cmds")).await else {
        panic!("a subscribe is an fsw link")
    };
    assert_eq!(info.command_ids, vec![<Ping as Msg>::ID]);

    tx.send((&Ping { n: 41 }).into_len_packet())
        .await
        .0
        .expect("the link takes commands");

    let mut buf = buf;
    let echoed = futures_lite::future::or(
        async {
            loop {
                if let OwnedPacket::Msg(m) = next(&mut rx, &mut buf).await
                    && m.id == <Ping as Msg>::ID
                {
                    return Some(m.parse::<Ping>().expect("a ping").n);
                }
            }
        },
        async {
            stellarator::sleep(DEADLINE).await;
            None
        },
    )
    .await;
    assert_eq!(echoed, Some(41), "the ping never came back");
}
