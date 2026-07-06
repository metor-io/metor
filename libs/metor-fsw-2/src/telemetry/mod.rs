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
//! Beside the component-frame snapshots, telemetry also taps every Postcard registry
//! entry and downlinks each `(id, postcard)` record as an `OwnedPacket::Msg` — **no
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
// Transport (telemetry.md §7)
// ---------------------------------------------------------------------------

/// An async error a [`Transport`] surfaces; v1 stops downlinking on any error (the
/// in-cycle snapshot stage keeps running and simply drops).
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// The link is not connected (never established, or dropped).
    #[error("transport is not connected")]
    Disconnected,
    /// An underlying I/O error, carried as its boxed source (transport-agnostic —
    /// the TCP paths raise `stellarator`/`metor-proto-stellar` errors) so the
    /// chain survives a `?` into anyhow/miette instead of flattening to a string.
    #[error("transport I/O error: {0}")]
    Io(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl TransportError {
    /// Wrap any transport-level error as the boxed [`Io`](TransportError::Io) source
    /// — the one conversion every TCP call site funnels through.
    fn io(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        TransportError::Io(Box::new(e))
    }
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
        sink.send(msg)
            .await
            .0
            .map_err(TransportError::io)?;
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
        sink.send(pkt)
            .await
            .0
            .map_err(TransportError::io)?;
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
/// read half is a [`PacketStream`] yielding the streamed Msgs. A dropped socket fails `recv` and
/// stops the reader (no reconnect); uplink and downlink no longer share a socket (shared link is
/// deferred, §4.5).
pub struct TcpRecvTransport {
    addr: std::net::SocketAddr,
    stream: Option<PacketStream<OwnedReader<TcpStream>>>,
    /// The write half, held open so the db keeps streaming (it is the channel the [`MsgStream`]
    /// subscription was sent on; dropping it could half-close the socket and end the stream).
    #[allow(dead_code)]
    sink: Option<PacketSink<OwnedWriter<TcpStream>>>,
    /// The message ids to subscribe to on connect (`docs/message-wiring.md` §5.2), set by
    /// [`subscribe`](RecvTransport::subscribe) before the first `recv`. Empty subscribes
    /// to nothing (the uplink warns about an empty config; there is no fallback id).
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

    /// Connect lazily on first use, send one [`MsgStream`] subscription per configured id,
    /// then reuse the open read half. Subsequent calls reuse the established stream.
    async fn ensure(&mut self) -> Result<&mut PacketStream<OwnedReader<TcpStream>>, TransportError> {
        if self.stream.is_none() {
            let stream = TcpStream::connect(self.addr)
                .await
                .map_err(TransportError::io)?;
            let (rx, tx) = stream.split();
            // Subscribe to the panel's command stream before reading (`docs/messages.md` §4.4):
            // the db only forwards a message id to clients that asked for it. Mirrors cube-sat
            // (`examples/cube-sat/src/main.rs:555`).
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
    /// ONE matcher for every entry kind: the `frames` subset list matches
    /// [`RegistryEntry::name`], which covers both frame names (`F::NAME` — the same
    /// string a frame id hashes) and channel names; instance matching is identical
    /// for both.
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

/// The telemetry downlink configuration [`TelemetrySystem::new`] is constructed
/// from. Programmatic users (and every mock-transport test) build one directly and
/// register the system like any other:
/// `builder.add_cyclic(TelemetrySystem::new(config))`.
pub struct TelemetryConfig<T: Transport> {
    /// Where the snapshots go (TCP in v1, a mock in tests).
    pub transport: T,
    /// Which outputs to tap.
    pub mode: TelemetryMode,
}

/// The wiring params of the built-in TCP downlink (`type="TcpDownlink"`): the ground
/// address plus an optional tap subset. Both lists absent ⇒ [`TelemetryMode::All`];
/// either present ⇒ [`TelemetryMode::Subset`] (an entry matches if its instance *or*
/// its frame/channel name is listed). Sequence params are child nodes
/// (`docs/design-kdl-serde.md`):
///
/// ```kdl
/// system "telemetry" type="TcpDownlink" addr="127.0.0.1:2240" {
///     instances "nav" "imu"      // optional; omit both children to tap everything
///     frames "gyro_b"
/// }
/// ```
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct DownlinkParams {
    /// The TCP address of the ground/db endpoint the downlink streams to.
    pub addr: std::net::SocketAddr,
    /// Instance names to tap; `None` (with `frames` also `None`) taps everything.
    #[serde(default)]
    pub instances: Option<Vec<String>>,
    /// Frame/channel names to tap.
    #[serde(default)]
    pub frames: Option<Vec<String>>,
}

impl DownlinkParams {
    /// Project the two optional subset lists onto the runtime [`TelemetryMode`].
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

/// The registry entry point of the built-in TCP downlink: data params in, lazy
/// transport out ([`TcpTransport::new`] connects on first announce inside the async
/// sender task, so nothing blocks here).
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

/// The wiring params of the built-in TCP uplink (`type="TcpUplink"`): the address of
/// the metor-db broker plus the message types to relay. Each `msgs` token is a
/// [`NamedMsg::NAME`] resolved against the registry's [`MsgTable`](crate::MsgTable)
/// (`Registry::register_msg` — the wkt set is pre-seeded); the uplink subscribes
/// to exactly those ids and mints one ordinary message output port per msg, so
/// `connect "uplink" -> … msg="…"` edges resolve like any other. No `msgs` child
/// means the uplink relays nothing (and warns) — there is no built-in default set.
///
/// ```kdl
/// system "uplink" type="TcpUplink" addr="127.0.0.1:2241" {
///     msgs "SequenceCommand" "AlarmAck"
/// }
/// ```
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct UplinkParams {
    /// The TCP address of the metor-db broker the uplink subscribes to.
    pub addr: std::net::SocketAddr,
    /// The [`NamedMsg::NAME`] tokens of the messages to relay.
    #[serde(default)]
    pub msgs: Option<Vec<String>>,
}

/// The registry entry point of the built-in TCP uplink ([`TcpRecvTransport::new`] is
/// as lazy as the downlink's transport — it connects and subscribes on first `recv`).
/// The `msgs` name tokens resolve in [`configure`](BuildSystem::configure), where the
/// host's msg table is in scope.
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
            // Idempotent: a duplicate token is one port, not a DuplicateRegistryKey.
            if !self.msgs.iter().any(|&(_, existing)| existing == id) {
                self.msgs.push((name, id));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Hand-off (the bounded, per-tap-coalescing queue, telemetry.md §4)
// ---------------------------------------------------------------------------

/// The bound on the shared Log-lane FIFO. A backed-up link drops the **oldest**
/// queued record past this depth (and counts it), bounding memory while keeping the
/// link's most recent log of events/commands.
const LOG_HANDOFF_CAP: usize = 1024;

/// THE hand-off (C4): one struct, one wait queue, one sender — two lanes keyed on
/// the entry's [`Delivery`](crate::Delivery) axis.
///
/// **Snapshot lane** — one coalescing slot per Snapshot tap: a newer snapshot
/// overwrites an older un-sent one (latest-wins — the Overwrite ring semantics, one
/// level up); overwriting an *occupied* slot counts a drop (`telemetry.dropped`).
///
/// **Log lane** — one bounded FIFO shared by every Log tap: an event/command record
/// must never be coalesced, so every drained record is appended in order and
/// forwarded verbatim (cross-channel order is irrelevant — each record
/// self-addresses). Overflow drops the **oldest** and counts it
/// (`telemetry.msg_dropped` — the health key is kept).
struct HandOff {
    /// Snapshot lane: one coalescing slot per Snapshot tap.
    slots: Mutex<Vec<Option<LenPacket>>>,
    /// Log lane: one bounded FIFO shared by every Log tap.
    fifo: Mutex<VecDeque<LenPacket>>,
    /// Set when either lane is filled; cleared by the sender when it parks. Just
    /// avoids busy spinning — a missed wake is harmless because the sender always
    /// drains both lanes fully.
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

    /// Cycle side (never blocks): coalesce `pkt` into `slot`, counting a drop if it
    /// overwrote an un-sent packet, then wake the sender.
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

    /// Cycle side (never blocks): append `pkt` to the Log lane, dropping the oldest
    /// queued record (and counting it) past [`LOG_HANDOFF_CAP`], then wake the sender.
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

    /// Sender side: take every pending packet from both lanes
    /// `(snapshots, log records)` — drops the locks before any `.await`.
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
// Uplink — the command-plane ingest system (`docs/messages.md` §4.4)
// ---------------------------------------------------------------------------

/// Idle pause one [`UplinkSystem::run`] pass takes once its link has dropped, so the
/// coordinator's async-run loop does not busy-spin a dead reader (teardown cancels the task
/// via its drop guard regardless).
const UPLINK_IDLE: Duration = Duration::from_millis(50);

/// The uplink's output bundle: the two implicit health/log ports **first**, then a
/// [`MsgFanOut`] holding one ordinary message output **per configured msg** — fully
/// normal message producers, no bespoke command-bus capability. A consumer receives
/// a msg only over an explicit edge (`connect "uplink" -> … msg="…"`, A2), and every
/// minted port is untelemetered (inbound control is never echoed on the downlink).
///
/// Hand-written (not `Out<...>` + derive) because the fan-out's port count is
/// config-determined: [`MsgFanOut::bind`] drains every ring the source still holds,
/// so the minted ports must be the descriptor's *trailing* outputs — the reverse of
/// the [`Out`] convention (user ports first). The static [`decls`](SystemOutput::decls)
/// carry only health/log (the empty-config shape); the per-instance msg ports come
/// from [`UplinkSystem::instance_descriptor`], which appends them in the same
/// config order the bind walk pops rings, keeping the positional contract.
pub struct UplinkOut {
    fan: MsgFanOut,
    health: HealthPort,
}

impl UplinkOut {
    /// Disjoint borrows of the minted ports and the health handle — the
    /// [`Out::split`] mirror.
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

/// The uplink's (empty) input bundle — it sources commands from its own connection, not edges.
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

/// The command-plane ingest system (`docs/messages.md` §4.4): the read twin of
/// [`TelemetrySystem`], an ordinary [`AsyncSystem`] that owns its **own** [`RecvTransport`]
/// connection (§4.5) and relays each subscribed wire Msg onto its matching minted output.
/// A pure pass-through for **any** message id: the forward set is the instance's config
/// (`msgs`, or [`with_msg`](Self::with_msg) programmatically), never a compiled-in list,
/// and payloads are forwarded verbatim — each consumer's [`MsgIn`](crate::MsgIn) drain
/// decodes (and discards garbage) exactly as on any other message edge.
pub struct UplinkSystem<R: RecvTransport> {
    /// The read transport, taken `None` on drop-on-disconnect (no reconnect, telemetry.md §7).
    recv: Option<R>,
    /// Whether the ground subscription has been sent (once, before the first `recv`).
    subscribed: bool,
    /// The forward set, in config order: one `(NAME, ID)` per msg. Index k is minted
    /// output port k ([`instance_descriptor`](AsyncSystem::instance_descriptor)) and
    /// bound writer k ([`MsgFanOut`]) — one list keys all three.
    msgs: Vec<(&'static str, PacketId)>,
    /// Config name tokens awaiting [`configure`](BuildSystem::configure) resolution
    /// (the registry path; [`with_msg`](Self::with_msg) resolves statically instead).
    unresolved: Vec<String>,
}

impl<R: RecvTransport> UplinkSystem<R> {
    /// Construct the (pre-init) uplink from its read transport (its own connection),
    /// with an empty forward set — add msgs via [`with_msg`](Self::with_msg) or the
    /// registry's `msgs` params.
    pub fn new(recv: R) -> Self {
        Self {
            recv: Some(recv),
            subscribed: false,
            msgs: Vec::new(),
            unresolved: Vec::new(),
        }
    }

    /// Add `M` to the forward set — the typed twin of the `msgs` config list:
    /// subscribes to `M::ID` on the ground and mints an `M`-keyed output port.
    /// Idempotent.
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
    /// The static health/log shape plus one untelemetered message port per configured
    /// msg, in config order — the subscription, the dispatch, and the wired ports all
    /// derive from the one `msgs` list, so they cannot diverge.
    fn instance_descriptor(&self) -> SystemDescriptor {
        let mut desc = Self::descriptor();
        desc.outputs.extend(
            self.msgs
                .iter()
                .map(|&(name, id)| PortDesc::msg_dynamic(name, id).untelemetered()),
        );
        desc
    }

    /// One ingest pass (the coordinator's async-run loop calls it repeatedly): `recv` the
    /// next packet and forward it verbatim on the minted output whose id matches. A Msg
    /// outside the configured set bumps `uplink.unroutable` (the db should only relay
    /// subscribed ids, so this signals a broker/config mismatch); a full ring bumps
    /// `uplink.dropped`; Tables are silently ignored (the downlink's traffic class, never
    /// expected here). On the first error the link is dropped (no reconnect) and subsequent
    /// passes idle so the loop does not spin a dead reader.
    async fn run(&mut self, _input: &mut Self::Input, output: &mut Self::Output) {
        // Subscribe once, before the first read, to exactly the configured ids.
        if !self.subscribed {
            let (fan, health) = output.split();
            // An async system has no per-cycle driver flushing its health, and an
            // init-time log never reaches the ground (the downlink's taps resolve
            // during init) — so warn on the first run pass and publish immediately.
            if self.msgs.is_empty() {
                health.log(
                    crate::health::Level::Warn,
                    "uplink has no msgs configured; it will receive nothing",
                );
                health.end_cycle(Timestamp::now(), 0);
            } else if fan.len() != self.msgs.len() {
                // One writer per configured msg is the bind contract; a mismatch
                // means the registered descriptor and this instance diverged.
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
                // Disjoint borrows: the forward takes the ports, the miss takes health.
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
            // Tables are ignored — the uplink is commands only.
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

/// The async sender task (telemetry.md §4): announce every Table tap once on
/// connect, then drain the one two-lane hand-off and send. Stops downlinking on any
/// transport error or when `stop` is set (the cycle is unaffected either way).
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
        // Snapshots (latest-wins) and log records (FIFO) share the one sender; drain
        // both lanes, park on the wait queue only when both are empty.
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
// Ports (no typed inputs; the output bundle carries the registry handle)
// ---------------------------------------------------------------------------

/// The telemetry system has no typed inputs (telemetry.md §3); its output bundle is a single
/// [`AllOutputs`] receive-all field (`docs/message-wiring.md` §4) — no longer a port but the
/// [`Capability::ReceiveAll`](crate::Capability) grant its `decl()` contributes to the
/// descriptor. `init` reaches the registry through it, the capability earns the downlink a
/// reader slot on every buffer at sizing time, and the derived `bind` walk skips it on the
/// ring cursor (`AllOutputs::bind` pulls the host registry — the downlink is never dlopen'd).
#[derive(crate::SystemOutput)]
pub struct TelemetryPorts {
    all: AllOutputs,
}

/// The empty input bundle (the streamer pulls outputs via the registry, not typed edges).
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
// The system
// ---------------------------------------------------------------------------

/// ONE resolved tap (C4): a read view into a registered buffer plus the two axes
/// that drive it — the hand-off lane (from the entry's `delivery`) and the wire
/// framing (from the entry's `schema`). The generic `Table × Log` combination falls
/// out for free: FIFO lane + announced Table packets.
struct Tap {
    view: View<NoWake, NoWake>,
    lane: Lane,
    wire: Wire,
    /// Coalesce lane only: the ring's `committed` at the last push, so a cycle with
    /// no new record pushes nothing (the pinned newest record is not re-sent).
    /// `u64::MAX` = nothing pushed yet.
    last_committed: u64,
}

/// Which hand-off lane a tap pushes to — the entry's `Delivery` projection.
enum Lane {
    /// Latest-wins: coalesce into this tap's dedicated slot.
    Coalesce { slot: usize },
    /// Every record, in order, into the shared FIFO.
    Fifo,
}

/// How a tap frames a drained record — the entry's schema projection.
enum Wire {
    /// A `Table` packet under the announce-assigned packet id.
    Table { packet_id: PacketId },
    /// A self-describing `Msg` packet; the id is the record's first two bytes.
    Msg,
}

/// Everything `init` starts, bundled (C6 — one `Option<Started>` instead of three
/// parallel `Option`s that were only ever set together).
struct Started {
    handoff: Arc<HandOff>,
    stop: Arc<AtomicBool>,
    /// Holds the sender task; its `Drop` cancels the task on teardown. `None` when
    /// the transport was already taken (a re-init) — the taps still drain.
    #[allow(dead_code)]
    sender: Option<JoinHandleDropGuard<()>>,
}

/// The telemetry downlink system (telemetry.md §3). Generic over the [`Transport`]; the
/// concrete `T` is chosen by the wiring (TCP, `type="TcpDownlink"`) or a test (mock).
/// An ordinary registry system: register it **after** every other cyclic system (its
/// `ReceiveAll` capability is what `build()`'s ordering check keys on), or let the
/// wiring resolver defer it there automatically.
///
/// **Init-time emit gap (B9)**: the downlink claims its read views in its own
/// `init`, which runs *after* earlier-registered systems' `init`s — a frame or
/// message a system emits during `init` is therefore **not** downlinked (the
/// view starts at the live edge past it). Values that must reach the ground
/// should be (re-)published from the first `execute`; the coordinator's own boot
/// `SequenceRegistry` deliberately emits at the head of `run_for`, after every
/// `init`, for exactly this reason.
pub struct TelemetrySystem<T: Transport> {
    transport: Option<T>,
    mode: TelemetryMode,
    /// ONE tap list (C4): each tap carries its own lane + wire framing.
    taps: Vec<Tap>,
    /// The running state `init` assembles: the hand-off, the sender's stop flag, and
    /// the sender task guard. `None` before `init`.
    started: Option<Started>,
    /// Snapshot drops already surfaced to health (each new one reported once).
    last_dropped: u64,
    /// The Log-lane twin of `last_dropped` (surfaced as `telemetry.msg_dropped`).
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

    /// Resolve the tap set, claim one read `View` per tapped buffer, and spawn the async
    /// sender (telemetry.md §3/§4). `init` runs on the coordinator's loop task within
    /// `start()`, so `stellarator::spawn` has a runtime; the sender announces before any
    /// data is queued (nothing is pushed until the first `execute`).
    fn init(&mut self, output: &mut Self::Output) {
        // ONE loop over the unified registry surface — `AllOutputs::entries()` is
        // already telemetered-only (the A6 source-side filter), so a command channel
        // or an opted-out frame never even reaches the matcher here.
        let mut taps = Vec::new();
        let mut announces = Vec::new();
        let mut n_slots = 0usize;
        // Deferred health reports: iterating `output.all` borrows the output bundle,
        // so `output.health()` (a `&mut` borrow) is driven after the loop.
        let mut exhausted: Vec<String> = Vec::new();
        for entry in output.all.entries() {
            if !self.mode.matches(entry) {
                continue;
            }
            let view = match entry.view() {
                Ok(v) => v,
                // Reader-slot budget exhausted: surface it — a health error plus a log
                // line NAMING the buffer (E8b) — and skip this tap rather than
                // panicking (telemetry.md §2.5). Build-time sizing makes this
                // unreachable for the known consumers, but a hand-built
                // over-subscription (or a too-small `CoordinatorConfig::reader_slack`)
                // is diagnosable.
                Err(_) => {
                    exhausted.push(format!("{}.{}", entry.instance, entry.name));
                    continue;
                }
            };
            // The two tap axes are independent projections of the entry: the LANE
            // comes off `delivery` (coalesce vs FIFO), the WIRE off `schema`
            // (announced Table id vs self-describing record). The generic
            // `Table × Log` combination — an every-record frame log — falls out with
            // zero extra code.
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

        // One wait queue on the one two-lane hand-off wakes the single sender task.
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

    /// Signal the sender to stop and wake it so it exits cooperatively (the drop guard
    /// cancels it regardless when the system is dropped at coordinator teardown).
    fn shutdown(&mut self, _output: &mut Self::Output) {
        if let Some(started) = &self.started {
            started.stop.store(true, Release);
            started.handoff.wq.wake_all();
        }
    }
}

/// Frame one drained record for the wire per the tap's [`Wire`]: a `Table` packet
/// under the announce-assigned id, or a self-describing `Msg` packet (the record's
/// own 2-byte id — `None` if the record is too short to carry one).
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
    /// End-of-cycle drain (telemetry.md §3/§4) — ONE loop over the one tap list;
    /// never awaits. A Coalesce tap borrows only the newest record and pushes one
    /// latest-wins snapshot (nothing when no new record landed); a Fifo tap pushes
    /// **every** record in order. Either way the record is borrowed in place off
    /// the ring and framed per the tap's wire form. Drops detected since last cycle
    /// surface as `telemetry.dropped` (snapshot lane) / `telemetry.msg_dropped`
    /// (log lane).
    fn execute(&mut self, _now: Timestamp, _input: &mut Self::Input, output: &mut Self::Output) {
        let Some(started) = &self.started else {
            return;
        };
        let handoff = started.handoff.clone();
        for tap in &mut self.taps {
            match tap.lane {
                // Latest-wins: borrow the newest committed record; an unchanged
                // `committed` means no new record this cycle — nothing to send
                // (and the pinned record is not re-pushed).
                Lane::Coalesce { slot } => {
                    let committed = tap.view.committed();
                    if committed == tap.last_committed {
                        continue;
                    }
                    tap.last_committed = committed;
                    // Corrupt (unreachable in practice) reads as "nothing new".
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
        // Surface any packets the sender's backlog forced us to drop (telemetry.md §4).
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
