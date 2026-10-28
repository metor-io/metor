//! Tests the fixture's sequence slot under `metor run` and `metor build`.
//!
//! Commands arrive through a `Subscribe`. The channel's status and the
//! registry go out through a `Publish`.

mod common;

use std::path::PathBuf;
use std::process::Stdio;

use common::{Fsw, Running, dial_fsw, fixture_dir, free_ports, local, metor, ports_env, within};
use metor_fsw_3::ChannelStatus;
use metor_proto::buf::Slice;
use metor_proto::types::{IntoLenPacket, Msg, OwnedPacket, PacketId};
use metor_proto_stellar::{PacketSink, PacketStream};
use metor_proto_wkt::{
    SequenceAccepts, SequenceChannelState, SequenceCommand, SequenceCommandKind, SequenceRegistry,
    SetComponentMetadata, VTableMsg,
};
use stellarator::io::{OwnedReader, OwnedWriter};
use stellarator::net::TcpStream;
use zerocopy::FromBytes;

/// How many cycles the target runs before it exits on its own.
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

/// A client of the target's `Publish` link, which reads what the channel
/// tests check.
struct Downlink {
    rx: PacketStream<OwnedReader<TcpStream>>,
    buf: Vec<u8>,
    registry: Option<SequenceRegistry>,
    /// Whether `mode.channel_status.artifact` was announced as a string.
    artifact_is_string: bool,
    /// The table id of the last vtable announced. Its components announce
    /// right after it.
    announced: Option<PacketId>,
    /// The table id `mode.channel_status` arrives on.
    status_id: Option<PacketId>,
    status: Option<ChannelStatus>,
}

impl Downlink {
    /// Reads until `mode`'s status passes `done`, and returns that status.
    async fn until(&mut self, what: &str, done: impl Fn(&ChannelStatus) -> bool) -> ChannelStatus {
        within(&format!("no status: {what}"), async {
            loop {
                if let Some(status) = self.status.take_if(|status| done(status)) {
                    return status;
                }
                self.next().await;
            }
        })
        .await
    }

    async fn next(&mut self) {
        let taken = core::mem::take(&mut self.buf);
        let packet = self.rx.next_grow(taken).await.expect("the link stays up");
        self.read(&packet);
        drop(packet);
        self.buf = vec![0u8; 64 * 1024];
    }

    fn read(&mut self, packet: &OwnedPacket<Slice<Vec<u8>>>) {
        match packet {
            OwnedPacket::Table(table) if Some(table.id) == self.status_id => {
                let status = ChannelStatus::read_from_bytes(&table.buf).expect("a status");
                self.status = Some(status);
            }
            OwnedPacket::Msg(msg) if msg.id == SequenceRegistry::ID => {
                self.registry = Some(msg.parse().expect("a registry"));
            }
            OwnedPacket::Msg(msg) if msg.id == VTableMsg::ID => {
                self.announced = Some(msg.parse::<VTableMsg>().expect("a vtable").id);
            }
            OwnedPacket::Msg(msg) if msg.id == SetComponentMetadata::ID => {
                let set: SetComponentMetadata = msg.parse().expect("metadata");
                if set.0.name.ends_with("mode.channel_status.artifact") {
                    self.artifact_is_string = set.0.is_string();
                    self.status_id = self.announced;
                }
            }
            _ => {}
        }
    }

    /// Reads until a registry arrives. The target sends one every second.
    async fn registry(&mut self) -> SequenceRegistry {
        within("no registry", async {
            while self.registry.is_none() {
                self.next().await;
            }
        })
        .await;
        self.registry.take().expect("read above")
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

fn message(status: &ChannelStatus) -> &str {
    status.message.to_str().expect("utf-8")
}

fn is(status: &ChannelStatus, state: SequenceChannelState) -> bool {
    status.state == state.code()
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
        announced: None,
        status_id: None,
        status: None,
    };
    let Fsw { tx, .. } = dial_fsw(local(ports[0])).await;

    let booted = down.until("boot", |_| true).await;
    assert!(is(&booted, SequenceChannelState::Unloaded), "{booted:?}");
    let accepts = SequenceAccepts(booted.accepts);
    assert!(accepts.allows(&load("count_up")));
    assert!(!accepts.allows(&SequenceCommandKind::Start));

    send(&tx, load("count_up")).await;
    let loaded = down
        .until("load count_up", |s| message(s) == "loaded count_up")
        .await;
    assert!(is(&loaded, SequenceChannelState::Stopped), "{loaded:?}");
    assert_eq!(loaded.loaded, 0);
    assert!(SequenceAccepts(loaded.accepts).allows(&SequenceCommandKind::Start));

    send(&tx, SequenceCommandKind::Start).await;
    let completed = down
        .until("complete", |s| is(s, SequenceChannelState::Completed))
        .await;
    assert_eq!(message(&completed), "completed");

    send(&tx, load("count_down")).await;
    let loaded = down
        .until("load count_down", |s| message(s) == "loaded count_down")
        .await;
    assert_eq!(loaded.loaded, 1);
    send(&tx, SequenceCommandKind::Start).await;
    down.until("start", |s| is(s, SequenceChannelState::Running))
        .await;
    send(&tx, SequenceCommandKind::Abort).await;
    let aborted = down
        .until("aborted", |s| is(s, SequenceChannelState::Aborted))
        .await;
    assert_eq!(message(&aborted), "aborted");

    send(&tx, load("count_sideways")).await;
    let refused = down
        .until("unknown load", |s| message(s).starts_with("refused: "))
        .await;
    assert!(message(&refused).contains("count_sideways"), "{refused:?}");
    assert!(is(&refused, SequenceChannelState::Aborted), "{refused:?}");
    assert!(refused.message_at > aborted.message_at);

    let registry = down.registry().await;
    assert_eq!(registry.channels.len(), 1);
    let channel = &registry.channels[0];
    assert_eq!(channel.name, "mode");
    assert_eq!(channel.available, ["count_up", "count_down"]);
    assert_eq!(channel.system, "dl_slot");
    assert_eq!(channel.status.state, "mode.channel_status.state");
    assert_eq!(channel.status.message, "mode.channel_status.message");
    assert_eq!(channel.status.message_at, "mode.channel_status.message_at");
    assert!(down.artifact_is_string, "the artifact is announced as text");
}
