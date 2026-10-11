//! Tests the links of a running target: what a client receives from a
//! `Publish`, and what a `Subscribe` does with the packets it receives.

mod common;

use core::time::Duration;
use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use common::{
    Deadline, Fsw, Imu, Ping, Running, Source, Target, dial_fsw, fixture_dir, metor, source, within,
};
use metor_fsw_3::coordinator::{
    CoordinatorConfig, InputConfig, OutputConfig, OutputSource, PortRef, SystemConfig, SystemTable,
};
use metor_fsw_3::link::register_builtins;
use metor_fsw_3::{Clock, DefCx, Frame, Input, LinkStatus, Output, Timestamp, system};
use metor_proto::types::{IntoLenPacket, LenPacket, Msg, OwnedPacket};
use metor_proto_stellar::PacketStream;
use metor_proto_wkt::LogEvent;
use metor_proto_wkt::{LinkInfo, SetComponentMetadata, VTableMsg};
use serde_json::json;
use stellarator::io::{AsyncWrite, OwnedReader, SplitExt};
use stellarator::net::TcpStream;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// A frame whose announce (vtable and metadata) is several kilobytes long.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(name = "wide")]
#[repr(C)]
struct Wide {
    #[frame(timestamp)]
    timestamp: Timestamp,
    samples: [f64; 256],
}

/// Publishes one wide frame per cycle.
#[system]
fn wide_source(now: Timestamp, wide: &mut Output<Wide>) {
    let _ = wide.write(&Wide {
        timestamp: now,
        samples: [1.0; 256],
    });
}

/// Records every status its link writes.
struct StatusSink(Arc<Mutex<Vec<LinkStatus>>>);

#[system]
fn status_sink(#[state] s: &mut StatusSink, link_status: &mut Input<LinkStatus>) {
    for status in link_status.drain().flatten() {
        // PANIC Safety: no test holds this lock across a panic.
        s.0.lock().expect("an unpoisoned sink").push(*status);
    }
}

/// Publishes one ping per cycle, counting up.
#[derive(Default)]
struct Pinger(u32);

#[system]
fn pinger(#[state] s: &mut Pinger, ping: &mut Output<Ping>) {
    s.0 += 1;
    let _ = ping.write(&Ping { n: s.0 });
}

/// Records the sample of every frame it receives, as a whole number. The
/// samples go in the same list as the pings.
struct ImuSink(Arc<Mutex<Vec<u32>>>);

#[system]
fn imu_sink(#[state] s: &mut ImuSink, imu: &mut Input<Imu>) {
    for imu in imu.drain().flatten() {
        // PANIC Safety: no test holds this lock across a panic.
        s.0.lock()
            .expect("an unpoisoned sink")
            .push(imu.sample as u32);
    }
}

/// Records every ping it receives.
struct PingSink(Arc<Mutex<Vec<u32>>>);

#[system]
fn ping_sink(#[state] s: &mut PingSink, ping: &mut Input<Ping>) {
    for ping in ping.drain().flatten() {
        // PANIC Safety: no test holds this lock across a panic.
        s.0.lock().expect("an unpoisoned sink").push(ping.n);
    }
}

/// Records the `kind` of every log line it receives.
struct LogSink(Arc<Mutex<Vec<String>>>);

#[system]
fn log_sink(#[state] s: &mut LogSink, log: &mut Input<LogEvent>) {
    for event in log.drain().flatten() {
        let kind = event.fields.iter().find(|(name, _)| name == "kind");
        let line = match kind {
            Some((_, kind)) => kind.to_string(),
            None => format!("line: {}", event.message),
        };
        // PANIC Safety: no test holds this lock across a panic.
        s.0.lock().expect("an unpoisoned sink").push(line);
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

/// Builds the system table for these tests: the test systems plus the
/// built-in links.
fn table(seen: &Seen) -> SystemTable {
    let mut table = SystemTable::new();
    table
        .register("source", source(), Source::default)
        .expect("valid records");
    table
        .register("wide", wide_source(), Default::default)
        .expect("valid records");
    table
        .register("pinger", pinger(), Pinger::default)
        .expect("valid records");
    let statuses = seen.statuses.clone();
    table
        .register("status_sink", status_sink(), move || {
            StatusSink(statuses.clone())
        })
        .expect("valid records");
    let pings = seen.pings.clone();
    table
        .register("ping_sink", ping_sink(), move || PingSink(pings.clone()))
        .expect("valid records");
    let samples = seen.pings.clone();
    table
        .register("imu_sink", imu_sink(), move || ImuSink(samples.clone()))
        .expect("valid records");
    let faults = seen.faults.clone();
    table
        .register("log_sink", log_sink(), move || LogSink(faults.clone()))
        .expect("valid records");
    register_builtins(&mut table).expect("valid records");
    table
}

/// Returns the config for system `id`, reading `from` on its `port` input.
fn reading(id: &str, ty: &str, port: &str, from: PortRef) -> SystemConfig {
    SystemConfig {
        inputs: vec![InputConfig {
            port: port.into(),
            from: vec![from],
        }],
        ..SystemConfig::new(id, ty)
    }
}

/// Returns a `Publish` named `link` on `transport` that serves `imu.imu`.
fn publish(link: &str, transport: serde_json::Value, conn_cap: usize) -> SystemConfig {
    SystemConfig {
        params: json!({
            "transport": transport,
            "namespace": "cube_sat",
            "link": link,
            "conn_cap": conn_cap,
        }),
        ..reading("pub", "fsw.publish", "imu.imu", PortRef::new("imu", "imu"))
    }
}

/// Returns a transport that listens on a port the OS picks.
fn listen_any() -> serde_json::Value {
    json!({ "listen": { "addr": "127.0.0.1:0" } })
}

/// Returns an `imu` source and a `Publish` named `link` that serves it on a
/// port the OS picks.
fn imu_publisher(link: &str) -> Vec<SystemConfig> {
    vec![
        SystemConfig::new("imu", "source"),
        publish(link, listen_any(), 1 << 20),
    ]
}

/// Returns a status sink that reads the link system `link`.
fn status_reader(link: &str) -> SystemConfig {
    reading(
        "status",
        "status_sink",
        "link_status",
        PortRef::new(link, "link_status"),
    )
}

/// Builds `systems` and runs them on a new thread, cycling at `rate`.
fn spawn(systems: Vec<SystemConfig>, seen: &Seen, rate: f64) -> Target {
    spawn_on(systems, seen, Clock::Wall { rate })
}

/// Builds `systems` and runs them on a new thread with `clock`.
fn spawn_on(systems: Vec<SystemConfig>, seen: &Seen, clock: Clock) -> Target {
    let config = CoordinatorConfig {
        clock,
        namespace: Some("cube_sat".into()),
        systems,
        ..Default::default()
    };
    let seen = seen.clone();
    let cycles = seen.cycles.clone();
    Target::spawn(config, move || table(&seen), cycles)
}

/// Waits for the link named `link` to bind, and returns its address.
///
/// The test links bind port zero, so tests never have to pick a port.
async fn bound(link: &str) -> SocketAddr {
    let deadline = Deadline::new();
    loop {
        let found = metor_fsw_3::link::bound_ports()
            .into_iter()
            .rev()
            .find(|(name, _)| name == link)
            .map(|(_, addr)| addr);
        if let Some(addr) = found {
            return addr;
        }
        deadline.tick(format!("link `{link}` never bound")).await;
    }
}

/// Connects to `addr` once the listener is up, without reading its
/// handshake.
async fn raw_dial(addr: SocketAddr) -> TcpStream {
    let deadline = Deadline::new();
    loop {
        match TcpStream::connect(addr).await {
            Ok(stream) => return stream,
            Err(e) => deadline.tick(format!("never came up: {e}")).await,
        }
    }
}

/// Reads the next packet, growing the buffer if needed.
async fn next(
    rx: &mut PacketStream<OwnedReader<TcpStream>>,
    buf: &mut Vec<u8>,
) -> OwnedPacket<stellarator::buf::Slice<Vec<u8>>> {
    let taken = core::mem::take(buf);
    let packet = match rx.next_grow(taken).await {
        Ok(packet) => packet,
        Err(error) => panic!("the link stays up: {error:?}"),
    };
    *buf = vec![0u8; 1024];
    packet
}

/// Waits until `done` returns true, failing the test at the deadline.
async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Deadline::new();
    while !done() {
        deadline.tick(what).await;
    }
}

/// Like [`until`], but the failure message includes the target's last
/// status and faults.
async fn until_seen(what: &str, seen: &Seen, mut done: impl FnMut() -> bool) {
    let deadline = Deadline::new();
    while !done() {
        let faults = seen.faults.lock().expect("an unpoisoned sink").clone();
        let (status, cycles) = (seen.status(), seen.cycles());
        let what = format!("{what}; last status {status:?}, faults {faults:?}, cycles {cycles}");
        deadline.tick(what).await;
    }
}

#[stellarator::test]
async fn test_publish_announces_and_streams() {
    let seen = Seen::default();
    let _target = spawn(imu_publisher("pub-announces"), &seen, 500.0);
    let addr = bound("pub-announces").await;

    let Fsw {
        info, mut rx, buf, ..
    } = dial_fsw(addr).await;
    assert_eq!(info.protocol_version, 2);
    assert_eq!(info.namespace.as_deref(), Some("cube_sat"));
    assert_eq!(
        (info.link.as_str(), info.command_ids.as_slice()),
        ("pub-announces", &[][..])
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
async fn test_publish_retries_connection() {
    let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    let addr = listener.local_addr().expect("bound");
    let seen = Seen::default();
    let _target = spawn(
        vec![
            SystemConfig::new("imu", "source"),
            publish(
                "pub-retries",
                json!({ "connect": { "addr": addr.to_string() } }),
                1 << 20,
            ),
        ],
        &seen,
        500.0,
    );
    // Hang up on the first connection. The link backs off and dials again,
    // and the second connection carries the announce.
    drop(listener.accept().await.expect("the link dials"));

    let (rx, _tx) = listener.accept().await.expect("the link redials").split();
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
async fn test_slow_client_isolation() {
    let seen = Seen::default();
    let _target = spawn(
        vec![
            SystemConfig::new("imu", "source"),
            publish("pub-slow", listen_any(), 256),
            status_reader("pub"),
        ],
        &seen,
        50_000.0,
    );
    let addr = bound("pub-slow").await;

    let _slow = dial_fsw(addr).await;
    let Fsw { mut rx, buf, .. } = dial_fsw(addr).await;

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

/// Returns a `Subscribe` named `link` on `transport`, which writes the pings
/// it receives to its `ping` output.
fn subscribe(link: &str, transport: serde_json::Value) -> SystemConfig {
    SystemConfig {
        params: json!({
            "transport": transport,
            "namespace": "cube_sat",
            "link": link,
        }),
        outputs: vec![OutputConfig::record("ping", "ping")],
        ..SystemConfig::new("cmds", "fsw.subscribe")
    }
}

#[stellarator::test]
async fn test_subscribe_counts_identity_write_once() {
    let seen = Seen::default();
    let _target = spawn(
        vec![subscribe("cmds-idle", listen_any()), status_reader("cmds")],
        &seen,
        500.0,
    );
    let addr = bound("cmds-idle").await;

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
async fn test_subscribe_announces_and_routes() {
    let seen = Seen::default();
    let _target = spawn(
        vec![
            subscribe("cmds-routes", listen_any()),
            reading("sink", "ping_sink", "ping", PortRef::new("cmds", "ping")),
            status_reader("cmds"),
        ],
        &seen,
        500.0,
    );
    let addr = bound("cmds-routes").await;

    let Fsw { info, tx, .. } = dial_fsw(addr).await;
    assert_eq!(info.command_ids, vec![<Ping as Msg>::ID]);
    assert_eq!(info.link, "cmds-routes");

    tx.send((&Ping { n: 7 }).into_len_packet())
        .await
        .0
        .expect("the link takes commands");
    until("the ping never reached the consumer", || {
        seen.pings().contains(&7)
    })
    .await;

    // A table no port takes is ignored: it is not routed or counted as
    // dropped, and the connection stays up.
    tx.send(LenPacket::table([1, 2], 8)).await.0.expect("up");
    tx.send((&Ping { n: 9 }).into_len_packet())
        .await
        .0
        .expect("the link is still up");
    until("the link stopped routing after a table", || {
        seen.pings().contains(&9)
    })
    .await;
    assert_eq!(seen.status().map(|s| s.inbound_dropped), Some(0));
}

#[stellarator::test]
async fn test_subscribe_receives_records() {
    let (server, client) = (Seen::default(), Seen::default());
    let _server = spawn(
        vec![
            SystemConfig::new("src", "pinger"),
            SystemConfig {
                params: json!({
                    "transport": listen_any(),
                    "link": "pub-records",
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
    let addr = bound("pub-records").await;
    let _client = spawn(
        vec![
            subscribe(
                "cmds-records",
                json!({ "connect": { "addr": addr.to_string() } }),
            ),
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

/// Returns a `Subscribe` that dials `addr`, plus sinks for its frames and
/// faults.
///
/// Its `imu` output uses `def`, the way `metor build` resolves a peer's port.
fn mirror(addr: SocketAddr, def: metor_fsw_3::PortDef) -> Vec<SystemConfig> {
    vec![
        SystemConfig {
            params: json!({
                "transport": { "connect": { "addr": addr.to_string() } },
                "link": "mirror",
            }),
            outputs: vec![OutputConfig {
                port: "imu".into(),
                source: OutputSource::Def(def),
            }],
            ..SystemConfig::new("cmds", "fsw.subscribe")
        },
        reading("sink", "imu_sink", "imu", PortRef::new("cmds", "imu")),
        reading("faults", "log_sink", "log", PortRef::new("cmds", "log")),
    ]
}

/// Returns the definition `imu.imu` announces on a `cube_sat` target, renamed
/// to the subscriber's `imu` port.
fn published_imu() -> metor_fsw_3::PortDef {
    let cx = DefCx {
        id: "imu",
        namespace: Some("cube_sat"),
        ..DefCx::empty()
    };
    metor_fsw_3::PortDef {
        name: "imu".into(),
        ..Output::<Imu>::def(&cx, "imu")
    }
}

#[stellarator::test]
async fn test_subscribe_mirrors_frames() {
    let (server, client) = (Seen::default(), Seen::default());
    let _server = spawn(imu_publisher("pub-frames"), &server, 500.0);
    let addr = bound("pub-frames").await;
    let _client = spawn(mirror(addr, published_imu()), &client, 500.0);
    until("the mirror never received a frame", || {
        client.pings().len() >= 3
    })
    .await;
    let samples = client.pings();
    assert!(samples.windows(2).all(|w| w[1] == w[0] + 1), "{samples:?}");
    assert!(!client.faulted("peer_schema_mismatch"));
}

/// A target can dial a peer and listen for the ground on the same thread.
#[stellarator::test]
async fn test_dialing_beside_a_listener() {
    let (server, client) = (Seen::default(), Seen::default());
    let _server = spawn(imu_publisher("pub-beside"), &server, 500.0);
    let addr = bound("pub-beside").await;
    let mut systems = mirror(addr, published_imu());
    systems.push(SystemConfig {
        params: json!({
            "transport": listen_any(),
            "link": "ground-beside",
        }),
        ..reading(
            "ground",
            "fsw.publish",
            "cmds.imu",
            PortRef::new("cmds", "imu"),
        )
    });
    let _client = spawn(systems, &client, 500.0);
    until_seen("the mirror never received a frame", &client, || {
        client.pings().len() >= 3
    })
    .await;
}

#[stellarator::test]
async fn test_subscribe_faults_unannounced_table() {
    let (server, client) = (Seen::default(), Seen::default());
    let _server = spawn(imu_publisher("pub-other"), &server, 500.0);
    let addr = bound("pub-other").await;
    // The same frame on a different target has a different table id.
    let elsewhere = DefCx {
        id: "imu",
        namespace: Some("other"),
        ..DefCx::empty()
    };
    let stale = metor_fsw_3::PortDef {
        name: "imu".into(),
        ..Output::<Imu>::def(&elsewhere, "imu")
    };
    let _client = spawn(mirror(addr, stale), &client, 500.0);
    until_seen("the mirror never faulted", &client, || {
        client.faulted("peer_schema_mismatch")
    })
    .await;
    assert!(client.pings().is_empty(), "nothing routes under a stale id");
}

/// Returns a free port, so two targets can use it one after the other.
fn free_port() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.local_addr().expect("its address")
}

#[stellarator::test]
async fn test_reconnect_checks_the_new_peer() {
    let (server, client) = (Seen::default(), Seen::default());
    let addr = free_port();
    let first = spawn(
        vec![
            SystemConfig::new("imu", "source"),
            publish(
                "pub-again",
                json!({ "listen": { "addr": addr.to_string() } }),
                1 << 20,
            ),
        ],
        &server,
        500.0,
    );
    let _client = spawn(mirror(addr, published_imu()), &client, 500.0);
    until_seen("the mirror never received a frame", &client, || {
        client.pings().len() >= 3
    })
    .await;
    assert!(!client.faulted("peer_schema_mismatch"));
    drop(first);

    // A new target on the same address, serving a frame the subscriber's port
    // does not take.
    let rebuilt = Seen::default();
    let _second = spawn(
        vec![
            SystemConfig::new("wide", "wide"),
            SystemConfig {
                params: json!({
                    "transport": { "listen": { "addr": addr.to_string() } },
                    "namespace": "cube_sat",
                    "link": "pub-rebuilt",
                    "conn_cap": 1 << 20,
                }),
                ..reading(
                    "pub",
                    "fsw.publish",
                    "wide.wide",
                    PortRef::new("wide", "wide"),
                )
            },
        ],
        &rebuilt,
        500.0,
    );
    until_seen("the mirror never faulted on the new peer", &client, || {
        client.faulted("peer_schema_mismatch")
    })
    .await;
}

#[stellarator::test]
async fn test_subscribe_skips_large_announce() {
    let (server, client) = (Seen::default(), Seen::default());
    let mut publish = reading(
        "pub",
        "fsw.publish",
        "src.ping",
        PortRef::new("src", "ping"),
    );
    publish
        .inputs
        .push(InputConfig::new("wide.wide", "wide", "wide"));
    publish.params = json!({
        "transport": listen_any(),
        "link": "pub-wide",
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
    let addr = bound("pub-wide").await;
    let _client = spawn(
        vec![
            subscribe(
                "cmds-wide",
                json!({ "connect": { "addr": addr.to_string() } }),
            ),
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
async fn test_oversized_packet_closes_connection() {
    let seen = Seen::default();
    let mut systems = imu_publisher("pub-cap");
    systems.extend([
        status_reader("pub"),
        reading("logs", "log_sink", "log", PortRef::new("pub", "log")),
    ]);
    let _target = spawn(systems, &seen, 500.0);
    let addr = bound("pub-cap").await;

    let peer = raw_dial(addr).await;
    until_seen("the link never took the connection", &seen, || {
        seen.status().is_some_and(|s| s.connections == 1)
    })
    .await;

    // A length prefix of four gigabytes, which the link must refuse rather
    // than allocate.
    let (written, _) = peer.write_all(u32::MAX.to_le_bytes().to_vec()).await;
    written.expect("the link takes the prefix");
    until_seen("the link kept a connection it cannot serve", &seen, || {
        seen.status().is_some_and(|s| s.connections == 0)
    })
    .await;
}

#[stellarator::test]
async fn test_disconnect_frees_slot() {
    let seen = Seen::default();
    let _target = spawn(
        vec![
            subscribe(
                "cmds-slot",
                json!({ "listen": { "addr": "127.0.0.1:0", "max_connections": 1 } }),
            ),
            reading("sink", "ping_sink", "ping", PortRef::new("cmds", "ping")),
        ],
        &seen,
        500.0,
    );
    let addr = bound("cmds-slot").await;

    // A peer takes the slot and leaves without sending a command.
    let Fsw { rx, tx, .. } = dial_fsw(addr).await;
    drop((rx, tx));

    // Nothing else wakes the link, so only noticing the hang-up can free the
    // slot.
    let Fsw { tx, .. } = dial_fsw(addr).await;
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
async fn test_refuse_peer_past_limit() {
    let seen = Seen::default();
    let _target = spawn(
        vec![
            subscribe(
                "cmds-full",
                json!({ "listen": { "addr": "127.0.0.1:0", "max_connections": 1 } }),
            ),
            reading("sink", "ping_sink", "ping", PortRef::new("cmds", "ping")),
        ],
        &seen,
        500.0,
    );
    let addr = bound("cmds-full").await;

    let Fsw { rx: _rx, tx, .. } = dial_fsw(addr).await;

    // A second peer is accepted and closed right away, not left waiting.
    let refused = raw_dial(addr).await;
    let (read, _) = within(
        "the link kept the refused peer waiting",
        refused.read(vec![0u8; 16]),
    )
    .await;
    assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");

    // The first peer still holds the slot.
    tx.send((&Ping { n: 4 }).into_len_packet())
        .await
        .0
        .expect("the link takes commands");
    until("the first peer never reached the consumer", || {
        seen.pings().contains(&4)
    })
    .await;
}

#[stellarator::test]
async fn test_stop_closes_connections() {
    let target = spawn(
        vec![subscribe("cmds-stop", listen_any())],
        &Seen::default(),
        500.0,
    );
    let Fsw { mut rx, buf, .. } = dial_fsw(bound("cmds-stop").await).await;
    drop(target);
    let read = within("the stopped link kept its peer", rx.next_grow(buf)).await;
    assert!(read.is_err());
}

/// Returns a `Publish` that dials `addr` and serves `imu.imu`, plus a sink
/// for its log.
fn dialing_publish(addr: SocketAddr) -> Vec<SystemConfig> {
    vec![
        SystemConfig::new("imu", "source"),
        publish(
            "pub-sim",
            json!({ "connect": { "addr": addr.to_string() } }),
            1 << 20,
        ),
        reading("log", "log_sink", "log", PortRef::new("pub", "log")),
    ]
}

#[stellarator::test]
async fn test_simulated_clock_preserves_connection() {
    // The peer listens before the target exists, so the connection opens
    // before the first cycle.
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

/// How many cycles the fixture target runs before it exits on its own. The
/// test kills it long before that.
const FIXTURE_CYCLES: &str = "600000";

/// The fixture's target running under `metor run`, with the addresses its
/// links bound.
struct Fixture {
    _child: Running,
    ports: Vec<(String, SocketAddr)>,
}

impl Fixture {
    /// Runs the fixture target with `--print-ports` and reads the addresses
    /// it prints.
    fn start() -> Self {
        let target = fixture_dir().join("target.py");
        let mut child = metor()
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
        Self {
            _child: Running(child),
            ports,
        }
    }

    fn addr(&self, link: &str) -> SocketAddr {
        let found = self.ports.iter().find(|(name, _)| name == link);
        // PANIC Safety: a target that did not bind both links fails the test.
        found.expect("the target printed both links").1
    }
}

#[stellarator::test]
async fn test_fixture_link_round_trip() {
    let fixture = Fixture::start();
    // Connect to the publisher first, so the reply cannot arrive before the
    // connection exists.
    let Fsw { mut rx, buf, .. } = dial_fsw(fixture.addr("pub")).await;
    let Fsw { info, tx, .. } = dial_fsw(fixture.addr("cmds")).await;
    assert_eq!(info.command_ids, vec![<Ping as Msg>::ID]);

    tx.send((&Ping { n: 41 }).into_len_packet())
        .await
        .0
        .expect("the link takes commands");

    let mut buf = buf;
    let echoed = within("the ping never came back", async {
        loop {
            if let OwnedPacket::Msg(m) = next(&mut rx, &mut buf).await
                && m.id == <Ping as Msg>::ID
            {
                return m.parse::<Ping>().expect("a ping").n;
            }
        }
    })
    .await;
    assert_eq!(echoed, 41);
}
