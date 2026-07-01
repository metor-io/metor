//! Work-Package 7 — the telemetry downlink (telemetry.md §3–§7).
//!
//! [`TelemetrySystem`] is the output registry's first consumer: an ordinary
//! [`CyclicSystem`] registered **last**, so its end-of-cycle `execute` snapshots the
//! freshest record of every tapped output buffer and re-emits each as a metor-proto
//! `Table` packet referencing a once-announced (instance-prefixed) `VTable` — the same
//! wire format `metor-db` ingests. Because a frame's ring payload *is* its table bytes,
//! there is no serialization step; the bytes a system committed are the bytes on the wire.
//!
//! ## Cycle / sender split (telemetry.md §4)
//!
//! The control cycle is single-threaded and synchronous; the socket is async. So the
//! in-cycle stage only *snapshots* (a `memcpy` per tapped buffer with a fresh record)
//! into a bounded, per-tap-coalescing hand-off, and a `stellarator::spawn`ed sender task
//! drains it and does the awaiting I/O. The cycle never blocks on the link: a backed-up
//! transport just causes snapshots to overwrite un-sent ones (latest-wins), bumping a
//! `telemetry.dropped` health counter. Loss, never delay — the same overrun philosophy
//! as the `Overwrite` rings one level down.
//!
//! ## Message downlink (`docs/messages.md` §3)
//!
//! Beside the component-frame snapshots, telemetry also taps every [`MessageRegistry`]
//! channel and downlinks each `(id, postcard)` record as an `OwnedPacket::Msg` — **no
//! announce** (a message is self-describing; its 2-byte id *is* its schema). Messages are
//! an event/command **log**, so they must never be coalesced like the latest-wins
//! component hand-off: the in-cycle stage drains **every** record of each message ring
//! into a separate non-coalescing FIFO ([`MsgHandOff`] — one bounded `VecDeque` shared
//! across all message taps, drop-**oldest** on overflow + a `telemetry.msg_dropped`
//! counter), and the same async sender drains that FIFO and sends each as a `Msg` packet.

use std::collections::VecDeque;
use std::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use metor_fsw_ring::{BoxBacking, NoWake, View};
use metor_proto::types::{ComponentId, LenPacket, Msg, OwnedPacket, PacketId, Timestamp};
use metor_proto::vtable::VTable;
use metor_proto_stellar::{PacketSink, PacketStream};
use metor_proto_wkt::{
    ComponentMetadata, MsgStream, SequenceCommand, SetComponentMetadata, VTableMsg,
};
use stellarator::JoinHandleDropGuard;
use stellarator::buf::Slice;
use stellarator::io::{OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::TcpStream;
use stellarator::sync::WaitQueue;

use crate::binder::{BindPorts, RingSource};
use crate::descriptor::{PortDesc, PortId};
use crate::message::{CommandOut, split_record};
use crate::registry::{AllOutputs, MessageEntry, RegistryEntry};
use crate::system::{AsyncSystem, CyclicSystem, Out, System, SystemInput, SystemOutput};

// ---------------------------------------------------------------------------
// Transport (telemetry.md §7)
// ---------------------------------------------------------------------------

/// An async error a [`Transport`] surfaces; v1 stops downlinking on any error (the
/// in-cycle snapshot stage keeps running and simply drops).
#[derive(Debug)]
pub enum TransportError {
    /// The link is not connected (never established, or dropped).
    Disconnected,
    /// An underlying I/O error, rendered to a string (transport-agnostic).
    Io(String),
}

/// Isolates the wire from the streamer (telemetry.md §7). v1 ships [`TcpTransport`];
/// `bbq`/SHM is a documented future impl. Tests drive an in-memory mock against the
/// same trait.
#[allow(async_fn_in_trait)]
pub trait Transport {
    /// Announce one tap's prefixed vtable + component metadata (sent once on connect):
    /// a `VTableMsg` followed by a `SetComponentMetadata` per component — exactly the
    /// two steps `SinkExt::init_world` does, but with a per-instance prefix.
    async fn announce(
        &mut self,
        msg: &VTableMsg,
        meta: &[ComponentMetadata],
    ) -> Result<(), TransportError>;

    /// Send one `Table` packet referencing an already-announced vtable.
    async fn send(&mut self, pkt: LenPacket) -> Result<(), TransportError>;
}

/// The read twin of [`Transport`] (`docs/messages.md` §4): yields the next inbound
/// `OwnedPacket` off the connection's read half — the uplink's source of panel
/// `SequenceCommand`s. v1 ships [`TcpRecvTransport`] (a [`PacketStream`] over the read
/// half, the inverse of [`Transport`]'s [`PacketSink`]); tests drive an in-memory mock
/// against the same trait. Like the sender, the reader stops on the first error
/// (drop-on-disconnect, telemetry.md §7) — no reconnect.
#[allow(async_fn_in_trait)]
pub trait RecvTransport {
    /// Receive the next packet, or an error if the link dropped.
    async fn recv(&mut self) -> Result<OwnedPacket<Slice<Vec<u8>>>, TransportError>;

    /// Declare the message ids this transport should subscribe to on connect
    /// (`docs/message-wiring.md` §5.2). Called once by the uplink before its first `recv`, with
    /// the ids of its declared message-output ports. The default is a no-op (a mock ignores it).
    fn subscribe(&mut self, _ids: &[PacketId]) {}
}

/// A live TCP connection's write half plus the parked read half. The downlink only writes;
/// the read half is held to keep the full socket open (the downlink does not read replies —
/// the uplink owns its own separate connection, `docs/messages.md` §4.5).
struct TcpConn {
    sink: PacketSink<OwnedWriter<TcpStream>>,
    #[allow(dead_code)]
    rx: OwnedReader<TcpStream>,
}

/// The v1 transport: connect-once to a ground/db endpoint and stream `LenPacket`s,
/// the same path cube-sat's hand-written loop uses (telemetry.md §7). On disconnect it
/// stops downlinking (drop-on-disconnect); automatic reconnect/backoff is future work.
pub struct TcpTransport {
    addr: std::net::SocketAddr,
    conn: Option<TcpConn>,
}

impl TcpTransport {
    /// A transport that will connect to `addr` on its first announce, inside the async
    /// sender task (connecting is async, so it cannot happen at build).
    pub fn new(addr: std::net::SocketAddr) -> Self {
        Self { addr, conn: None }
    }

    /// Connect lazily on first use; subsequent calls reuse the open socket.
    async fn ensure(&mut self) -> Result<&PacketSink<OwnedWriter<TcpStream>>, TransportError> {
        if self.conn.is_none() {
            let stream = TcpStream::connect(self.addr)
                .await
                .map_err(|e| TransportError::Io(format!("{e}")))?;
            let (rx, tx) = stream.split();
            self.conn = Some(TcpConn {
                sink: PacketSink::new(tx),
                rx,
            });
        }
        Ok(&self.conn.as_ref().expect("just connected").sink)
    }
}

impl Transport for TcpTransport {
    async fn announce(
        &mut self,
        msg: &VTableMsg,
        meta: &[ComponentMetadata],
    ) -> Result<(), TransportError> {
        let sink = self.ensure().await?;
        sink.send(msg)
            .await
            .0
            .map_err(|e| TransportError::Io(format!("{e}")))?;
        for m in meta {
            sink.send(&SetComponentMetadata(m.clone()))
                .await
                .0
                .map_err(|e| TransportError::Io(format!("{e}")))?;
        }
        Ok(())
    }

    async fn send(&mut self, pkt: LenPacket) -> Result<(), TransportError> {
        let sink = self.ensure().await?;
        sink.send(pkt)
            .await
            .0
            .map_err(|e| TransportError::Io(format!("{e}")))?;
        Ok(())
    }
}

/// The v1 uplink read path (`docs/messages.md` §4.2/§4.5): the uplink's **own** connection to
/// the metor-db broker, mirroring cube-sat (`examples/cube-sat/src/main.rs:541-559`). It connects
/// lazily on first `recv` (inside the uplink system's async task, as the downlink's
/// [`TcpTransport`] connects on first announce), then — crucially — **subscribes** to the panel's
/// command stream: the db relays a message id only to clients that send a [`MsgStream`] for it
/// (`metor-db` `MsgStream` handler), so without this the uplink would read nothing. The write half
/// (the [`PacketSink`] the subscription is sent over) is held for the connection's lifetime; the
/// read half is a [`PacketStream`] yielding the streamed `SequenceCommand` Msgs. A dropped socket
/// fails `recv` and stops the reader (no reconnect); uplink and downlink no longer share a socket
/// (shared link is deferred, §4.5).
pub struct TcpRecvTransport {
    addr: std::net::SocketAddr,
    stream: Option<PacketStream<OwnedReader<TcpStream>>>,
    /// The write half, held open so the db keeps streaming (it is the channel the [`MsgStream`]
    /// subscription was sent on; dropping it could half-close the socket and end the stream).
    #[allow(dead_code)]
    sink: Option<PacketSink<OwnedWriter<TcpStream>>>,
    /// The message ids to subscribe to on connect (`docs/message-wiring.md` §5.2), set by
    /// [`subscribe`](RecvTransport::subscribe) before the first `recv`. Falls back to
    /// `SequenceCommand::ID` if the uplink never set it.
    subscribe_ids: Vec<PacketId>,
}

impl TcpRecvTransport {
    /// A reader that will connect to `addr` (the metor-db broker) and subscribe on its first
    /// `recv` (connecting is async, so it cannot happen at build). The uplink's own connection,
    /// distinct from the downlink's.
    pub fn new(addr: std::net::SocketAddr) -> Self {
        Self {
            addr,
            stream: None,
            sink: None,
            subscribe_ids: Vec::new(),
        }
    }

    /// Connect lazily on first use, send the `SequenceCommand` [`MsgStream`] subscription, then
    /// reuse the open read half. Subsequent calls reuse the established stream.
    async fn ensure(&mut self) -> Result<&mut PacketStream<OwnedReader<TcpStream>>, TransportError> {
        if self.stream.is_none() {
            let stream = TcpStream::connect(self.addr)
                .await
                .map_err(|e| TransportError::Io(format!("{e}")))?;
            let (rx, tx) = stream.split();
            // Subscribe to the panel's command stream before reading (`docs/messages.md` §4.4):
            // the db only forwards a message id to clients that asked for it. Mirrors cube-sat
            // (`examples/cube-sat/src/main.rs:555`). v1 subscribes to `SequenceCommand` only.
            let sink = PacketSink::new(tx);
            // Subscribe to every id the uplink declared (its message-output ports, §5.2), or
            // `SequenceCommand` if it never set them (the historical default).
            let ids: Vec<PacketId> = if self.subscribe_ids.is_empty() {
                vec![SequenceCommand::ID]
            } else {
                self.subscribe_ids.clone()
            };
            for msg_id in ids {
                sink.send(&MsgStream { msg_id })
                    .await
                    .0
                    .map_err(|e| TransportError::Io(format!("{e}")))?;
            }
            self.sink = Some(sink);
            self.stream = Some(PacketStream::new(rx));
        }
        Ok(self.stream.as_mut().expect("just connected"))
    }
}

impl RecvTransport for TcpRecvTransport {
    async fn recv(&mut self) -> Result<OwnedPacket<Slice<Vec<u8>>>, TransportError> {
        let stream = self.ensure().await?;
        stream
            .next_grow(vec![0u8; 1024])
            .await
            .map_err(|e| TransportError::Io(format!("{e}")))
    }

    fn subscribe(&mut self, ids: &[PacketId]) {
        self.subscribe_ids = ids.to_vec();
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which outputs to tap (telemetry.md §3).
#[derive(Clone, Debug)]
pub enum TelemetryMode {
    /// Tap every registry entry: every system's user frames *and* their implicit
    /// `health`/`log`, plus the coordinator-owned `health`/`log`/`status`.
    All,
    /// Tap only the entries whose instance name or frame name matches the configured
    /// lists (matching either is enough).
    Subset {
        instances: Vec<String>,
        frames: Vec<String>,
    },
}

impl TelemetryMode {
    fn matches(&self, entry: &RegistryEntry) -> bool {
        match self {
            TelemetryMode::All => true,
            TelemetryMode::Subset { instances, frames } => {
                instances.iter().any(|i| i.as_str() == &*entry.instance)
                    || frames.iter().any(|f| ComponentId::new(f) == entry.frame_id)
            }
        }
    }

    /// The message-channel twin of [`matches`](Self::matches). A message channel has no
    /// frame type, so the `frames` subset list is matched against the channel name (the
    /// message analogue of a frame id); instance matching is identical.
    fn matches_message(&self, entry: &MessageEntry) -> bool {
        match self {
            TelemetryMode::All => true,
            TelemetryMode::Subset { instances, frames } => {
                instances.iter().any(|i| i.as_str() == &*entry.instance)
                    || frames.iter().any(|f| f.as_str() == &*entry.channel)
            }
        }
    }
}

/// The telemetry downlink configuration handed to
/// [`add_telemetry`](crate::CoordinatorBuilder::add_telemetry).
pub struct TelemetryConfig<T: Transport> {
    /// Where the snapshots go (TCP in v1, a mock in tests).
    pub transport: T,
    /// Which outputs to tap.
    pub mode: TelemetryMode,
}

// ---------------------------------------------------------------------------
// Hand-off (the bounded, per-tap-coalescing queue, telemetry.md §4)
// ---------------------------------------------------------------------------

/// One pending packet slot per tap: a newer snapshot overwrites an older un-sent one
/// (latest-wins coalescing — the Overwrite ring semantics, one level up). Overwriting an
/// *occupied* slot means a snapshot was dropped (the link is backed up), bumping `dropped`.
struct HandOff {
    slots: Mutex<Vec<Option<LenPacket>>>,
    /// Set when a slot is filled; cleared by the sender when it parks. Just avoids busy
    /// spinning — a missed wake is harmless because the sender always drains *all* slots.
    pending: AtomicBool,
    /// Count of snapshots dropped by overwrite (surfaced as `telemetry.dropped` health).
    dropped: AtomicU64,
    /// Wakes the parked sender when a snapshot is pushed (or on shutdown). Shared with the
    /// [`MsgHandOff`] so either hand-off can rouse the one sender task.
    wq: Arc<WaitQueue>,
}

impl HandOff {
    fn new(n_taps: usize, wq: Arc<WaitQueue>) -> Self {
        Self {
            slots: Mutex::new((0..n_taps).map(|_| None).collect()),
            pending: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            wq,
        }
    }

    /// Cycle side (never blocks): coalesce `pkt` into `slot`, counting a drop if it
    /// overwrote an un-sent packet, then wake the sender.
    fn push(&self, slot: usize, pkt: LenPacket) {
        {
            let mut slots = self.slots.lock().expect("handoff poisoned");
            if slots[slot].is_some() {
                self.dropped.fetch_add(1, Relaxed);
            }
            slots[slot] = Some(pkt);
        }
        self.pending.store(true, Release);
        self.wq.wake_all();
    }

    /// Sender side: take every pending packet (drops the lock before any `.await`).
    fn drain(&self) -> Vec<LenPacket> {
        let mut slots = self.slots.lock().expect("handoff poisoned");
        slots.iter_mut().filter_map(Option::take).collect()
    }
}

/// The bound on the shared message FIFO ([`MsgHandOff`]). A backed-up link drops the
/// **oldest** queued message past this depth (and counts it), bounding memory while
/// keeping the link's most recent log of events/commands.
const MSG_HANDOFF_CAP: usize = 1024;

/// The **non-coalescing** message hand-off (`docs/messages.md` §3.2): one bounded FIFO
/// shared across *all* message taps. Unlike the latest-wins component [`HandOff`], a
/// message is an event/command log record — it must not be coalesced — so every drained
/// record is appended in FIFO order and forwarded verbatim. Cross-channel order is
/// irrelevant (each `Msg` self-addresses by id), so one shared queue suffices. On
/// overflow the **oldest** record is dropped and `dropped` bumped (surfaced as
/// `telemetry.msg_dropped`).
struct MsgHandOff {
    queue: Mutex<VecDeque<LenPacket>>,
    /// Set when a record is queued; cleared by the sender when it parks (a missed wake is
    /// harmless — the sender always drains the whole queue).
    pending: AtomicBool,
    /// Count of records dropped-oldest on overflow (surfaced as `telemetry.msg_dropped`).
    dropped: AtomicU64,
    /// The same [`WaitQueue`] the component [`HandOff`] holds, so a queued message wakes
    /// the one shared sender task.
    wq: Arc<WaitQueue>,
}

impl MsgHandOff {
    fn new(wq: Arc<WaitQueue>) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            pending: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            wq,
        }
    }

    /// Cycle side (never blocks): append `pkt`, dropping the oldest queued record (and
    /// counting it) if the FIFO is already at [`MSG_HANDOFF_CAP`], then wake the sender.
    fn push(&self, pkt: LenPacket) {
        {
            let mut queue = self.queue.lock().expect("msg handoff poisoned");
            if queue.len() >= MSG_HANDOFF_CAP {
                queue.pop_front();
                self.dropped.fetch_add(1, Relaxed);
            }
            queue.push_back(pkt);
        }
        self.pending.store(true, Release);
        self.wq.wake_all();
    }

    /// Sender side: take every queued record in FIFO order (drops the lock before any
    /// `.await`).
    fn drain(&self) -> Vec<LenPacket> {
        let mut queue = self.queue.lock().expect("msg handoff poisoned");
        queue.drain(..).collect()
    }
}

// ---------------------------------------------------------------------------
// Uplink — the command-plane ingest system (`docs/messages.md` §4.4)
// ---------------------------------------------------------------------------

/// Idle pause one [`UplinkSystem::run`] pass takes once its link has dropped, so the
/// coordinator's async-run loop does not busy-spin a dead reader (teardown cancels the task
/// via its drop guard regardless).
const UPLINK_IDLE: Duration = Duration::from_millis(50);

/// The uplink's output bundle (`docs/message-wiring.md` §6): a single ordinary
/// [`CommandOut<SequenceCommand>`](CommandOut) message output it re-emits each decoded
/// `SequenceCommand` onto — a fully normal message producer, no bespoke command-bus capability.
/// The coordinator collects every `SequenceCommand` output as a command producer every slot fans
/// in from. Untelemetered (inbound control), so it is never echoed on the downlink.
pub struct UplinkPorts {
    commands: CommandOut<SequenceCommand>,
}

impl SystemOutput for UplinkPorts {
    fn descriptors() -> Vec<PortDesc> {
        vec![CommandOut::<SequenceCommand>::descriptor()]
    }
}

impl BindPorts<BoxBacking> for UplinkPorts {
    fn bind<S: RingSource<B = BoxBacking>>(src: &mut S) -> Self {
        Self {
            commands: CommandOut::bind(src),
        }
    }
}

/// The uplink's (empty) input bundle — it sources commands from its own connection, not edges.
pub struct UplinkIn;

impl SystemInput for UplinkIn {
    fn descriptors() -> Vec<PortDesc> {
        Vec::new()
    }
    fn any_lapped(&self) -> bool {
        false
    }
}

impl BindPorts<BoxBacking> for UplinkIn {
    fn bind<S: RingSource<B = BoxBacking>>(_src: &mut S) -> Self {
        UplinkIn
    }
}

/// The command-plane ingest system (`docs/messages.md` §4.4): the read twin of
/// [`TelemetrySystem`], an ordinary [`AsyncSystem`] that owns its **own** [`RecvTransport`]
/// connection (§4.5) and re-emits each panel `SequenceCommand` onto the command bus the
/// coordinator drains at head-of-cycle. A near pass-through — wire and internal command type
/// are now identical (`SequenceCommand`), so there is no mapping, no inbox, no translation.
pub struct UplinkSystem<R: RecvTransport> {
    /// The read transport, taken `None` on drop-on-disconnect (no reconnect, telemetry.md §7).
    recv: Option<R>,
    /// Whether the ground subscription has been sent (once, before the first `recv`).
    subscribed: bool,
}

impl<R: RecvTransport> UplinkSystem<R> {
    /// Construct the (pre-init) uplink from its read transport (its own connection).
    pub fn new(recv: R) -> Self {
        Self {
            recv: Some(recv),
            subscribed: false,
        }
    }
}

/// The message ids the uplink subscribes to on the ground: the `PacketId`s of its declared
/// message-output ports (`docs/message-wiring.md` §5.2). Derived from the bundle descriptor, so a
/// future uplink that forwards a second command type just declares a second output — the
/// subscription follows automatically.
fn uplink_subscribe_ids() -> Vec<PacketId> {
    <UplinkPorts as SystemOutput>::descriptors()
        .iter()
        .filter_map(|p| match p.id {
            PortId::Msg(id) => Some(id),
            _ => None,
        })
        .collect()
}

impl<R: RecvTransport + 'static> System for UplinkSystem<R> {
    type Input = UplinkIn;
    type Output = Out<UplinkPorts>;
    const NAME: &'static str = "uplink";
}

impl<R: RecvTransport + 'static> AsyncSystem for UplinkSystem<R> {
    /// One ingest pass (the coordinator's async-run loop calls it repeatedly): `recv` the next
    /// packet, keep only `SequenceCommand` Msgs (cube-sat's exact filter,
    /// `examples/cube-sat/src/main.rs:690`), and `emit` each onto the command channel; ignore any
    /// other packet. On the first error the link is dropped (no reconnect) and subsequent passes
    /// idle so the loop does not spin a dead reader.
    async fn run(&mut self, _input: &mut Self::Input, output: &mut Self::Output) {
        // Subscribe once, before the first read, to the ids of the uplink's declared outputs.
        if !self.subscribed {
            let ids = uplink_subscribe_ids();
            if let Some(recv) = self.recv.as_mut() {
                recv.subscribe(&ids);
            }
            self.subscribed = true;
        }
        let Some(recv) = self.recv.as_mut() else {
            stellarator::sleep(UPLINK_IDLE).await;
            return;
        };
        match recv.recv().await {
            Ok(OwnedPacket::Msg(m)) if m.id == SequenceCommand::ID => {
                if let Ok(cmd) = m.parse::<SequenceCommand>() {
                    let _ = output.commands.emit(&cmd);
                }
            }
            // Other Msgs / Tables are ignored — the uplink is commands only.
            Ok(_) => {}
            // A dropped link (or an exhausted mock) ends the reader, like the sender.
            Err(_) => self.recv = None,
        }
    }
}

/// One announced tap's wire schema, moved into the sender task to replay on connect.
struct Announce {
    packet_id: PacketId,
    vtable: VTable,
    metadata: Vec<ComponentMetadata>,
}

/// The async sender task (telemetry.md §4): announce every tap once on connect, then
/// drain the hand-off and send `Table` packets. Stops downlinking on any transport error
/// or when `stop` is set (the cycle is unaffected either way).
async fn run_sender<T: Transport>(
    mut transport: T,
    announces: Vec<Announce>,
    handoff: Arc<HandOff>,
    msg_handoff: Arc<MsgHandOff>,
    wq: Arc<WaitQueue>,
    stop: Arc<AtomicBool>,
) {
    for a in &announces {
        let msg = VTableMsg {
            id: a.packet_id,
            vtable: a.vtable.clone(),
        };
        if transport.announce(&msg, &a.metadata).await.is_err() {
            return;
        }
    }
    loop {
        if stop.load(Acquire) {
            return;
        }
        // Component snapshots (latest-wins) and message-log records (FIFO) share the one
        // sender; drain both, park on the shared wait queue only when both are empty.
        let pkts = handoff.drain();
        let msgs = msg_handoff.drain();
        if pkts.is_empty() && msgs.is_empty() {
            let _ = wq
                .wait_for(|| {
                    // Clear *both* pending flags (non-short-circuiting) so neither hand-off
                    // leaves a stale set bit, then also honor `stop`.
                    let woke = handoff.pending.swap(false, AcqRel);
                    let woke_msg = msg_handoff.pending.swap(false, AcqRel);
                    woke || woke_msg || stop.load(Acquire)
                })
                .await;
            continue;
        }
        for pkt in pkts {
            if transport.send(pkt).await.is_err() {
                return;
            }
        }
        // Messages carry no announce — `Transport::send` writes the `Msg` packet verbatim.
        for pkt in msgs {
            if transport.send(pkt).await.is_err() {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ports (no typed inputs; the output bundle carries the registry handle)
// ---------------------------------------------------------------------------

/// The telemetry system has no typed inputs (telemetry.md §3); its output bundle is a single
/// [`AllOutputs`] receive-all tap (`docs/message-wiring.md` §4) — the reusable generalization
/// of what this bundle used to hand-pull. `init` reaches both registries through it, and the
/// `ReceiveAll` port is what earns the downlink a reader slot on every buffer.
pub struct TelemetryPorts {
    all: AllOutputs,
}

impl SystemOutput for TelemetryPorts {
    fn descriptors() -> Vec<PortDesc> {
        vec![AllOutputs::descriptor()]
    }
}

impl BindPorts<BoxBacking> for TelemetryPorts {
    /// Host-only (`B = BoxBacking`): the receive-all tap pulls both broad registries the host
    /// [`Binder`] carries. The telemetry downlink is never dlopen'd.
    fn bind<S: RingSource<B = BoxBacking>>(src: &mut S) -> Self {
        Self {
            all: AllOutputs::bind(src),
        }
    }
}

/// The empty input bundle (the streamer pulls outputs via the registry, not typed edges).
pub struct TelemetryIn;

impl SystemInput for TelemetryIn {
    fn descriptors() -> Vec<PortDesc> {
        Vec::new()
    }
    fn any_lapped(&self) -> bool {
        false
    }
}

impl BindPorts<BoxBacking> for TelemetryIn {
    fn bind<S: RingSource<B = BoxBacking>>(_src: &mut S) -> Self {
        TelemetryIn
    }
}

// ---------------------------------------------------------------------------
// The system
// ---------------------------------------------------------------------------

/// One resolved tap: a read view into a buffer, its assigned packet id, and the reused
/// snapshot scratch.
struct Tap {
    slot: usize,
    packet_id: PacketId,
    view: View<BoxBacking, NoWake, NoWake>,
    scratch: Vec<u8>,
}

/// One resolved **message** tap: a read view into a message channel plus the reused
/// record scratch. Unlike a [`Tap`] it carries no `packet_id` — each record's id is the
/// first two bytes ([`split_record`]), so the downlink reads the id from the record
/// itself rather than from a build-time assignment.
struct MsgTap {
    view: View<BoxBacking, NoWake, NoWake>,
    scratch: Vec<u8>,
}

/// The telemetry downlink system (telemetry.md §3). Generic over the [`Transport`]; the
/// concrete `T` is chosen by the builder/loader (TCP) or a test (mock).
pub struct TelemetrySystem<T: Transport> {
    transport: Option<T>,
    mode: TelemetryMode,
    taps: Vec<Tap>,
    /// The message-channel taps (`docs/messages.md` §3): every record drained, never
    /// coalesced, into the shared [`MsgHandOff`].
    msg_taps: Vec<MsgTap>,
    handoff: Option<Arc<HandOff>>,
    msg_handoff: Option<Arc<MsgHandOff>>,
    stop: Option<Arc<AtomicBool>>,
    /// Holds the sender task; its `Drop` cancels the task on teardown.
    #[allow(dead_code)]
    sender: Option<JoinHandleDropGuard<()>>,
    /// How many drops we have already surfaced to health (so each new one is reported once).
    last_dropped: u64,
    /// The message-FIFO twin of `last_dropped` (surfaced as `telemetry.msg_dropped`).
    last_msg_dropped: u64,
}

impl<T: Transport> TelemetrySystem<T> {
    /// Construct the (pre-init) downlink from its config. Taps are resolved and the
    /// sender spawned at `init`, where the registry handle is reachable.
    pub fn new(config: TelemetryConfig<T>) -> Self {
        Self {
            transport: Some(config.transport),
            mode: config.mode,
            taps: Vec::new(),
            msg_taps: Vec::new(),
            handoff: None,
            msg_handoff: None,
            stop: None,
            sender: None,
            last_dropped: 0,
            last_msg_dropped: 0,
        }
    }
}

impl<T: Transport + 'static> System for TelemetrySystem<T> {
    type Input = TelemetryIn;
    type Output = Out<TelemetryPorts>;
    const NAME: &'static str = "telemetry";

    /// Resolve the tap set, claim one read `View` per tapped buffer, and spawn the async
    /// sender (telemetry.md §3/§4). `init` runs on the coordinator's loop task within
    /// `start()`, so `stellarator::spawn` has a runtime; the sender announces before any
    /// data is queued (nothing is pushed until the first `execute`).
    fn init(&mut self, output: &mut Self::Output) {
        let registry = output.all.outputs.clone();
        let messages = output.all.messages.clone();
        let mut taps = Vec::new();
        let mut announces = Vec::new();
        for entry in registry.entries() {
            if !self.mode.matches(entry) {
                continue;
            }
            let view = match entry.view() {
                Ok(v) => v,
                // Reader-slot budget exhausted: surface it and skip this tap rather than
                // panicking (telemetry.md §2.5). Build-time sizing makes this unreachable
                // for the known consumers, but a hand-built over-subscription is observable.
                Err(_) => {
                    output.health().error("telemetry.reader_slot");
                    continue;
                }
            };
            let slot = taps.len();
            let packet_id = (slot as u16).to_le_bytes();
            taps.push(Tap {
                slot,
                packet_id,
                view,
                scratch: Vec::new(),
            });
            announces.push(Announce {
                packet_id,
                vtable: entry.vtable.clone(),
                metadata: entry.metadata.clone(),
            });
        }

        // Message taps (`docs/messages.md` §3): claim a view per channel, **no announce**
        // (a message is self-describing — its id is its schema). A lapped/over-subscribed
        // reader slot is surfaced and skipped exactly like an output tap.
        let mut msg_taps = Vec::new();
        for entry in messages.entries() {
            // Command channels (`telemetered = false`) are inbound control — never downlinked
            // (`docs/message-wiring.md` §6.4).
            if !entry.telemetered || !self.mode.matches_message(entry) {
                continue;
            }
            let view = match entry.view() {
                Ok(v) => v,
                Err(_) => {
                    output.health().error("telemetry.reader_slot");
                    continue;
                }
            };
            msg_taps.push(MsgTap {
                view,
                scratch: Vec::new(),
            });
        }

        // One wait queue shared by both hand-offs wakes the single sender task.
        let wq = Arc::new(WaitQueue::new());
        let handoff = Arc::new(HandOff::new(taps.len(), wq.clone()));
        let msg_handoff = Arc::new(MsgHandOff::new(wq.clone()));
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(transport) = self.transport.take() {
            let handle = stellarator::spawn(run_sender(
                transport,
                announces,
                handoff.clone(),
                msg_handoff.clone(),
                wq,
                stop.clone(),
            ));
            self.sender = Some(handle.drop_guard());
        }
        self.taps = taps;
        self.msg_taps = msg_taps;
        self.handoff = Some(handoff);
        self.msg_handoff = Some(msg_handoff);
        self.stop = Some(stop);
    }

    /// Signal the sender to stop and wake it so it exits cooperatively (the drop guard
    /// cancels it regardless when the system is dropped at coordinator teardown).
    fn shutdown(&mut self, _output: &mut Self::Output) {
        if let Some(stop) = &self.stop {
            stop.store(true, Release);
        }
        if let Some(handoff) = &self.handoff {
            handoff.wq.wake_all();
        }
    }
}

impl<T: Transport + 'static> CyclicSystem for TelemetrySystem<T> {
    /// End-of-cycle snapshot (telemetry.md §3/§4): for each tap with a fresh record,
    /// copy the latest table bytes into a `LenPacket` and hand it to the sender; never
    /// awaits. Any drops detected since last cycle are surfaced as `telemetry.dropped`.
    /// Message taps are drained too — **every** record, non-coalescing — into the shared
    /// [`MsgHandOff`] (`docs/messages.md` §3).
    fn execute(&mut self, _now: Timestamp, _input: &mut Self::Input, output: &mut Self::Output) {
        let handoff = match &self.handoff {
            Some(h) => h.clone(),
            None => return,
        };
        for tap in &mut self.taps {
            // Drain to the newest committed record (latest-wins); a lapped view skips to
            // the live edge. No new record this cycle ⇒ nothing to send for this tap.
            let mut got = false;
            loop {
                match tap.view.try_read_into(&mut tap.scratch) {
                    Ok(true) => got = true,
                    Ok(false) => break,
                    Err(_) => {
                        tap.view.resync();
                        break;
                    }
                }
            }
            if got {
                let mut pkt = LenPacket::table(tap.packet_id, tap.scratch.len());
                pkt.extend_from_slice(&tap.scratch);
                handoff.push(tap.slot, pkt);
            }
        }
        // Surface any snapshots the sender's backlog forced us to drop (telemetry.md §4).
        let dropped = handoff.dropped.load(Relaxed);
        while self.last_dropped < dropped {
            output.health().error("telemetry.dropped");
            self.last_dropped += 1;
        }

        // Message taps: drain **every** record (the `Input::drain` shape, not latest-wins),
        // re-frame each `(id, payload)` as a `Msg` packet, and FIFO it to the sender. A
        // lapped view skips to the live edge (the cycle never blocks); no announce.
        if let Some(msg_handoff) = &self.msg_handoff {
            let msg_handoff = msg_handoff.clone();
            for tap in &mut self.msg_taps {
                loop {
                    match tap.view.try_read_into(&mut tap.scratch) {
                        Ok(true) => {
                            if let Some((id, payload)) = split_record(&tap.scratch) {
                                let mut pkt = LenPacket::msg(id, payload.len());
                                pkt.extend_from_slice(payload);
                                msg_handoff.push(pkt);
                            }
                        }
                        Ok(false) => break,
                        Err(_) => {
                            tap.view.resync();
                            break;
                        }
                    }
                }
            }
            // Surface any records the FIFO dropped-oldest on overflow.
            let msg_dropped = msg_handoff.dropped.load(Relaxed);
            while self.last_msg_dropped < msg_dropped {
                output.health().error("telemetry.msg_dropped");
                self.last_msg_dropped += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests;
