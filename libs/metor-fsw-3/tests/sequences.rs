//! The fixture's slot under `metor run` and `metor build`: commands in over
//! a `Subscribe`, events and the registry out over a `Publish`.

mod common;

use std::path::PathBuf;
use std::process::Stdio;

use common::{Fsw, Running, dial_fsw, fixture_dir, free_ports, local, metor, ports_env, within};
use metor_proto::buf::Slice;
use metor_proto::types::{IntoLenPacket, Msg, OwnedPacket};
use metor_proto_stellar::{PacketSink, PacketStream};
use metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
    SequenceRegistry, SetComponentMetadata,
};
use stellarator::io::{OwnedReader, OwnedWriter};
use stellarator::net::TcpStream;

/// Cycles the target runs before it exits on its own.
const CYCLES: &str = "60000";

const PORTS_ENV: &str = "METOR_SLOT_PORTS";

fn target() -> PathBuf {
    fixture_dir().join("slot.py")
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
        within("no registry", async {
            while self.registry.is_none() {
                self.next().await;
            }
        })
        .await;
        self.registry.take().expect("read above")
    }

    /// The next event, failing the test at the deadline.
    async fn expect(&mut self, what: &str) -> SequenceEventKind {
        within(&format!("no event: {what}"), self.event()).await
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
    let Fsw { rx, buf, .. } = dial_fsw(local(ports[1])).await;
    let mut down = Downlink {
        rx,
        buf,
        registry: None,
        artifact_is_string: false,
    };
    let Fsw { tx, .. } = dial_fsw(local(ports[0])).await;

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
