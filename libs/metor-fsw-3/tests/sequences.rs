//! Tests the fixture's sequence slot under `metor run` and `metor build`.
//!
//! Commands arrive through a `Subscribe`. The channel's status, its messages,
//! and the registry go out through a `Publish`.

mod common;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;

use common::{Fsw, Running, dial_fsw, fixture_dir, free_ports, local, metor, ports_env, within};
use metor_fsw_3::{ChannelMessage, ChannelStatus};
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
    /// The table id `mode.channel_message` arrives on.
    message_id: Option<PacketId>,
    /// The messages read and not yet waited for, oldest first.
    messages: VecDeque<ChannelMessage>,
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

    /// Reads until a message passes `done`, and returns it. Messages before
    /// it are dropped.
    async fn until_message(&mut self, what: &str, done: impl Fn(&str) -> bool) -> ChannelMessage {
        within(&format!("no message: {what}"), async {
            loop {
                while let Some(message) = self.messages.pop_front() {
                    if done(text(&message)) {
                        return message;
                    }
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
            OwnedPacket::Table(table) if Some(table.id) == self.message_id => {
                let message = ChannelMessage::read_from_bytes(&table.buf).expect("a message");
                self.messages.push_back(message);
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
                if set.0.name.ends_with("mode.channel_message.text") {
                    self.message_id = self.announced;
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

fn text(message: &ChannelMessage) -> &str {
    message.text.to_str().expect("utf-8")
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
        message_id: None,
        messages: VecDeque::new(),
    };
    let Fsw { tx, .. } = dial_fsw(local(ports[0])).await;

    let booted = down.until("boot", |_| true).await;
    assert!(is(&booted, SequenceChannelState::Unloaded), "{booted:?}");
    let accepts = SequenceAccepts(booted.accepts);
    assert!(accepts.allows(&load("count_up")));
    assert!(!accepts.allows(&SequenceCommandKind::Start));

    send(&tx, load("count_up")).await;
    down.until_message("load count_up", |m| m == "loaded count_up")
        .await;
    let loaded = down.until("count_up stopped", |s| s.loaded == 0).await;
    assert!(is(&loaded, SequenceChannelState::Stopped), "{loaded:?}");
    assert!(SequenceAccepts(loaded.accepts).allows(&SequenceCommandKind::Start));

    send(&tx, SequenceCommandKind::Start).await;
    down.until("complete", |s| is(s, SequenceChannelState::Completed))
        .await;
    down.until_message("completed", |m| m == "completed").await;

    send(&tx, load("count_down")).await;
    down.until_message("load count_down", |m| m == "loaded count_down")
        .await;
    down.until("count_down loaded", |s| s.loaded == 1).await;
    send(&tx, SequenceCommandKind::Start).await;
    down.until("start", |s| is(s, SequenceChannelState::Running))
        .await;
    send(&tx, SequenceCommandKind::Abort).await;
    down.until("aborted", |s| is(s, SequenceChannelState::Aborted))
        .await;
    let aborted = down.until_message("aborted", |m| m == "aborted").await;

    send(&tx, load("count_sideways")).await;
    let refused = down
        .until_message("unknown load", |m| m.starts_with("refused: "))
        .await;
    assert!(text(&refused).contains("count_sideways"), "{refused:?}");
    assert!(refused.timestamp > aborted.timestamp);
    let after = down.until("after the refusal", |_| true).await;
    assert!(is(&after, SequenceChannelState::Aborted), "{after:?}");

    let registry = down.registry().await;
    assert_eq!(registry.channels.len(), 1);
    let channel = &registry.channels[0];
    assert_eq!(channel.name, "mode");
    assert_eq!(channel.available, ["count_up", "count_down"]);
    assert_eq!(channel.system, "dl_slot");
    assert_eq!(channel.status.state, "mode.channel_status.state");
    assert_eq!(channel.status.message, "mode.channel_message.text");
    assert!(down.artifact_is_string, "the artifact is announced as text");
}
