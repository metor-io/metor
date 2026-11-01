//! Tests alarm edges over a real link under `metor run`.
//!
//! `hot` (a pack system) and `burn` (a sequence) read a level that the test
//! sets by pinging `cmds`. A `Publish` serves their `AlarmRaised` and
//! `AlarmCleared` messages.

mod common;

use std::path::PathBuf;
use std::process::Stdio;

use common::{
    Fsw, Ping, Running, dial_fsw, fixture_dir, free_ports, local, metor, ports_env, within,
};
use metor_proto::buf::Slice;
use metor_proto::types::{IntoLenPacket, Msg, OwnedPacket};
use metor_proto_stellar::{PacketSink, PacketStream};
use metor_proto_wkt::{AlarmCleared, AlarmRaised, SequenceCommand, SequenceCommandKind, Severity};
use stellarator::io::{OwnedReader, OwnedWriter};
use stellarator::net::TcpStream;

/// How many cycles the target runs before it exits on its own.
const CYCLES: &str = "60000";

const PORTS_ENV: &str = "METOR_ALARM_PORTS";

const HOT: &str = "hot.hot";
const WARM: &str = "burn.warm";
const REDLINE: &str = "burn.redline.too hot";

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

fn target() -> PathBuf {
    fixture_dir().join("alarms.py")
}

/// An alarm edge as the ground decodes it.
#[derive(Clone, Debug)]
enum Edge {
    Raised(AlarmRaised),
    Cleared(AlarmCleared),
}

impl Edge {
    fn id(&self) -> &str {
        match self {
            Self::Raised(r) => &r.def_id,
            Self::Cleared(c) => &c.def_id,
        }
    }

    fn occurrence(&self) -> u64 {
        match self {
            Self::Raised(r) => r.occurrence,
            Self::Cleared(c) => c.occurrence,
        }
    }

    fn is_raise(&self) -> bool {
        matches!(self, Self::Raised(_))
    }
}

/// A client of the target's `Publish` link that collects alarm edges.
struct Downlink {
    rx: PacketStream<OwnedReader<TcpStream>>,
    buf: Vec<u8>,
    edges: Vec<Edge>,
}

impl Downlink {
    /// Reads until an edge passes `done`, and returns it. Edges before it
    /// are kept for [`Downlink::seen`].
    async fn until(&mut self, what: &str, done: impl Fn(&Edge) -> bool) -> Edge {
        within(&format!("no edge: {what}"), async {
            let mut checked = 0;
            loop {
                if let Some(edge) = self.edges[checked..].iter().find(|e| done(e)) {
                    return edge.clone();
                }
                checked = self.edges.len();
                self.next().await;
            }
        })
        .await
    }

    /// A raise of `id` with an occurrence not in `old`.
    async fn raised(&mut self, id: &str, old: &[u64]) -> Edge {
        let what = format!("raise of {id}");
        self.until(&what, |e| {
            e.is_raise() && e.id() == id && !old.contains(&e.occurrence())
        })
        .await
    }

    /// The clear of occurrence `occurrence`.
    async fn cleared(&mut self, what: &str, occurrence: u64) -> Edge {
        self.until(what, |e| !e.is_raise() && e.occurrence() == occurrence)
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
        let OwnedPacket::Msg(msg) = packet else {
            return;
        };
        if msg.id == AlarmRaised::ID {
            self.edges.push(Edge::Raised(msg.parse().expect("a raise")));
        } else if msg.id == AlarmCleared::ID {
            self.edges
                .push(Edge::Cleared(msg.parse().expect("a clear")));
        }
    }
}

/// The command half of the link.
struct Uplink(PacketSink<OwnedWriter<TcpStream>>);

impl Uplink {
    async fn level(&self, n: u32) {
        let ping = Ping { n };
        self.0
            .send((&ping).into_len_packet())
            .await
            .0
            .expect("sent");
    }

    async fn command(&self, channel: &str, command: SequenceCommandKind) {
        let command = SequenceCommand {
            channel: channel.into(),
            command,
        };
        self.0
            .send((&command).into_len_packet())
            .await
            .0
            .expect("sent");
    }
}

#[stellarator::test]
async fn test_alarm_edges_over_links() {
    let ports = free_ports();
    let _running = run(&ports);
    let Fsw { rx, buf, .. } = dial_fsw(local(ports[1])).await;
    let mut down = Downlink {
        rx,
        buf,
        edges: Vec::new(),
    };
    let Fsw { tx, .. } = dial_fsw(local(ports[0])).await;
    let up = Uplink(tx);

    // Nominal: `hot` raises then clears with one occurrence; so does `warm`.
    up.level(1).await;
    let hot = down.raised(HOT, &[]).await;
    let warm = down.raised(WARM, &[]).await;
    let Edge::Raised(raise) = &hot else {
        unreachable!("a raise")
    };
    assert_eq!(raise.severity, Severity::Warning);
    assert_eq!(raise.message, "at 1");
    up.level(0).await;
    down.cleared("hot clears", hot.occurrence()).await;
    down.cleared("warm clears", warm.occurrence()).await;

    // Failure: an abort of `burn` with `warm` active still clears it.
    up.level(1).await;
    let hot = down.raised(HOT, &[hot.occurrence()]).await;
    let warm = down.raised(WARM, &[warm.occurrence()]).await;
    up.command("burn", SequenceCommandKind::Abort).await;
    down.cleared("warm clears on abort", warm.occurrence())
        .await;

    // Failure: a reset of `hot` with its alarm active clears it from the
    // pack's drop.
    up.command("hot", SequenceCommandKind::Stop).await;
    up.command("hot", SequenceCommandKind::Reset).await;
    down.cleared("hot clears on reset", hot.occurrence()).await;

    // Edge: the redline raises and clears as it aborts.
    up.command("burn", SequenceCommandKind::Reset).await;
    up.command("burn", SequenceCommandKind::Start).await;
    down.raised(WARM, &[warm.occurrence()]).await;
    up.level(2).await;
    let redline = down.raised(REDLINE, &[]).await;
    let Edge::Raised(raise) = &redline else {
        unreachable!("a raise")
    };
    assert_eq!(raise.severity, Severity::Critical);
    assert_eq!(raise.message, "too hot");
    down.cleared("redline clears", redline.occurrence()).await;
}
