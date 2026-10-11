//! The fixture deployment under `metor run`: `a` samples, `b` mirrors and
//! pings, and the gateway's db holds both.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use metor_fsw_3::coordinator::{
    CoordinatorConfig, InputConfig, PortRef, SystemConfig, SystemTable,
};
use metor_fsw_3::link::register_builtins;
use metor_fsw_3::{Clock, Frame, LinkStatus, Output, Timestamp, system};
use metor_proto::types::{ComponentId, Msg, OwnedPacket};
use metor_proto_stellar::{Client, Peer, identify};
use metor_proto_wkt::{DumpMetadata, DumpMetadataResp, GetTimeSeries, LogEvent};
use serde_json::json;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// How long a test waits for the deployment before it fails.
const DEADLINE: Duration = Duration::from_secs(15);

/// Cycles a target runs before it exits on its own, which outlives any test
/// here and bounds what a killed run's children live on for.
const CYCLES: &str = "20000";

const PORTS_ENV: &str = "METOR_DEPLOY_PORTS";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-pack")
}

fn metor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_metor"))
}

/// Three ports nothing holds at the moment they are read.
fn free_ports() -> [u16; 3] {
    let held: Vec<TcpListener> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("a free port"))
        .collect();
    let ports: Vec<u16> = held
        .iter()
        .map(|listener| listener.local_addr().expect("bound").port())
        .collect();
    [ports[0], ports[1], ports[2]]
}

fn ports_env(ports: &[u16; 3]) -> String {
    format!("{},{},{}", ports[0], ports[1], ports[2])
}

fn addr(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

/// A `metor run` of the fixture, killed and reaped when it drops.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(target: &Path, ports: &[u16; 3], args: &[&str]) -> Running {
    let child = metor()
        .arg("run")
        .arg(target)
        .args(args)
        .env(PORTS_ENV, ports_env(ports))
        .stdout(Stdio::null())
        .spawn()
        .expect("metor runs");
    Running(child)
}

/// Dials the db until it answers.
async fn connect(addr: SocketAddr) -> Client {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Ok(client) = Client::connect(addr).await {
            return client;
        }
        assert!(Instant::now() < deadline, "the db never came up");
        stellarator::sleep(Duration::from_millis(20)).await;
    }
}

async fn metadata(client: &mut Client) -> DumpMetadataResp {
    client.request(&DumpMetadata).await.expect("the db answers")
}

#[stellarator::test]
async fn test_gateway_records_every_target() {
    let ports = free_ports();
    let _running = run(&fixture().join("deploy.py"), &ports, &["--cycles", CYCLES]);
    let mut client = connect(addr(ports[2])).await;

    // A mirrored frame keeps its origin's names, so `b`'s copy of the sample
    // lands in the same table as `a`'s own; `b`'s link status is `b`'s.
    let deadline = Instant::now() + DEADLINE;
    let wanted = ["a.sampler.sample.value", "b.mirror.link_status.connections"];
    loop {
        let dump = metadata(&mut client).await;
        let names: Vec<&str> = dump
            .component_metadata
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        if wanted.iter().all(|name| names.contains(name))
            && dump.msg_metadata.iter().any(|m| m.name == "Ping")
        {
            break;
        }
        assert!(Instant::now() < deadline, "announced only {names:?}");
        stellarator::sleep(Duration::from_millis(20)).await;
    }

    // Samples reach a series once the db's persistence drains its log, so a
    // query is refused until then.
    let component_id = ComponentId::new(wanted[0]);
    loop {
        let series = client
            .request(&GetTimeSeries {
                id: [0, 0],
                range: Timestamp(0)..Timestamp(i64::MAX / 2),
                component_id,
                limit: Some(64),
            })
            .await;
        // The table is fed by `a` and again through `b`'s mirror, so a stamp
        // may repeat; it never goes backwards.
        if let Ok(series) = &series
            && let Ok(stamps) = series.timestamps()
            && stamps.len() >= 2
            && stamps.first() != stamps.last()
        {
            assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "{stamps:?}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the series never filled: {series:?}"
        );
        stellarator::sleep(Duration::from_millis(20)).await;
    }
}

/// `metor build` writes the resolved envelope, with the pack path as given.
fn build(ports: Option<&[u16; 3]>, out: &Path) -> serde_json::Value {
    let mut command = metor();
    command
        .args([
            "build",
            &fixture().join("deploy.py").display().to_string(),
            "-o",
        ])
        .arg(out);
    if let Some(ports) = ports {
        command.env(PORTS_ENV, ports_env(ports));
    }
    let output = command.output().expect("metor builds");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&std::fs::read(out).expect("written")).expect("json")
}

#[test]
fn test_build_matches_golden() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut built = build(None, &dir.path().join("deploy.json"));
    let libs = fixture().join(".metor/echo_pack/_libs");
    for target in built["targets"].as_array_mut().expect("targets") {
        for pack in target["packs"].as_array_mut().expect("packs") {
            assert_eq!(pack["libs"], json!(libs.display().to_string()));
            pack["libs"] = json!("<libs>");
        }
    }
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/deploy.json");
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&golden).expect("the golden exists")).expect("json");
    assert_eq!(built, expected);
}

/// A target run from a resolved envelope opens no pack but its own.
#[test]
fn test_resolved_target_opens_only_its_packs() {
    let ports = free_ports();
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dir.path().join("deploy.json");
    let mut built = build(Some(&ports), &out);
    for target in built["targets"].as_array_mut().expect("targets") {
        if target["coordinator"]["namespace"] != "b" {
            for pack in target["packs"].as_array_mut().expect("packs") {
                pack["libs"] = json!("/nonexistent/_libs");
            }
        }
    }
    std::fs::write(&out, serde_json::to_vec(&built).expect("encodes")).expect("writes");
    let output = metor()
        .args([
            "run",
            &out.display().to_string(),
            "--target",
            "b",
            "--cycles",
            "3",
        ])
        .output()
        .expect("metor runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A frame the fixture does not know, served in `a`'s place.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(name = "imu")]
#[repr(C)]
struct Imu {
    #[frame(timestamp)]
    timestamp: Timestamp,
    sample: f64,
}

/// Writes a frame per cycle once `gate` is set.
struct Source(i64, Arc<AtomicBool>);

#[system]
impl Source {
    fn execute(&mut self, imu: &mut Output<Imu>) {
        if !self.1.load(Ordering::Acquire) {
            return;
        }
        self.0 += 1;
        let _ = imu.write(&Imu {
            timestamp: Timestamp(self.0),
            sample: self.0 as f64,
        });
    }
}

/// An in-process target on its own thread, stopped and joined when dropped;
/// it announces at once and sends data once `gate` is set.
struct Impostor {
    stop: Arc<AtomicBool>,
    gate: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Impostor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

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

/// Serves `imu.imu` as target `a`'s `pub` on `port`, which is not the table
/// `b` was built against.
fn impostor(port: u16) -> Impostor {
    let stop = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(AtomicBool::new(false));
    let (flag, open) = (stop.clone(), gate.clone());
    let thread = std::thread::spawn(move || {
        let mut table = SystemTable::new();
        table
            .register("source", move || Source(0, open.clone()))
            .expect("valid records");
        register_builtins(&mut table).expect("valid records");
        let config = CoordinatorConfig {
            clock: Clock::Wall { rate: 1000.0 },
            namespace: Some("a".into()),
            systems: vec![
                SystemConfig::new("imu", "source"),
                SystemConfig {
                    params: json!({
                        "transport": { "listen": { "addr": format!("127.0.0.1:{port}"), "max_connections": 2 } },
                        "namespace": "a",
                        "link": "pub",
                    }),
                    inputs: vec![InputConfig {
                        port: "imu.imu".into(),
                        from: vec![PortRef::new("imu", "imu")],
                    }],
                    ..SystemConfig::new("pub", "fsw.publish")
                },
            ],
            ..Default::default()
        };
        let mut coordinator = config.build(&table).expect("valid config");
        stellarator::run(|| async move { coordinator.run(Until(flag)).await });
    });
    Impostor {
        stop,
        gate,
        thread: Some(thread),
    }
}

/// Dials until the link is up.
async fn dial(addr: SocketAddr) -> Peer {
    let deadline = Instant::now() + DEADLINE;
    loop {
        match identify(addr).await {
            Ok(peer) => return peer,
            Err(e) => assert!(Instant::now() < deadline, "never came up: {e}"),
        }
        stellarator::sleep(Duration::from_millis(20)).await;
    }
}

#[stellarator::test]
async fn test_peer_announcing_another_table_is_faulted() {
    let ports = free_ports();
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dir.path().join("deploy.json");
    let built = build(Some(&ports), &out);
    let mirror = &built["targets"][1]["coordinator"]["systems"][0];
    assert_eq!(
        mirror["params"]["transport"]["connect"]["addr"],
        json!(format!("127.0.0.1:{}", ports[0])),
        "{mirror}"
    );
    let impostor = impostor(ports[0]);
    let Peer::Fsw { info, .. } = dial(addr(ports[0])).await else {
        panic!("the impostor is an fsw link")
    };
    assert_eq!(info.link, "pub");
    let _running = run(&out, &ports, &["--target", "b", "--cycles", CYCLES]);
    // The fault is logged once, on the peer's first data packet, so the
    // observer attaches before the impostor sends any.
    let Peer::Fsw { mut rx, buf, .. } = dial(addr(ports[1])).await else {
        panic!("b's publish is an fsw link")
    };
    impostor.gate.store(true, Ordering::Release);
    let mut buf = buf;
    let mut seen: Vec<String> = Vec::new();
    let deadline = Instant::now() + DEADLINE;
    loop {
        assert!(
            Instant::now() < deadline,
            "no mismatch fault arrived; saw {seen:?}"
        );
        let taken = core::mem::take(&mut buf);
        let packet = rx.next_grow(taken).await.expect("the link stays up");
        buf = vec![0u8; 64 * 1024];
        let OwnedPacket::Msg(m) = packet else {
            if let OwnedPacket::Table(t) = &packet
                && let Ok(status) = LinkStatus::read_from_bytes(&t.buf)
                && !seen.last().is_some_and(|s| s.contains("connections"))
            {
                seen.push(format!(
                    "connections {} out {}",
                    status.connections, status.bytes_out
                ));
            }
            continue;
        };
        if m.id != LogEvent::ID {
            seen.push(format!("msg {:?}", m.id));
            continue;
        }
        let event = m.parse::<LogEvent>().expect("a log event");
        let kind = event.fields.iter().find(|(name, _)| name == "kind");
        seen.push(format!("log {kind:?} {}", event.message));
        if kind.is_some_and(|(_, kind)| kind == "peer_schema_mismatch") {
            assert!(
                event.message.contains("sampler.sample"),
                "{}",
                event.message
            );
            return;
        }
    }
}
