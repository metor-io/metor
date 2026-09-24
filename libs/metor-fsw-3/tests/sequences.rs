//! The fixture's slot under `metor run` and `metor build`: commands in over
//! a `Subscribe`, events and the registry out over a `Publish`.

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use metor_proto::buf::Slice;
use metor_proto::types::{IntoLenPacket, Msg, OwnedPacket};
use metor_proto_stellar::{PacketSink, PacketStream, Peer, identify};
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
    SequenceRegistry, SetComponentMetadata,
};
use stellarator::io::{OwnedReader, OwnedWriter};
use stellarator::net::TcpStream;

/// How long a test waits for the target before it fails.
const DEADLINE: Duration = Duration::from_secs(20);

/// Cycles the target runs before it exits on its own.
const CYCLES: &str = "60000";

const PORTS_ENV: &str = "METOR_SLOT_PORTS";

fn target() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/echo-pack/slot.py")
}

fn metor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_metor"))
}

/// Two ports nothing holds at the moment they are read.
fn free_ports() -> [u16; 2] {
    let held: Vec<TcpListener> = (0..2)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("a free port"))
        .collect();
    let port = |at: usize| held[at].local_addr().expect("bound").port();
    [port(0), port(1)]
}

fn ports_env(ports: &[u16; 2]) -> String {
    format!("{},{}", ports[0], ports[1])
}

/// A `metor run` of the fixture, killed and reaped when it drops.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(ports: &[u16; 2]) -> Running {
    let child = metor()
        .arg("run")
        .arg(target())
        .args(["--cycles", CYCLES])
        .env(PORTS_ENV, ports_env(ports))
        .stdout(Stdio::null())
        .spawn()
        .expect("metor runs");
    Running(child)
}

/// Dials a link until it answers with its identity.
async fn dial(port: u16) -> Peer {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + DEADLINE;
    loop {
        match identify(addr).await {
            Ok(peer) => return peer,
            Err(e) => assert!(Instant::now() < deadline, "never came up: {e}"),
        }
        stellarator::sleep(Duration::from_millis(20)).await;
    }
}

/// What the publish link carries that a channel test reads.
struct Downlink {
    rx: PacketStream<OwnedReader<TcpStream>>,
    buf: Vec<u8>,
    registry: Option<SequenceRegistry>,
    /// Whether `mode.channel_status.artifact` was announced as a string.
    artifact_is_string: bool,
}

impl Downlink {
    /// Reads until `mode` reports an event, keeping what else it sees.
    async fn event(&mut self) -> SequenceEventKind {
        loop {
            if let Some(kind) = self.next().await {
                return kind;
            }
        }
    }

    /// Reads one packet, returning the event it carries for `mode`.
    async fn next(&mut self) -> Option<SequenceEventKind> {
        let taken = core::mem::take(&mut self.buf);
        let packet = self.rx.next_grow(taken).await.expect("the link stays up");
        let kind = self.read(&packet);
        drop(packet);
        self.buf = vec![0u8; 64 * 1024];
        kind
    }

    fn read(&mut self, packet: &OwnedPacket<Slice<Vec<u8>>>) -> Option<SequenceEventKind> {
        let OwnedPacket::Msg(msg) = packet else {
            return None;
        };
        if msg.id == SequenceRegistry::ID {
            self.registry = Some(msg.parse().expect("a registry"));
        } else if msg.id == SetComponentMetadata::ID {
            let set: SetComponentMetadata = msg.parse().expect("metadata");
            if set.0.name.ends_with("mode.channel_status.artifact") {
                self.artifact_is_string = set.0.is_string();
            }
        } else if msg.id == SequenceChannelEvent::ID {
            let event: SequenceChannelEvent = msg.parse().expect("an event");
            if event.channel == "mode" {
                return Some(event.kind);
            }
        }
        None
    }

    /// Reads until a registry arrives; it is rewritten once a second.
    async fn registry(&mut self) -> SequenceRegistry {
        futures_lite::future::or(
            async {
                while self.registry.is_none() {
                    self.next().await;
                }
            },
            async {
                stellarator::sleep(DEADLINE).await;
                panic!("no registry")
            },
        )
        .await;
        self.registry.take().expect("read above")
    }

    /// The next event, failing the test at the deadline.
    async fn expect(&mut self, what: &str) -> SequenceEventKind {
        futures_lite::future::or(self.event(), async {
            stellarator::sleep(DEADLINE).await;
            panic!("no event: {what}")
        })
        .await
    }
}

async fn send(tx: &PacketSink<OwnedWriter<TcpStream>>, command: SequenceCommandKind) {
    let command = SequenceCommand {
        channel: "mode".into(),
        command,
    };
    tx.send((&command).into_len_packet())
        .await
        .0
        .expect("the link takes commands");
}

fn load(name: &str) -> SequenceCommandKind {
    SequenceCommandKind::Load { name: name.into() }
}

#[stellarator::test]
async fn test_slot_commanded_over_links() {
    let ports = free_ports();
    let _running = run(&ports);
    let Peer::Fsw { rx, buf, .. } = dial(ports[1]).await else {
        panic!("a publish is an fsw link")
    };
    let mut down = Downlink {
        rx,
        buf,
        registry: None,
        artifact_is_string: false,
    };
    let Peer::Fsw { tx, .. } = dial(ports[0]).await else {
        panic!("a subscribe is an fsw link")
    };

    send(&tx, load("count_up")).await;
    let loaded = SequenceEventKind::Loaded {
        name: "count_up".into(),
    };
    assert_eq!(down.expect("load count_up").await, loaded);
    send(&tx, SequenceCommandKind::Start).await;
    assert_eq!(down.expect("start").await, SequenceEventKind::Started);
    assert_eq!(down.expect("complete").await, SequenceEventKind::Completed);

    send(&tx, load("count_down")).await;
    assert!(matches!(
        down.expect("load count_down").await,
        SequenceEventKind::Loaded { name } if name == "count_down"
    ));
    send(&tx, SequenceCommandKind::Start).await;
    assert_eq!(down.expect("start").await, SequenceEventKind::Started);
    send(&tx, SequenceCommandKind::Abort).await;
    assert!(matches!(
        down.expect("abort accepted").await,
        SequenceEventKind::Progress { .. }
    ));
    assert_eq!(down.expect("aborted").await, SequenceEventKind::Aborted);

    send(&tx, load("count_sideways")).await;
    assert!(matches!(
        down.expect("unknown load").await,
        SequenceEventKind::Refused { .. }
    ));

    let registry = down.registry().await;
    assert_eq!(registry.channels.len(), 1);
    assert_eq!(registry.channels[0].name, "mode");
    assert_eq!(registry.channels[0].available, ["count_up", "count_down"]);
    assert!(down.artifact_is_string, "the artifact is announced as text");
}

#[test]
fn test_build_stores_candidates() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dir.path().join("slot.json");
    let output = metor()
        .args(["build", &target().display().to_string(), "--out"])
        .arg(&out)
        .output()
        .expect("metor runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&out).expect("written")).expect("json");
    let slot = &envelope["targets"][0]["coordinator"]["systems"][1];
    assert_eq!(slot["ty"], "fsw.dl_slot");
    let allow = slot["params"]["allow"].as_array().expect("an allow list");
    assert_eq!(allow.len(), 2);
    for candidate in allow {
        assert!(candidate.get("pack").is_none(), "{candidate}");
        let path = Path::new(candidate["path"].as_str().expect("a path"));
        let hash = candidate["hash"].as_str().expect("a hash");
        assert!(path.is_file(), "{}", path.display());
        let dir = path.parent().and_then(Path::file_name).expect("a hash dir");
        assert_eq!(dir.to_str(), Some(hash));
        assert!(candidate["def"].is_object());
    }
    let store = Path::new(slot["params"]["store"].as_str().expect("a store"));
    assert!(
        allow[0]["path"]
            .as_str()
            .is_some_and(|p| Path::new(p).starts_with(store))
    );
}
