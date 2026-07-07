//! Streams output buffers to a ground endpoint and relays ground commands
//! back in.
//!
//! Two systems live here. [`TelemetrySystem`] is the downlink, an ordinary
//! [`CyclicSystem`] registered after every other cyclic system. Its
//! end-of-cycle `execute` copies the pending records of every tapped output
//! buffer into a bounded hand-off, and a spawned sender task drains the
//! hand-off onto the wire. [`UplinkSystem`] is the read twin, an
//! [`AsyncSystem`] that owns its own connection, subscribes to a configured
//! set of message ids, and republishes each inbound `Msg` packet on an
//! ordinary message output port, so consumers receive commands over explicit
//! edges like any other message.
//!
//! # Cycle / sender split
//!
//! The control cycle is single-threaded and synchronous while the socket is
//! async, so the downlink is split in two. The in-cycle stage only copies
//! records into the hand-off and never blocks on the link; the sender task
//! does all the awaiting I/O. When the transport backs up, data is dropped
//! rather than delayed, and every drop is counted on the system's health
//! output.
//!
//! # Taps, lanes, and wire framing
//!
//! Each tapped registry entry becomes one tap, and the entry's two axes drive
//! it independently. Its [`Delivery`](crate::Delivery) picks the hand-off
//! lane:
//!
//! * `Snapshot` entries get a dedicated coalescing slot. Only the newest
//!   record of a cycle is pushed, a newer snapshot overwrites an older un-sent
//!   one, and each overwrite of an occupied slot bumps the `telemetry.dropped`
//!   counter.
//! * `Log` entries share one bounded FIFO. Event and command records must
//!   never be coalesced, so every drained record is queued in order and sent
//!   verbatim. Overflow drops the oldest queued record and bumps
//!   `telemetry.msg_dropped`.
//!
//! The entry's schema picks the wire framing. A table entry is sent as a
//! `Table` packet whose payload is the ring record itself (the bytes a system
//! committed are the bytes on the wire), referencing a `VTable` announced once
//! on connect. A postcard entry is sent as a self-describing `Msg` packet
//! whose id is the record's leading two bytes, with no announce step. The two
//! axes combine freely; an every-record table log needs no extra code.
//!
//! # Connection lifecycle
//!
//! Both TCP transports connect lazily on first use, inside the async task that
//! drives them, and neither reconnects. The first transport error ends the
//! sender (or reader) while the in-cycle stage keeps running and drops.

use std::collections::VecDeque;
use std::sync::atomic::{
    AtomicBool, AtomicU64,
    Ordering::{AcqRel, Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use metor_fsw_ring::{NoWake, View};
use metor_proto::types::{LenPacket, OwnedPacket, PacketId, Timestamp};
use metor_proto::vtable::VTable;
use metor_proto_stellar::{PacketSink, PacketStream};
use metor_proto_wkt::{ComponentMetadata, MsgStream, SetComponentMetadata, VTableMsg};
use stellarator::JoinHandleDropGuard;
use stellarator::buf::Slice;
use stellarator::io::{OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::TcpStream;
use stellarator::sync::WaitQueue;

use crate::binder::{BindPorts, RingSource};
use crate::descriptor::{Delivery, PortDecl, PortDesc, SystemDescriptor};
use crate::health::HealthPort;
use crate::message::{MsgFanOut, NamedMsg, split_record};
use crate::registry::{AllOutputs, EntrySchema, RegistryEntry};
use crate::system::{
    AsyncSystem, BuildCtx, BuildSystem, ConfigureError, CyclicSystem, Out, System, SystemInput,
    SystemOutput,
};

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A failed send or receive on the link. The first error stops the task
/// driving the transport, while the in-cycle stage keeps running and drops.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The link is not connected (never established, or dropped).
    #[error("transport is not connected")]
    Disconnected,
    /// An underlying I/O error, boxed so the source chain survives a `?` into
    /// a caller's error type instead of flattening to a string.
    #[error("transport I/O error: {0}")]
    Io(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl TransportError {
    /// Box any transport-level error into [`Io`](TransportError::Io).
    fn io(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        TransportError::Io(Box::new(e))
    }
}

/// The downlink's write side, isolating the wire from the streaming logic.
/// [`TcpTransport`] is the shipped implementation; tests drive an in-memory
/// mock against the same trait.
#[allow(async_fn_in_trait)]
pub trait Transport {
    /// Announce one tap's schema, a `VTableMsg` followed by one
    /// `SetComponentMetadata` per component. Sent once per tap on connect.
    async fn announce(
        &mut self,
        msg: &VTableMsg,
        meta: &[ComponentMetadata],
    ) -> Result<(), TransportError>;

    /// Send one `Table` packet referencing an already-announced vtable.
    async fn send(&mut self, pkt: LenPacket) -> Result<(), TransportError>;
}

/// The read twin of [`Transport`]: yields the next inbound packet off the
/// connection. Like the sender, a reader stops on its first error; there is no
/// reconnect.
#[allow(async_fn_in_trait)]
pub trait RecvTransport {
    /// Receive the next packet, or an error if the link dropped.
    async fn recv(&mut self) -> Result<OwnedPacket<Slice<Vec<u8>>>, TransportError>;

    /// Declare the message ids to subscribe to on connect. Called once before
    /// the first `recv`; the default is a no-op.
    fn subscribe(&mut self, _ids: &[PacketId]) {}
}

/// A live connection's write half plus the parked read half, which is held
/// only to keep the full socket open; the downlink never reads replies.
struct TcpConn {
    sink: PacketSink<OwnedWriter<TcpStream>>,
    #[allow(dead_code)]
    rx: OwnedReader<TcpStream>,
}

/// A [`Transport`] that streams packets over one TCP connection to a ground
/// endpoint. On the first error the sender stops; there is no reconnect.
pub struct TcpTransport {
    addr: std::net::SocketAddr,
    conn: Option<TcpConn>,
}

impl TcpTransport {
    /// A transport that connects to `addr` on its first announce, inside the
    /// async sender task (connecting is async, so it cannot happen at build).
    pub fn new(addr: std::net::SocketAddr) -> Self {
        Self { addr, conn: None }
    }

    /// Connect on first use; later calls reuse the open socket.
    async fn ensure(&mut self) -> Result<&PacketSink<OwnedWriter<TcpStream>>, TransportError> {
        if self.conn.is_none() {
            let stream = TcpStream::connect(self.addr)
                .await
                .map_err(TransportError::io)?;
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
        sink.send(msg).await.0.map_err(TransportError::io)?;
        for m in meta {
            sink.send(&SetComponentMetadata(m.clone()))
                .await
                .0
                .map_err(TransportError::io)?;
        }
        Ok(())
    }

    async fn send(&mut self, pkt: LenPacket) -> Result<(), TransportError> {
        let sink = self.ensure().await?;
        sink.send(pkt).await.0.map_err(TransportError::io)?;
        Ok(())
    }
}

/// A [`RecvTransport`] that reads packets over the uplink's own TCP
/// connection, distinct from the downlink's. It connects lazily on the first
/// `recv`, then subscribes to the configured message ids; the broker relays a
/// message id only to clients that
/// sent a [`MsgStream`] for it, so without the subscription the uplink would
/// read nothing. The write half carries the subscription and is held for the
/// connection's lifetime; the read half yields the streamed msgs. A dropped
/// socket fails `recv` and ends the reader; there is no reconnect.
pub struct TcpRecvTransport {
    addr: std::net::SocketAddr,
    stream: Option<PacketStream<OwnedReader<TcpStream>>>,
    /// The write half, held open because it carried the subscription; dropping
    /// it could half-close the socket and end the stream.
    #[allow(dead_code)]
    sink: Option<PacketSink<OwnedWriter<TcpStream>>>,
    /// The message ids to subscribe to on connect, set by
    /// [`subscribe`](RecvTransport::subscribe) before the first `recv`. Empty
    /// subscribes to nothing.
    subscribe_ids: Vec<PacketId>,
}

impl TcpRecvTransport {
    /// A reader that connects to `addr` and subscribes on its first `recv`
    /// (connecting is async, so it cannot happen at build).
    pub fn new(addr: std::net::SocketAddr) -> Self {
        Self {
            addr,
            stream: None,
            sink: None,
            subscribe_ids: Vec::new(),
        }
    }

    /// Connect on first use and send one [`MsgStream`] subscription per
    /// configured id; later calls reuse the open stream.
    async fn ensure(
        &mut self,
    ) -> Result<&mut PacketStream<OwnedReader<TcpStream>>, TransportError> {
        if self.stream.is_none() {
            let stream = TcpStream::connect(self.addr)
                .await
                .map_err(TransportError::io)?;
            let (rx, tx) = stream.split();
            // Subscribe before reading anything; the broker only forwards a
            // message id to clients that asked for it.
            let sink = PacketSink::new(tx);
            for msg_id in self.subscribe_ids.clone() {
                sink.send(&MsgStream { msg_id })
                    .await
                    .0
                    .map_err(TransportError::io)?;
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
            .map_err(TransportError::io)
    }

    fn subscribe(&mut self, ids: &[PacketId]) {
        self.subscribe_ids = ids.to_vec();
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which registry entries the downlink taps.
#[derive(Clone, Debug)]
pub enum TelemetryMode {
    /// Tap every entry: every system's user frames and their implicit
    /// `health`/`log`, plus the coordinator-owned `health`/`log`/`status`.
    All,
    /// Tap only the entries whose instance name or frame name appears in the
    /// configured lists; matching either is enough.
    Subset {
        instances: Vec<String>,
        frames: Vec<String>,
    },
}

impl TelemetryMode {
    /// Whether `entry` is tapped. The `frames` list matches
    /// [`RegistryEntry::name`], which covers frame names and channel names
    /// alike.
    fn matches(&self, entry: &RegistryEntry) -> bool {
        match self {
            TelemetryMode::All => true,
            TelemetryMode::Subset { instances, frames } => {
                instances.iter().any(|i| i.as_str() == &*entry.instance)
                    || frames.iter().any(|f| f.as_str() == &*entry.name)
            }
        }
    }
}

/// The transport and tap selection a [`TelemetrySystem`] is built from.
/// Programmatic users build one directly and register the system like any
/// other: `builder.add_cyclic(TelemetrySystem::new(config))`.
pub struct TelemetryConfig<T: Transport> {
    /// Where the snapshots go.
    pub transport: T,
    /// Which outputs to tap.
    pub mode: TelemetryMode,
}

/// Wiring parameters for the built-in TCP downlink (`type="TcpDownlink"`): the
/// ground address plus an optional tap subset. With both lists absent every
/// entry is tapped; with either present an entry is tapped when its instance
/// or its frame/channel name is listed.
///
/// ```kdl
/// system "telemetry" type="TcpDownlink" addr="127.0.0.1:2240" {
///     instances "nav" "imu"      // optional; omit both children to tap everything
///     frames "gyro_b"
/// }
/// ```
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct DownlinkParams {
    /// The TCP address of the ground endpoint the downlink streams to.
    pub addr: std::net::SocketAddr,
    /// Instance names to tap; `None` (with `frames` also `None`) taps everything.
    #[serde(default)]
    pub instances: Option<Vec<String>>,
    /// Frame/channel names to tap.
    #[serde(default)]
    pub frames: Option<Vec<String>>,
}

impl DownlinkParams {
    /// Project the two optional subset lists onto a [`TelemetryMode`].
    fn mode(&self) -> TelemetryMode {
        match (&self.instances, &self.frames) {
            (None, None) => TelemetryMode::All,
            (instances, frames) => TelemetryMode::Subset {
                instances: instances.clone().unwrap_or_default(),
                frames: frames.clone().unwrap_or_default(),
            },
        }
    }
}

impl BuildSystem for TelemetrySystem<TcpTransport> {
    type Params = DownlinkParams;

    fn new(params: DownlinkParams) -> Self {
        let mode = params.mode();
        Self::new(TelemetryConfig {
            transport: TcpTransport::new(params.addr),
            mode,
        })
    }
}

/// Wiring parameters for the built-in TCP uplink (`type="TcpUplink"`): the
/// broker address plus the message types to relay. Each `msgs` token is a
/// [`NamedMsg::NAME`] resolved against the registry's
/// [`MsgTable`](crate::MsgTable); the uplink subscribes to exactly those ids
/// and mints one ordinary message output port per msg, so
/// `connect "uplink" -> … msg="…"` edges resolve like any other. No `msgs`
/// child means the uplink relays nothing (and warns); there is no built-in
/// default set.
///
/// ```kdl
/// system "uplink" type="TcpUplink" addr="127.0.0.1:2241" {
///     msgs "SequenceCommand" "AlarmAck"
/// }
/// ```
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct UplinkParams {
    /// The TCP address of the broker the uplink subscribes to.
    pub addr: std::net::SocketAddr,
    /// The [`NamedMsg::NAME`] tokens of the messages to relay.
    #[serde(default)]
    pub msgs: Option<Vec<String>>,
}

impl BuildSystem for UplinkSystem<TcpRecvTransport> {
    type Params = UplinkParams;

    fn new(params: UplinkParams) -> Self {
        let mut system = Self::new(TcpRecvTransport::new(params.addr));
        system.unresolved = params.msgs.unwrap_or_default();
        system
    }

    fn configure(&mut self, ctx: &BuildCtx) -> Result<(), ConfigureError> {
        for token in std::mem::take(&mut self.unresolved) {
            let Some((name, id)) = ctx.msgs.get(&token) else {
                return Err(ConfigureError::UnknownMsg {
                    name: token,
                    available: ctx.msgs.names(),
                });
            };
            // A duplicate token folds into one port instead of a duplicate-key
            // error.
            if !self.msgs.iter().any(|&(_, existing)| existing == id) {
                self.msgs.push((name, id));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hand-off
// ---------------------------------------------------------------------------

/// The bound on the shared log-lane FIFO. Past this depth the oldest queued
/// record is dropped (and counted), bounding memory while keeping the most
/// recent events and commands.
const LOG_HANDOFF_CAP: usize = 1024;

/// Where the cycle parks framed packets for the sender, one coalescing slot
/// per snapshot tap and one FIFO shared by every log tap. The module docs tell
/// the lane story; the methods here implement it without ever blocking the
/// cycle side.
struct HandOff {
    /// Snapshot lane: one coalescing slot per snapshot tap.
    slots: Mutex<Vec<Option<LenPacket>>>,
    /// Log lane: one bounded FIFO shared by every log tap.
    fifo: Mutex<VecDeque<LenPacket>>,
    /// Set when either lane is filled, cleared by the sender before it parks.
    /// This only avoids busy spinning; a missed wake is harmless because the
    /// sender always drains both lanes fully.
    pending: AtomicBool,
    /// Snapshots dropped by coalescing overwrite (surfaced as `telemetry.dropped`).
    dropped_snapshots: AtomicU64,
    /// Log records dropped-oldest on overflow (surfaced as `telemetry.msg_dropped`).
    dropped_logs: AtomicU64,
    /// Wakes the parked sender when either lane fills (or on shutdown).
    wq: Arc<WaitQueue>,
}

impl HandOff {
    fn new(n_slots: usize, wq: Arc<WaitQueue>) -> Self {
        Self {
            slots: Mutex::new((0..n_slots).map(|_| None).collect()),
            fifo: Mutex::new(VecDeque::new()),
            pending: AtomicBool::new(false),
            dropped_snapshots: AtomicU64::new(0),
            dropped_logs: AtomicU64::new(0),
            wq,
        }
    }

    /// Cycle side, never blocks: coalesce `pkt` into `slot`, counting a drop
    /// if it overwrote an un-sent packet, then wake the sender.
    fn push_snapshot(&self, slot: usize, pkt: LenPacket) {
        {
            let mut slots = self.slots.lock().expect("handoff poisoned");
            if slots[slot].is_some() {
                self.dropped_snapshots.fetch_add(1, Relaxed);
            }
            slots[slot] = Some(pkt);
        }
        self.pending.store(true, Release);
        self.wq.wake_all();
    }

    /// Cycle side, never blocks: append `pkt` to the log lane, dropping the
    /// oldest queued record (and counting it) past [`LOG_HANDOFF_CAP`], then
    /// wake the sender.
    fn push_log(&self, pkt: LenPacket) {
        {
            let mut fifo = self.fifo.lock().expect("handoff poisoned");
            if fifo.len() >= LOG_HANDOFF_CAP {
                fifo.pop_front();
                self.dropped_logs.fetch_add(1, Relaxed);
            }
            fifo.push_back(pkt);
        }
        self.pending.store(true, Release);
        self.wq.wake_all();
    }

    /// Sender side: take every pending packet from both lanes as
    /// `(snapshots, log records)`, dropping the locks before any `.await`.
    fn drain(&self) -> (Vec<LenPacket>, Vec<LenPacket>) {
        let snapshots = {
            let mut slots = self.slots.lock().expect("handoff poisoned");
            slots.iter_mut().filter_map(Option::take).collect()
        };
        let logs = {
            let mut fifo = self.fifo.lock().expect("handoff poisoned");
            fifo.drain(..).collect()
        };
        (snapshots, logs)
    }
}

// ---------------------------------------------------------------------------
// Uplink
// ---------------------------------------------------------------------------

/// How long one [`UplinkSystem::run`] pass sleeps once its link has dropped,
/// so the async-run loop does not busy-spin a dead reader (teardown cancels
/// the task regardless).
const UPLINK_IDLE: Duration = Duration::from_millis(50);

/// The uplink's output bundle: the two implicit health/log ports first, then a
/// [`MsgFanOut`] holding one ordinary message output per configured msg. A
/// consumer receives a msg only over an explicit edge, and every minted port
/// is untelemetered, so inbound control is never echoed back on the downlink.
///
/// Hand-written rather than derived because the fan-out's port count is
/// decided by configuration. [`MsgFanOut::bind`] drains every ring the source
/// still holds, so the minted ports must be the descriptor's trailing outputs,
/// the reverse of the [`Out`] convention of user ports first. The static
/// [`decls`](SystemOutput::decls) carry only health and log (the empty-config
/// shape); [`UplinkSystem::instance_descriptor`] appends the msg ports in the
/// same config order the bind walk pops rings, keeping the positional
/// contract.
pub struct UplinkOut {
    fan: MsgFanOut,
    health: HealthPort,
}

impl UplinkOut {
    /// Disjoint borrows of the minted ports and the health handle.
    fn split(&mut self) -> (&mut MsgFanOut, &mut HealthPort) {
        (&mut self.fan, &mut self.health)
    }
}

impl SystemOutput for UplinkOut {
    fn decls() -> Vec<PortDecl> {
        vec![
            PortDecl::Port(PortDesc::of::<crate::SystemHealth>()),
            PortDecl::Port(PortDesc::of::<crate::SystemLog>()),
        ]
    }
}

impl BindPorts for UplinkOut {
    fn bind<S: RingSource>(src: &mut S) -> Self {
        let health = crate::Output::bind(src);
        let log = crate::Output::bind(src);
        let fan = MsgFanOut::bind(src);
        Self {
            fan,
            health: HealthPort::new(health, log),
        }
    }
}

/// The uplink's empty input bundle; commands come from its own connection, not
/// edges.
pub struct UplinkIn;

impl SystemInput for UplinkIn {
    fn decls() -> Vec<PortDecl> {
        Vec::new()
    }
}

impl BindPorts for UplinkIn {
    fn bind<S: RingSource>(_src: &mut S) -> Self {
        UplinkIn
    }
}

/// The command ingest system, the read twin of [`TelemetrySystem`]: an
/// ordinary [`AsyncSystem`] that owns its own [`RecvTransport`] connection and
/// relays each subscribed inbound msg onto its matching minted output. It is a
/// pure pass-through for any message id: the forward set is per-instance
/// configuration (`msgs`, or [`with_msg`](Self::with_msg) programmatically),
/// never a compiled-in list, and payloads are forwarded verbatim, leaving each
/// consumer's own drain to decode (and discard garbage) exactly as on any
/// other message edge.
pub struct UplinkSystem<R: RecvTransport> {
    /// The read transport, taken to `None` on the first error; no reconnect.
    recv: Option<R>,
    /// Whether the subscription has been sent (once, before the first `recv`).
    subscribed: bool,
    /// The forward set, in config order: one `(NAME, ID)` per msg. Index k is
    /// minted output port k and bound writer k, so this one list keys the
    /// subscription, the dispatch, and the ports.
    msgs: Vec<(&'static str, PacketId)>,
    /// Config name tokens awaiting resolution in
    /// [`configure`](BuildSystem::configure); [`with_msg`](Self::with_msg)
    /// resolves statically instead.
    unresolved: Vec<String>,
}

impl<R: RecvTransport> UplinkSystem<R> {
    /// A pre-init uplink over its read transport, with an empty forward set;
    /// add msgs via [`with_msg`](Self::with_msg) or the registry's `msgs`
    /// params.
    pub fn new(recv: R) -> Self {
        Self {
            recv: Some(recv),
            subscribed: false,
            msgs: Vec::new(),
            unresolved: Vec::new(),
        }
    }

    /// Add `M` to the forward set, the typed twin of the `msgs` config list:
    /// subscribes to `M::ID` and mints an `M`-keyed output port. Idempotent.
    pub fn with_msg<M: NamedMsg>(mut self) -> Self {
        if !self.msgs.iter().any(|&(_, id)| id == M::ID) {
            self.msgs.push((M::NAME, M::ID));
        }
        self
    }
}

impl<R: RecvTransport + 'static> System for UplinkSystem<R> {
    type Input = UplinkIn;
    type Output = UplinkOut;
    const NAME: &'static str = "uplink";
}

impl<R: RecvTransport + 'static> AsyncSystem for UplinkSystem<R> {
    /// The static health/log shape plus one untelemetered message port per
    /// configured msg, in config order. The subscription, the dispatch, and
    /// the wired ports all derive from the one `msgs` list, so they cannot
    /// diverge.
    fn instance_descriptor(&self) -> SystemDescriptor {
        let mut desc = Self::descriptor();
        desc.outputs.extend(
            self.msgs
                .iter()
                .map(|&(name, id)| PortDesc::msg_dynamic(name, id).untelemetered()),
        );
        desc
    }

    /// One ingest pass, called repeatedly by the coordinator's async-run loop:
    /// receive the next packet and forward it verbatim on the minted output
    /// whose id matches. A msg outside the configured set bumps
    /// `uplink.unroutable` (the broker should only relay subscribed ids, so
    /// this signals a broker or config mismatch), a full ring bumps
    /// `uplink.dropped`, and `Table` packets are silently ignored. The first
    /// error drops the link for good, and later passes idle instead of
    /// spinning.
    async fn run(&mut self, _input: &mut Self::Input, output: &mut Self::Output) {
        // Subscribe once, before the first read, to exactly the configured ids.
        if !self.subscribed {
            let (fan, health) = output.split();
            // An async system has no per-cycle driver flushing its health, and
            // an init-time log would be emitted before the downlink's taps
            // resolve, so warn on the first run pass and publish immediately.
            if self.msgs.is_empty() {
                health.log(
                    crate::health::Level::Warn,
                    "uplink has no msgs configured; it will receive nothing",
                );
                health.end_cycle(Timestamp::now(), 0);
            } else if fan.len() != self.msgs.len() {
                // One writer per configured msg is the bind contract; a
                // mismatch means the registered descriptor and this instance
                // diverged.
                health.error("uplink.bind_mismatch");
                health.end_cycle(Timestamp::now(), 0);
            }
            let ids: Vec<PacketId> = self.msgs.iter().map(|&(_, id)| id).collect();
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
            Ok(OwnedPacket::Msg(m)) => {
                let (fan, health) = output.split();
                match self.msgs.iter().position(|&(_, id)| id == m.id) {
                    Some(idx) => {
                        if fan.write_raw(idx, m.id, &m.buf).is_err() {
                            health.error("uplink.dropped");
                            health.end_cycle(Timestamp::now(), 0);
                        }
                    }
                    None => {
                        health.error("uplink.unroutable");
                        health.end_cycle(Timestamp::now(), 0);
                    }
                }
            }
            // Tables are ignored; the uplink is commands only.
            Ok(_) => {}
            // A dropped link (or an exhausted mock) ends the reader, like the
            // sender.
            Err(_) => self.recv = None,
        }
    }
}

/// One announced tap's wire schema, moved into the sender task to replay on
/// connect.
struct Announce {
    packet_id: PacketId,
    vtable: VTable,
    metadata: Vec<ComponentMetadata>,
}

/// The async sender task: announce every table tap once, then drain the
/// hand-off and send until a transport error or `stop` ends it. The cycle side
/// is unaffected either way.
async fn run_sender<T: Transport>(
    mut transport: T,
    announces: Vec<Announce>,
    handoff: Arc<HandOff>,
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
        // Drain both lanes; park on the wait queue only when both are empty.
        let (pkts, logs) = handoff.drain();
        if pkts.is_empty() && logs.is_empty() {
            let _ = wq
                .wait_for(|| handoff.pending.swap(false, AcqRel) || stop.load(Acquire))
                .await;
            continue;
        }
        for pkt in pkts.into_iter().chain(logs) {
            if transport.send(pkt).await.is_err() {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------

/// The downlink's output bundle: no typed ports, just the [`AllOutputs`]
/// receive-all field. `init` reaches the output registry through it, the
/// receive-all capability its decl contributes earns the downlink a reader
/// slot on every buffer at sizing time, and its `bind` pulls the host registry
/// rather than consuming a ring.
#[derive(crate::SystemOutput)]
pub struct TelemetryPorts {
    all: AllOutputs,
}

/// The downlink's empty input bundle; it pulls outputs through the registry,
/// not typed edges.
pub struct TelemetryIn;

impl SystemInput for TelemetryIn {
    fn decls() -> Vec<PortDecl> {
        Vec::new()
    }
}

impl BindPorts for TelemetryIn {
    fn bind<S: RingSource>(_src: &mut S) -> Self {
        TelemetryIn
    }
}

// ---------------------------------------------------------------------------
// The downlink system
// ---------------------------------------------------------------------------

/// A read view into one tapped buffer plus the [`Lane`] and [`Wire`] framing
/// projected from the entry's delivery and schema axes.
struct Tap {
    view: View<NoWake, NoWake>,
    lane: Lane,
    wire: Wire,
    /// Coalesce lane only: the ring's `committed` at the last push, so a cycle
    /// with no new record pushes nothing (the pinned newest record is not
    /// re-sent). `u64::MAX` means nothing pushed yet.
    last_committed: u64,
}

/// Which hand-off lane a tap pushes to, projected from the entry's
/// [`Delivery`](crate::Delivery).
enum Lane {
    /// Latest wins: coalesce into this tap's dedicated slot.
    Coalesce { slot: usize },
    /// Every record, in order, into the shared FIFO.
    Fifo,
}

/// How a tap frames a drained record, projected from the entry's schema.
enum Wire {
    /// A `Table` packet under the announce-assigned packet id.
    Table { packet_id: PacketId },
    /// A self-describing `Msg` packet; the id is the record's first two bytes.
    Msg,
}

/// Everything `init` starts, bundled so it is present or absent as one.
struct Started {
    handoff: Arc<HandOff>,
    stop: Arc<AtomicBool>,
    /// Holds the sender task; dropping the guard cancels the task on teardown.
    /// `None` when the transport was already taken by an earlier `init`; the
    /// taps still drain.
    #[allow(dead_code)]
    sender: Option<JoinHandleDropGuard<()>>,
}

/// A [`CyclicSystem`] that copies every tapped output buffer's pending records
/// onto a ground link, generic over the [`Transport`] chosen by the wiring
/// (`type="TcpDownlink"` for TCP) or by a test (a mock). Register it
/// after every other cyclic system, or let the wiring resolver defer it there;
/// its `ReceiveAll` capability is what the build-time ordering check keys on.
///
/// Read views are claimed in `init`, which runs after earlier-registered
/// systems' `init`s, so a frame or message emitted during another system's
/// `init` is not downlinked (the view starts at the live edge past it). Values
/// that must reach the ground should be published from the first `execute`
/// onward.
pub struct TelemetrySystem<T: Transport> {
    transport: Option<T>,
    mode: TelemetryMode,
    /// The resolved taps; each carries its own lane and wire framing.
    taps: Vec<Tap>,
    /// The running state `init` assembles. `None` before `init`.
    started: Option<Started>,
    /// Snapshot drops already surfaced to health (each new one reported once).
    last_dropped: u64,
    /// The log-lane twin of `last_dropped` (surfaced as `telemetry.msg_dropped`).
    last_msg_dropped: u64,
}

impl<T: Transport> TelemetrySystem<T> {
    /// A pre-init downlink from its config. Taps are resolved and the sender
    /// spawned at `init`, where the registry handle is reachable.
    pub fn new(config: TelemetryConfig<T>) -> Self {
        Self {
            transport: Some(config.transport),
            mode: config.mode,
            taps: Vec::new(),
            started: None,
            last_dropped: 0,
            last_msg_dropped: 0,
        }
    }
}

impl<T: Transport + 'static> System for TelemetrySystem<T> {
    type Input = TelemetryIn;
    type Output = Out<TelemetryPorts>;
    const NAME: &'static str = "telemetry";

    /// Resolve the tap set, claim one read `View` per tapped buffer, and spawn
    /// the async sender. `init` runs on the coordinator's loop task, so
    /// `stellarator::spawn` has a runtime, and the sender announces before any
    /// data is queued (nothing is pushed until the first `execute`).
    fn init(&mut self, output: &mut Self::Output) {
        // `AllOutputs::entries()` is already filtered to telemetered entries,
        // so a command channel or an opted-out frame never reaches the matcher.
        let mut taps = Vec::new();
        let mut announces = Vec::new();
        let mut n_slots = 0usize;
        // Deferred health reports: iterating `output.all` borrows the output
        // bundle, so `output.health()` (a `&mut` borrow) runs after the loop.
        let mut exhausted: Vec<String> = Vec::new();
        for entry in output.all.entries() {
            if !self.mode.matches(entry) {
                continue;
            }
            let view = match entry.view() {
                Ok(v) => v,
                // The buffer has no reader slot left. Build-time sizing makes
                // this unreachable for the known consumers, but a hand-built
                // over-subscription (or too little configured reader slack) is
                // worth diagnosing, so log the buffer by name and skip the tap
                // instead of panicking.
                Err(_) => {
                    exhausted.push(format!("{}.{}", entry.instance, entry.name));
                    continue;
                }
            };
            // Lane and wire are independent projections of the entry: delivery
            // picks the hand-off lane, schema picks the framing.
            let lane = match entry.delivery {
                Delivery::Snapshot => {
                    let slot = n_slots;
                    n_slots += 1;
                    Lane::Coalesce { slot }
                }
                Delivery::Log => Lane::Fifo,
            };
            let wire = match &entry.schema {
                EntrySchema::Table {
                    vtable, metadata, ..
                } => {
                    let packet_id = (announces.len() as u16).to_le_bytes();
                    announces.push(Announce {
                        packet_id,
                        vtable: vtable.clone(),
                        metadata: metadata.clone(),
                    });
                    Wire::Table { packet_id }
                }
                EntrySchema::Postcard => Wire::Msg,
            };
            taps.push(Tap {
                view,
                lane,
                wire,
                last_committed: u64::MAX,
            });
        }

        for key in &exhausted {
            let health = output.health();
            health.error("telemetry.reader_slot");
            health.log(
                crate::health::Level::Warn,
                &format!("no reader slot left on `{key}` — raise CoordinatorConfig::reader_slack"),
            );
        }

        // One wait queue on the one two-lane hand-off wakes the single sender.
        let wq = Arc::new(WaitQueue::new());
        let handoff = Arc::new(HandOff::new(n_slots, wq.clone()));
        let stop = Arc::new(AtomicBool::new(false));
        let sender = self.transport.take().map(|transport| {
            stellarator::spawn(run_sender(
                transport,
                announces,
                handoff.clone(),
                wq,
                stop.clone(),
            ))
            .drop_guard()
        });
        self.taps = taps;
        self.started = Some(Started {
            handoff,
            stop,
            sender,
        });
    }

    /// Signal the sender to stop and wake it so it exits cooperatively (the
    /// drop guard cancels it regardless when the system is dropped at
    /// teardown).
    fn shutdown(&mut self, _output: &mut Self::Output) {
        if let Some(started) = &self.started {
            started.stop.store(true, Release);
            started.handoff.wq.wake_all();
        }
    }
}

/// Frame one drained record per the tap's [`Wire`]: a `Table` packet under the
/// announce-assigned id, or a self-describing `Msg` packet keyed by the
/// record's own leading id (`None` if the record is too short to carry one).
fn frame_packet(wire: &Wire, rec: &[u8]) -> Option<LenPacket> {
    match wire {
        Wire::Table { packet_id } => {
            let mut pkt = LenPacket::table(*packet_id, rec.len());
            pkt.extend_from_slice(rec);
            Some(pkt)
        }
        Wire::Msg => split_record(rec).map(|(id, payload)| {
            let mut pkt = LenPacket::msg(id, payload.len());
            pkt.extend_from_slice(payload);
            pkt
        }),
    }
}

impl<T: Transport + 'static> CyclicSystem for TelemetrySystem<T> {
    /// The end-of-cycle drain; never awaits. A coalesce tap borrows only the
    /// newest record and pushes one latest-wins snapshot (nothing when no new
    /// record landed); a FIFO tap pushes every record in order. Either way the
    /// record is borrowed in place off the ring and framed per the tap's wire
    /// form. Drops detected since the last cycle surface as
    /// `telemetry.dropped` (snapshot lane) and `telemetry.msg_dropped` (log
    /// lane).
    fn execute(&mut self, _now: Timestamp, _input: &mut Self::Input, output: &mut Self::Output) {
        let Some(started) = &self.started else {
            return;
        };
        let handoff = started.handoff.clone();
        for tap in &mut self.taps {
            match tap.lane {
                // An unchanged `committed` means no new record this cycle;
                // push nothing rather than re-sending the pinned record.
                Lane::Coalesce { slot } => {
                    let committed = tap.view.committed();
                    if committed == tap.last_committed {
                        continue;
                    }
                    tap.last_committed = committed;
                    // A corrupt read (unreachable in practice) counts as
                    // nothing new.
                    if let Ok(Some(grant)) = tap.view.try_latest()
                        && let Some(pkt) = frame_packet(&tap.wire, &grant)
                    {
                        handoff.push_snapshot(slot, pkt);
                    }
                }
                // Every record, in order, onto the shared FIFO.
                Lane::Fifo => {
                    let wire = &tap.wire;
                    let handoff = &handoff;
                    let _ = crate::port::drain_view(&mut tap.view, |rec| {
                        if let Some(pkt) = frame_packet(wire, rec) {
                            handoff.push_log(pkt);
                        }
                    });
                }
            }
        }
        // Surface any packets the sender's backlog forced us to drop.
        let dropped = handoff.dropped_snapshots.load(Relaxed);
        while self.last_dropped < dropped {
            output.health().error("telemetry.dropped");
            self.last_dropped += 1;
        }
        let msg_dropped = handoff.dropped_logs.load(Relaxed);
        while self.last_msg_dropped < msg_dropped {
            output.health().error("telemetry.msg_dropped");
            self.last_msg_dropped += 1;
        }
    }
}

#[cfg(test)]
mod tests;
