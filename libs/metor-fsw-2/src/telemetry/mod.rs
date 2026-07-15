//! Streams output buffers to a ground endpoint and relays ground commands
//! back in.
//!
//! [`TelemetrySystem`] is the downlink, an ordinary [`CyclicSystem`] registered after every other cyclic system. At the
//! end of the cycle `execute` frames the pending records of every tapped output
//! buffer into one batch buffer and queues the batch for a spawned sender task.
//!
//!  [`UplinkSystem`] is the read side, an [`AsyncSystem`] that owns its own connection, subscribes
//! to a configured set of message ids, and republishes each inbound `Msg`
//! packet on an ordinary message output port, so consumers receive commands
//! over explicit edges like any other message.
//!
//! # Cycle / sender split
//!
//! The control cycle is single-threaded and synchronous while the socket is
//! async, so the downlink is split in two. The in-cycle stage only frames
//! records into the cycle's batch and hands it over a fixed-depth queue,
//! never blocking on the link; the sender task does all the awaiting I/O.
//! The queue recycles its buffers — a sent batch's allocation comes back for
//! a later cycle — so the steady state allocates nothing. When the transport
//! backs up the queue fills, whole batches are dropped rather than delayed,
//! and every drop is counted on the system's health output as
//! `telemetry_dropped`.
//!
//! # Taps and wire framing
//!
//! Each tapped registry entry becomes one tap, and the entry's two axes drive
//! it independently. Its [`Delivery`](crate::Delivery) picks how much of the
//! buffer each cycle contributes to the batch:
//!
//! * `Snapshot` entries contribute only the cycle's newest record; a cycle
//!   with nothing new contributes nothing.
//! * `Log` entries contribute every pending record, in commit order. Event
//!   and command records must never be coalesced.
//!
//! The entry's schema picks the wire framing. A table entry is framed as a
//! `Table` packet whose payload is the ring record itself (the bytes a system
//! committed are the bytes on the wire), referencing a `VTable` announced once
//! on connect. A postcard entry is framed as a self-describing `Msg` packet
//! whose id is the record's leading two bytes, with no announce step. The two
//! axes combine freely; an every-record table log needs no extra code. The
//! wire is a plain stream of length-prefixed packets, so a batch is just the
//! cycle's packets concatenated and the receiver never notices the batching.
//!
//! # Connection lifecycle
//!
//! Both TCP transports connect lazily on first use, inside the async task that
//! drives them, and both redial after an error: a failed connect or a dropped
//! link is retried under exponential backoff ([`RECONNECT_MIN`] doubling to
//! [`RECONNECT_MAX`]), so a ground endpoint that is down at boot or restarts
//! mid-mission picks the stream back up on its own. Each downlink connect
//! replays every tap's announce before any batch, which is what lets a
//! restarted consumer decode tables again; each uplink connect re-sends its
//! subscriptions. Batches and inbound msgs that arrive while the link is down
//! are dropped, never queued — loss on the link, no delay in the cycle. A
//! dropped downlink connection is counted on the system's health as
//! `link_reconnect`; an uplink receive error is counted as
//! `uplink_disconnect`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use metor_fsw_ring::{NoWake, View};
use metor_proto::types::{
    IntoLenPacket, OwnedPacket, PACKET_HEADER_LEN, PacketId, PacketTy, Timestamp,
};
use metor_proto::vtable::VTable;
use metor_proto_stellar::{PacketSink, PacketStream};
use metor_proto_wkt::{ComponentMetadata, MsgStream, SetComponentMetadata, VTableMsg};
use stellarator::JoinHandleDropGuard;
use stellarator::buf::Slice;
use stellarator::io::{AsyncWrite, OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::TcpStream;
use thingbuf::mpsc;

use crate::binder::{BindPorts, RingSource};
use crate::descriptor::{Declarations, Delivery, PortDesc, SystemDescriptor};
use crate::health::HealthPort;
use crate::message::{MsgFanOut, NamedMsg, split_record};
use crate::registry::{AllOutputs, RegistryEntry};
use crate::system::{
    AsyncSystem, BuildCtx, BuildSystem, ConfigureError, CyclicSystem, Out, System, SystemInput,
    SystemOutput,
};

/// A failed send or receive on the link. The task driving the transport
/// backs off and redials, while the in-cycle stage keeps running and drops.
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
    /// `SetComponentMetadata` per component. Sent once per tap on every
    /// connect — the sender replays the full announce set after a redial.
    async fn announce(
        &mut self,
        msg: &VTableMsg,
        meta: &[ComponentMetadata],
    ) -> Result<(), TransportError>;

    /// Send one batch of concatenated length-prefixed packets. The buffer is
    /// returned alongside the result — on failure too — so its allocation can
    /// be recycled for a later batch either way.
    async fn send(&mut self, buf: Vec<u8>) -> (Result<(), TransportError>, Vec<u8>);
}

/// The read twin of [`Transport`]: yields the next inbound packet off the
/// connection. An error must leave the transport ready to redial — the uplink
/// backs off and calls `recv` again rather than going dead.
#[allow(async_fn_in_trait)]
pub trait RecvTransport {
    /// Receive the next packet, reading into `buf`, or an error if the link
    /// dropped. The caller recovers the buffer from the returned packet via
    /// [`OwnedPacket::into_buf`] once the payload is routed, so one buffer
    /// serves the link's whole lifetime.
    async fn recv(&mut self, buf: Vec<u8>) -> Result<OwnedPacket<Slice<Vec<u8>>>, TransportError>;

    /// Declare the message ids to subscribe to on connect. Called once before
    /// the first `recv`; the default is a no-op.
    fn subscribe(&mut self, _ids: &[PacketId]) {}
}

/// A live connection's write half plus the parked read half, which is held
/// only to keep the full socket open; the downlink never reads replies.
struct TcpConn {
    tx: OwnedWriter<TcpStream>,
    #[allow(dead_code)]
    rx: OwnedReader<TcpStream>,
}

/// A [`Transport`] that streams packets over one TCP connection to a ground
/// endpoint. An error drops the connection, and the next call redials.
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
    async fn ensure(&mut self) -> Result<&OwnedWriter<TcpStream>, TransportError> {
        if self.conn.is_none() {
            let stream = TcpStream::connect(self.addr)
                .await
                .map_err(TransportError::io)?;
            let (rx, tx) = stream.split();
            self.conn = Some(TcpConn { tx, rx });
        }
        Ok(&self.conn.as_ref().expect("just connected").tx)
    }
}

impl Transport for TcpTransport {
    async fn announce(
        &mut self,
        msg: &VTableMsg,
        meta: &[ComponentMetadata],
    ) -> Result<(), TransportError> {
        let res: Result<(), TransportError> = async {
            let tx = self.ensure().await?;
            let pkt = msg.into_len_packet();
            tx.write_all(pkt.inner)
                .await
                .0
                .map_err(TransportError::io)?;
            for m in meta {
                let pkt = (&SetComponentMetadata(m.clone())).into_len_packet();
                tx.write_all(pkt.inner)
                    .await
                    .0
                    .map_err(TransportError::io)?;
            }
            Ok(())
        }
        .await;
        // A failed write leaves the socket in an unknown state; drop it so
        // the next call redials.
        if res.is_err() {
            self.conn = None;
        }
        res
    }

    async fn send(&mut self, buf: Vec<u8>) -> (Result<(), TransportError>, Vec<u8>) {
        let tx = match self.ensure().await {
            Ok(tx) => tx,
            Err(e) => return (Err(e), buf),
        };
        let (res, buf) = tx.write_all(buf).await;
        match res {
            Ok(()) => (Ok(()), buf),
            Err(e) => {
                self.conn = None;
                (Err(TransportError::io(e)), buf)
            }
        }
    }
}

/// A [`RecvTransport`] that reads packets over the uplink's own TCP
/// connection, distinct from the downlink's. It connects lazily on the first
/// `recv`, then subscribes to the configured message ids; the broker relays a
/// message id only to clients that
/// sent a [`MsgStream`] for it, so without the subscription the uplink would
/// read nothing. The write half carries the subscription and is held for the
/// connection's lifetime; the read half yields the streamed msgs. An error
/// drops the connection, and the next `recv` redials and re-subscribes.
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
    async fn recv(&mut self, buf: Vec<u8>) -> Result<OwnedPacket<Slice<Vec<u8>>>, TransportError> {
        let res = match self.ensure().await {
            Ok(stream) => stream.next_grow(buf).await.map_err(TransportError::io),
            Err(e) => Err(e),
        };
        // A failed connect or read leaves the socket in an unknown state;
        // drop both halves so the next call redials (and re-subscribes,
        // since `ensure` sends the retained subscription set on connect).
        if res.is_err() {
            self.stream = None;
            self.sink = None;
        }
        res
    }

    fn subscribe(&mut self, ids: &[PacketId]) {
        self.subscribe_ids = ids.to_vec();
    }
}
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
                    || frames.iter().any(|f| f.as_str() == entry.name())
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
/// ```python
/// # optional; omit both lists to tap everything
/// m.add("telemetry", TcpDownlink(addr="127.0.0.1:2240", instances=["nav", "imu"], frames=["gyro_b"]))
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
/// `m.route(uplink, …, msg="…")` edges resolve like any other. An empty
/// `msgs` list means the uplink relays nothing (and warns); there is no
/// built-in default set.
///
/// ```python
/// m.add("uplink", TcpUplink(addr="127.0.0.1:2241", msgs=["SequenceCommand", "AlarmAck"]))
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

/// How many cycle batches may wait for the sender before the cycle side drops
/// new ones. The channel recycles its slots' buffers, so this also bounds the
/// downlink's steady-state allocation.
const BATCH_QUEUE_CAP: usize = 8;

/// The first redial delay after a transport error, doubling per consecutive
/// failure up to [`RECONNECT_MAX`] and resetting on success. Shared by the
/// downlink sender and the uplink reader.
const RECONNECT_MIN: Duration = Duration::from_millis(100);

/// The redial backoff cap: with the link down, a redial is attempted this
/// often, forever — the link recovers on its own whenever the far end comes
/// back.
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// The initial size of the uplink's recycled receive buffer. `recv` grows it
/// past this when a packet demands, and the grown buffer is what recycles, so
/// it converges to the largest packet seen and then never reallocates.
const UPLINK_RECV_BUF: usize = 1024;

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
    fn decls() -> Declarations {
        vec![
            PortDesc::of::<crate::SystemHealth>(),
            PortDesc::of::<crate::SystemLog>(),
        ]
        .into()
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
    fn decls() -> Declarations {
        Declarations::default()
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
    /// The read transport. A receive error is backed off and retried — the
    /// transport redials underneath — so the uplink survives a dropped link.
    recv: R,
    /// The current redial delay, doubling per consecutive receive error up to
    /// [`RECONNECT_MAX`] and resetting to [`RECONNECT_MIN`] on success.
    backoff: Duration,
    /// The receive buffer, lent to `recv` and recovered from each returned
    /// packet, so the link's whole lifetime reuses one allocation.
    recv_buf: Vec<u8>,
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
            recv,
            backoff: RECONNECT_MIN,
            recv_buf: vec![0u8; UPLINK_RECV_BUF],
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

    /// Receive packets until shutdown and forward each one
    /// verbatim on the minted output whose id matches, then recover the
    /// buffer from the packet for the next pass. A msg outside the configured
    /// set bumps `uplink_unroutable` (the broker should only relay subscribed
    /// ids, so this signals a broker or config mismatch), a full ring bumps
    /// `uplink_dropped`, and `Table` packets are silently ignored. A receive
    /// error bumps `uplink_disconnect` and backs the pass off; the transport
    /// redials on the next receive, so a dropped link recovers on its own.
    async fn run(
        &mut self,
        context: &crate::AsyncContext,
        _input: &mut Self::Input,
        output: &mut Self::Output,
    ) {
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
                health.error("uplink_bind_mismatch");
                health.end_cycle(Timestamp::now(), 0);
            }
            let ids: Vec<PacketId> = self.msgs.iter().map(|&(_, id)| id).collect();
            self.recv.subscribe(&ids);
            self.subscribed = true;
        }
        loop {
            let Some(received) = context
                .until_cancelled(self.recv.recv(std::mem::take(&mut self.recv_buf)))
                .await
            else {
                break;
            };
            match received {
                Ok(pkt) => {
                    self.backoff = RECONNECT_MIN;
                    // Msgs are routed; tables are ignored (the uplink is commands
                    // only). Either way the packet's backing buffer is recovered
                    // for the next receive.
                    if let OwnedPacket::Msg(m) = &pkt {
                        let (fan, health) = output.split();
                        match self.msgs.iter().position(|&(_, id)| id == m.id) {
                            Some(idx) => {
                                if fan.write_raw(idx, m.id, &m.buf).is_err() {
                                    health.error("uplink_dropped");
                                    health.end_cycle(Timestamp::now(), 0);
                                }
                            }
                            None => {
                                health.error("uplink_unroutable");
                                health.end_cycle(Timestamp::now(), 0);
                            }
                        }
                    }
                    self.recv_buf = pkt.into_buf().into_inner();
                }
                // A dropped link: count it, back off, and let the next pass
                // redial. An async system has no per-cycle driver flushing its
                // health, so publish immediately.
                Err(_) => {
                    let (_, health) = output.split();
                    health.error("uplink_disconnect");
                    health.end_cycle(Timestamp::now(), 0);
                    if context
                        .until_cancelled(stellarator::sleep(self.backoff))
                        .await
                        .is_none()
                    {
                        break;
                    }
                    self.backoff = (self.backoff * 2).min(RECONNECT_MAX);
                }
            }
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

/// The async sender task: announce every table tap, then send each queued
/// batch until the closed channel ends it (shutdown). A transport error
/// drops one batch, backs off, and re-enters the announce phase — replaying
/// the announces is what lets a restarted consumer decode tables again. The
/// cycle side is unaffected throughout: while the sender backs off, the
/// queue fills and the cycle side counts `telemetry_dropped`. A sent batch's
/// buffer goes back into its channel slot, so the allocation is recycled
/// instead of freed.
///
/// `drops` counts each established-connection loss (never the retries while
/// already down); the cycle side folds it into health as `link_reconnect`.
async fn run_sender<T: Transport>(
    mut transport: T,
    announces: Vec<Announce>,
    rx: mpsc::Receiver<Vec<u8>>,
    drops: Arc<AtomicU64>,
) {
    let mut backoff = RECONNECT_MIN;
    let mut was_up = false;
    'link: loop {
        for a in &announces {
            let msg = VTableMsg {
                id: a.packet_id,
                vtable: a.vtable.clone(),
            };
            if transport.announce(&msg, &a.metadata).await.is_err() {
                back_off(&drops, &mut was_up, &mut backoff).await;
                continue 'link;
            }
        }
        was_up = true;
        backoff = RECONNECT_MIN;
        // `recv_ref` parks until a batch is queued and returns `None` once the
        // cycle side has dropped its sender and the queue is drained (shutdown).
        while let Some(mut slot) = rx.recv_ref().await {
            let batch = std::mem::take(&mut *slot);
            let (res, buf) = transport.send(batch).await;
            *slot = buf;
            if res.is_err() {
                back_off(&drops, &mut was_up, &mut backoff).await;
                continue 'link;
            }
        }
        return;
    }
}

/// One redial delay in [`run_sender`]'s loop: count the drop when it is the
/// loss of an established connection (not a retry while already down), sleep
/// the current backoff, and double it toward [`RECONNECT_MAX`].
async fn back_off(drops: &AtomicU64, was_up: &mut bool, backoff: &mut Duration) {
    if *was_up {
        drops.fetch_add(1, Relaxed);
        *was_up = false;
    }
    stellarator::sleep(*backoff).await;
    *backoff = (*backoff * 2).min(RECONNECT_MAX);
}

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
    fn decls() -> Declarations {
        Declarations::default()
    }
}

impl BindPorts for TelemetryIn {
    fn bind<S: RingSource>(_src: &mut S) -> Self {
        TelemetryIn
    }
}

/// A read view into one tapped buffer plus the delivery axis and the [`Wire`]
/// framing projected from the entry.
struct Tap {
    view: View<NoWake>,
    delivery: Delivery,
    wire: Wire,
    /// Snapshot taps only: the ring's `committed` at the last contribution, so
    /// a cycle with no new record contributes nothing (the pinned newest
    /// record is not re-sent). `u64::MAX` means nothing contributed yet.
    last_committed: u64,
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
    /// The cycle side of the batch queue; taken by `shutdown` so the channel
    /// closes and the sender drains what is queued, then exits.
    tx: Option<mpsc::Sender<Vec<u8>>>,
    /// Established-connection losses counted by the sender task, drained into
    /// health as `link_reconnect` each cycle.
    drops: Arc<AtomicU64>,
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
    /// The resolved taps; each carries its own delivery axis and wire framing.
    taps: Vec<Tap>,
    /// The running state `init` assembles. `None` before `init`.
    started: Option<Started>,
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
                    exhausted.push(format!("{}.{}", entry.instance, entry.name()));
                    continue;
                }
            };
            // Delivery and wire are independent projections of the entry:
            // delivery picks how much each cycle contributes, schema picks
            // the framing.
            let wire = match entry.announce() {
                Some((vtable, metadata)) => {
                    let packet_id = (announces.len() as u16).to_le_bytes();
                    announces.push(Announce {
                        packet_id,
                        vtable,
                        metadata,
                    });
                    Wire::Table { packet_id }
                }
                None => Wire::Msg,
            };
            taps.push(Tap {
                view,
                delivery: entry.delivery(),
                wire,
                last_committed: u64::MAX,
            });
        }

        for key in &exhausted {
            let health = output.health();
            health.error("telemetry_reader_slot");
            health.log(
                crate::health::Level::Warn,
                &format!("no reader slot left on `{key}` — raise CoordinatorConfig::reader_slack"),
            );
        }

        let (tx, rx) = mpsc::channel::<Vec<u8>>(BATCH_QUEUE_CAP);
        let drops = Arc::new(AtomicU64::new(0));
        let sender = self.transport.take().map(|transport| {
            stellarator::spawn(run_sender(transport, announces, rx, drops.clone())).drop_guard()
        });
        self.taps = taps;
        self.started = Some(Started {
            tx: Some(tx),
            drops,
            sender,
        });
    }

    fn shutdown(&mut self, _output: &mut Self::Output) {
        if let Some(started) = &mut self.started {
            // closes the drop queue so the sender shutsdown
            started.tx = None;
        }
    }
}

/// Append one length-prefixed packet to `batch`: the framing [`LenPacket`]
/// builds (`metor_proto::types::LenPacket`), minus the intermediate
/// allocation.
fn append_packet(batch: &mut Vec<u8>, ty: PacketTy, id: PacketId, payload: &[u8]) {
    batch.extend_from_slice(&((PACKET_HEADER_LEN + payload.len()) as u32).to_le_bytes());
    batch.push(ty as u8);
    batch.extend_from_slice(&id);
    batch.push(0); // req_id
    batch.extend_from_slice(payload);
}

/// Frame one drained record onto `batch` per the tap's [`Wire`]: a `Table`
/// packet under the announce-assigned id, or a self-describing `Msg` packet
/// keyed by the record's own leading id (skipped if the record is too short
/// to carry one).
fn append_record(batch: &mut Vec<u8>, wire: &Wire, rec: &[u8]) {
    match wire {
        Wire::Table { packet_id } => append_packet(batch, PacketTy::Table, *packet_id, rec),
        Wire::Msg => {
            if let Some((id, payload)) = split_record(rec) {
                append_packet(batch, PacketTy::Msg, id, payload);
            }
        }
    }
}

impl<T: Transport + 'static> CyclicSystem for TelemetrySystem<T> {
    fn execute(&mut self, _now: Timestamp, _input: &mut Self::Input, output: &mut Self::Output) {
        let Some(started) = &self.started else {
            return;
        };
        // Fold the sender's connection losses into this cycle's health.
        for _ in 0..started.drops.swap(0, Relaxed) {
            output.health().error("link_reconnect");
        }
        // The link must never backpressure the mission. With no queue slot (the sender task
        // parked in redial backoff, or the link slower than the cycle) the taps still drain
        // below — records are consumed and DISCARDED (loss on the link, counted as
        // `telemetry_dropped`), because an undrained tap view stalls its producer's ring and
        // freezes every consumer of that output, not just telemetry.
        let mut batch = match &started.tx {
            Some(tx) => match tx.try_send_ref() {
                Ok(batch) => Some(batch),
                Err(_) => {
                    output.health().error("telemetry_dropped");
                    None
                }
            },
            None => None,
        };
        for tap in &mut self.taps {
            match tap.delivery {
                // An unchanged `committed` means no new record this cycle;
                // contribute nothing rather than re-sending the pinned record.
                Delivery::Snapshot => {
                    let committed = tap.view.committed();
                    if committed == tap.last_committed {
                        continue;
                    }
                    tap.last_committed = committed;
                    match tap.view.try_latest() {
                        Ok(Some(grant)) => {
                            if let Some(batch) = &mut batch {
                                append_record(batch, &tap.wire, &grant);
                            }
                        }
                        Ok(None) => {}
                        Err(_) => output.health().error("telemetry_input_corrupt"),
                    }
                }
                // Every record, in order.
                Delivery::Log => {
                    let wire = &tap.wire;
                    let batch = &mut batch;
                    let result = crate::port::drain_view(&mut tap.view, |rec| {
                        if let Some(batch) = batch.as_mut() {
                            append_record(batch, wire, rec);
                        }
                    });
                    if result.is_err() {
                        output.health().error("telemetry_input_corrupt");
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
