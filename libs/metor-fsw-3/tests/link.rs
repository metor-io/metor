//! A running target's link: what a client sees when it dials a `Publish`.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use core::time::Duration;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use metor_fsw_3::coordinator::{
    CoordinatorConfig, InputConfig, PortRef, SystemConfig, SystemTable,
};
use metor_fsw_3::link::register_builtins;
use metor_fsw_3::{Clock, Frame, Input, LinkStatus, Output, Timestamp, system};
use metor_proto::types::{Msg, OwnedPacket};
use metor_proto_stellar::{PacketStream, Peer, identify};
use metor_proto_wkt::{SetComponentMetadata, VTableMsg};
use serde_json::json;
use stellarator::io::{OwnedReader, SplitExt};
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

/// The systems every test in this file may name, plus the built-in links.
fn table(statuses: &Arc<Mutex<Vec<LinkStatus>>>) -> SystemTable {
    let mut table = SystemTable::new();
    table.register("source", Source::default);
    let statuses = statuses.clone();
    table.register("status_sink", move || StatusSink(statuses.clone()));
    register_builtins(&mut table);
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
fn spawn(systems: Vec<SystemConfig>, statuses: &Arc<Mutex<Vec<LinkStatus>>>, rate: f64) -> Target {
    let stop = Arc::new(AtomicBool::new(false));
    let (flag, statuses) = (stop.clone(), statuses.clone());
    let thread = std::thread::spawn(move || {
        let config = CoordinatorConfig {
            clock: Clock::Wall { rate },
            systems,
            ..Default::default()
        };
        let mut coordinator = config.build(&table(&statuses)).expect("valid config");
        stellarator::run(|| async move { coordinator.run(Until(flag)).await });
    });
    Target {
        stop,
        thread: Some(thread),
    }
}

/// Ready once the target's handle asks it to stop.
struct Until(Arc<AtomicBool>);

impl Future for Until {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
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
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let _target = spawn(
        vec![
            SystemConfig::new("imu", "source"),
            publish(json!({ "listen": { "addr": addr.to_string() } }), 1 << 20),
        ],
        &statuses,
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
    let statuses = Arc::new(Mutex::new(Vec::new()));
    let _target = spawn(
        vec![
            SystemConfig::new("imu", "source"),
            publish(json!({ "connect": { "addr": addr.to_string() } }), 1 << 20),
        ],
        &statuses,
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
    let statuses = Arc::new(Mutex::new(Vec::new()));
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
        &statuses,
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

    let latest = || {
        // PANIC Safety: no test holds this lock across a panic.
        statuses.lock().expect("an unpoisoned sink").last().copied()
    };
    until("the link never reported two connections", || {
        latest().is_some_and(|s| s.connections == 2 && s.batches_dropped > 0)
    })
    .await;
    assert_eq!(latest().expect("a status").connections, 2);
}
