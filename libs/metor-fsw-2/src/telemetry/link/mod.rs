//! The in-FSW link server: one listening socket, pub/sub fan-out.
//!
//! [`LinkState`] is the pack-shared state behind the downlink and uplink
//! systems. It binds its listener at construction (a taken port is a
//! resolve-time error), spawns one accept task at
//! [`start`](crate::SharedLifecycle::start), and serves every connection the
//! same full stream: the announce replay first, then each cycle's batch.
//! The read half of every connection feeds one bounded inbound queue the
//! uplink system drains into its minted command ports.
//!
//! # Fan-out and the slow consumer
//!
//! `broadcast` enqueues the cycle's batch for each live connection and wakes
//! its writer task; the cycle never awaits the sockets. A
//! connection whose pending buffer would exceed [`PENDING_CAP`] misses the
//! whole batch and the loss is counted. A stalled ground tool is not
//! disconnected, does not delay the cycle, and does not cost its sibling
//! connections data. With no connections nothing is
//! buffered at all.
//!
//! # Ownership discipline
//!
//! Attached cyclic systems receive the ordinary scoped `&mut` grant to the
//! whole [`LinkState`]. The spawned server task never aliases that grant: it
//! owns its [`ServerState`] and exchanges commands and inbound messages with
//! `LinkState` over nonblocking channels.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

use bbqueue::prod_cons::stream::{StreamConsumer, StreamProducer};
use bbqueue::traits::coordination::cas::AtomicCoord;
use bbqueue::traits::notifier::maitake::MaiNotSpsc;
use bbqueue::traits::storage::BoxedSlice;
use bbqueue::{ArcBBQueue, BBQueue};
use metor_proto::types::{IntoLenPacket, Msg as _, OwnedPacket, PacketId};
use metor_proto_stellar::PacketStream;
use metor_proto_wkt::{
    LINK_PROTOCOL_VERSION, LinkInfo, MsgStream, NODE_PROTOCOL_MESSAGES, SetComponentMetadata,
    VTableMsg,
};
use stellarator::JoinHandleDropGuard;
use stellarator::io::{AsyncWrite, OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::{TcpListener, TcpStream};

use super::Announce;

/// Per-connection pending-byte cap: a batch that would push a connection's
/// buffered backlog past this is dropped for that connection alone.
const PENDING_CAP: usize = 1 << 20;

/// Inbound command queue cap, across all connections. Commands arrive at
/// human rates; a full queue means nobody is draining it.
const INBOUND_CAP: usize = 256;

/// One aggregate init and the first cycle may queue before the server task
/// first runs; steady state contributes at most one command per cycle.
const CONTROL_CAP: usize = 2;

/// The initial per-connection receive buffer; `next_grow` grows it to the
/// largest packet seen.
const RECV_BUF: usize = 1024;

/// Wiring params of the link server state (`state type="TcpServer"`).
#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct LinkParams {
    /// The address the FSW listens on.
    pub addr: std::net::SocketAddr,
    /// Human node name advertised over mDNS. `None` falls back to the OS
    /// hostname at advertise time.
    #[serde(default)]
    pub name: Option<String>,
}

/// One inbound `Msg` packet off any connection, queued for the uplink.
#[derive(Default)]
pub(crate) struct InboundMsg {
    pub id: PacketId,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy)]
struct InboundRecycle;

impl thingbuf::Recycle<InboundMsg> for InboundRecycle {
    fn new_element(&self) -> InboundMsg {
        InboundMsg {
            id: [0; 2],
            payload: Vec::new(),
        }
    }

    fn recycle(&self, msg: &mut InboundMsg) {
        msg.payload.clear();
    }
}

/// The counters the downlink drains into health each cycle.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct LinkStats {
    /// Connections accepted.
    pub accepted: u64,
    /// Connections that ended (error, EOF, or hangup).
    pub closed: u64,
    /// Whole batches dropped for one connection over the pending-byte cap.
    pub conn_dropped: u64,
    /// Inbound msgs dropped on a full queue.
    pub inbound_dropped: u64,
}

/// Observations crossing from the task-owned server into the cyclic state.
/// They are counters rather than mutable server data: the server and
/// connection tasks own all collections and buffers outright.
#[derive(Default)]
struct ServerMetrics {
    connections: AtomicUsize,
    accepted: AtomicU64,
    closed: AtomicU64,
    conn_dropped: AtomicU64,
    inbound_dropped: AtomicU64,
}

#[derive(Clone, Copy, Default)]
enum CommandKind {
    #[default]
    Idle,
    InstallAnnounces,
    Cycle,
}

struct RetainedUpdate {
    slot: usize,
    framed: Vec<u8>,
}

impl Default for RetainedUpdate {
    fn default() -> Self {
        Self {
            slot: 0,
            framed: Vec::new(),
        }
    }
}

/// One recycled thingbuf slot. `batch` and every retained buffer deliberately
/// survive recycling: sender and actor swap allocations through the slot.
#[derive(Default)]
struct ServerCommand {
    kind: CommandKind,
    announces: Option<Rc<Vec<u8>>>,
    retained_slots: usize,
    retained: Vec<RetainedUpdate>,
    batch: Vec<u8>,
}

#[derive(Clone, Copy)]
struct CommandRecycle;

impl thingbuf::Recycle<ServerCommand> for CommandRecycle {
    fn new_element(&self) -> ServerCommand {
        ServerCommand::default()
    }

    fn recycle(&self, command: &mut ServerCommand) {
        command.kind = CommandKind::Idle;
        command.announces = None;
        command.retained_slots = 0;
    }
}

type ControlSender = thingbuf::mpsc::Sender<ServerCommand, CommandRecycle>;
type ControlReceiver = thingbuf::mpsc::Receiver<ServerCommand, CommandRecycle>;
type InboundSender = thingbuf::mpsc::Sender<InboundMsg, InboundRecycle>;
type InboundReceiver = thingbuf::mpsc::Receiver<InboundMsg, InboundRecycle>;
type OutboundInner = BBQueue<BoxedSlice, AtomicCoord, MaiNotSpsc>;
type OutboundProducer = StreamProducer<std::sync::Arc<OutboundInner>>;
type OutboundConsumer = StreamConsumer<std::sync::Arc<OutboundInner>>;

/// The mutable server data, owned exclusively by `server_loop`.
struct ServerState {
    /// The pre-encoded identity + announce packets every new connection
    /// receives first. Accepted connections park in `pending` until set.
    announce_blob: Option<Rc<Vec<u8>>>,
    /// The newest framed record of each Snapshot message tap, replayed to
    /// every new connection right after the announces: latest-wins boot
    /// state (a wiring manifest, a sequence registry) a late joiner would
    /// otherwise never see. Slot-indexed by the downlink; an empty slot has
    /// no record yet.
    retained: Vec<Vec<u8>>,
    /// Streams accepted before the announcement replay is installed.
    pending: Vec<TcpStream>,
    conns: Vec<ServerConn>,
    inbound: InboundSender,
    metrics: Rc<ServerMetrics>,
}

/// The actor-owned handle for one live connection. The connection task owns
/// the socket halves and write buffers.
struct ServerConn {
    outbound: OutboundProducer,
    status: Rc<ConnStatus>,
    _guard: JoinHandleDropGuard<()>,
}

#[derive(Default)]
struct ConnStatus {
    queued: AtomicUsize,
    closed: AtomicBool,
}

/// The pack-shared link server state. See the module doc.
pub struct LinkState {
    /// Bound at construction, taken by `start`.
    listener: Option<TcpListener>,
    local_addr: std::net::SocketAddr,
    /// The configured node name; `None` resolves to the hostname in
    /// [`node_name`](Self::node_name).
    name: Option<String>,
    /// Configuration is mutated only through the cyclic systems' whole-state
    /// `&mut` grants, before it is encoded and sent to the server task.
    uplink_msgs: Vec<PacketId>,
    announces_set: bool,
    pending_announces: Option<Rc<Vec<u8>>>,
    control_tx: ControlSender,
    control_rx: Option<ControlReceiver>,
    inbound_tx: Option<InboundSender>,
    inbound_rx: InboundReceiver,
    retained_spares: Vec<RetainedUpdate>,
    pending_retained: Vec<RetainedUpdate>,
    pending_batch: Vec<u8>,
    metrics: Rc<ServerMetrics>,
    accept_guard: Option<JoinHandleDropGuard<()>>,
    /// The mDNS advertisement, live between `start` and `shutdown`; dropping
    /// it unregisters the service. `None` for a loopback bind or a daemon
    /// that couldn't start.
    advertiser: Option<mdns_sd::ServiceDaemon>,
}

impl LinkState {
    /// Bind the listener. Failure (the port is taken) surfaces as the state
    /// declaration's construction error at resolve.
    pub fn bind(addr: std::net::SocketAddr) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind(addr)?;
        let local_addr = listener.local_addr()?;
        let (control_tx, control_rx) = thingbuf::mpsc::with_recycle(CONTROL_CAP, CommandRecycle);
        let (inbound_tx, inbound_rx) = thingbuf::mpsc::with_recycle(INBOUND_CAP, InboundRecycle);
        Ok(Self {
            listener: Some(listener),
            local_addr,
            name: None,
            uplink_msgs: Vec::new(),
            announces_set: false,
            pending_announces: None,
            control_tx,
            control_rx: Some(control_rx),
            inbound_tx: Some(inbound_tx),
            inbound_rx,
            retained_spares: Vec::new(),
            pending_retained: Vec::new(),
            pending_batch: Vec::new(),
            metrics: Rc::new(ServerMetrics::default()),
            accept_guard: None,
            advertiser: None,
        })
    }

    /// Set the configured node name (from [`LinkParams::name`]). A builder
    /// step off [`bind`](Self::bind) so the registry factory threads it in
    /// without changing `bind`'s signature.
    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name;
        self
    }

    /// Advertise the uplink's command set for the identity packet. Runs
    /// from the uplink's init, before the downlink's
    /// [`set_announces`](Self::set_announces) freezes the replay (init
    /// order guarantees it: the downlink's `ReceiveAll` capability defers
    /// it last). Errors when the replay is already frozen: an uplink
    /// registered after the downlink, a config defect the caller reports
    /// through its health. Appends with id-dedup, so a second uplink's set
    /// unions in.
    pub(crate) fn add_uplink_msgs(&mut self, ids: &[PacketId]) -> Result<(), AnnouncesAlreadySet> {
        if self.announces_set {
            return Err(AnnouncesAlreadySet);
        }
        for id in ids {
            if !self.uplink_msgs.contains(id) {
                self.uplink_msgs.push(*id);
            }
        }
        Ok(())
    }

    /// Encode and install the identity + announce replay every connection
    /// receives before any data: the [`LinkInfo`] identity packet first,
    /// whose position is what lets a probing client decide the peer's mode
    /// from the first packet, then the schema announces. Called once, by
    /// the downlink's init; a second downlink on one server is a config
    /// defect its health reports.
    pub(crate) fn set_announces(
        &mut self,
        announces: &[Announce],
    ) -> Result<(), AnnouncesAlreadySet> {
        if self.announces_set {
            return Err(AnnouncesAlreadySet);
        }
        let info = LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids: self.uplink_msgs.clone(),
        };
        let mut blob = (&info).into_len_packet().inner;
        for a in announces {
            match a {
                Announce::Table {
                    packet_id,
                    vtable,
                    metadata,
                } => {
                    let msg = VTableMsg {
                        id: *packet_id,
                        vtable: vtable.clone(),
                    };
                    blob.extend_from_slice(&msg.into_len_packet().inner);
                    for m in metadata {
                        let pkt = (&SetComponentMetadata(m.clone())).into_len_packet();
                        blob.extend_from_slice(&pkt.inner);
                    }
                }
                Announce::Msg(msg) => blob.extend_from_slice(&msg.into_len_packet().inner),
            }
        }
        self.announces_set = true;
        self.pending_announces = Some(Rc::new(blob));
        Ok(())
    }

    /// Size the retained-record store: one slot per Snapshot message tap,
    /// assigned by the downlink's init alongside the announce set. This also
    /// publishes both pieces of init state as one recycled command.
    pub(crate) fn set_retained_slots(&mut self, n: usize) {
        let announces = self
            .pending_announces
            .take()
            .expect("set_announces runs immediately before retained sizing");
        let mut command = self.command();
        command.kind = CommandKind::InstallAnnounces;
        command.announces = Some(announces);
        command.retained_slots = n;
    }

    /// Move a freshly framed retained record to the server, replacing the
    /// caller's scratch with a recycled retained buffer. After the initial
    /// ping-pong buffers warm up, this performs no byte allocation or copy.
    pub(crate) fn retain(&mut self, slot: usize, framed: &mut Vec<u8>) {
        let mut update = self.retained_spares.pop().unwrap_or_default();
        update.slot = slot;
        update.framed.clear();
        std::mem::swap(framed, &mut update.framed);
        self.pending_retained.push(update);
    }

    /// Return the largest recycled cycle buffer to the downlink before it
    /// starts framing. This closes the actor/channel ownership loop early
    /// enough for the allocation to be reused in the current cycle.
    pub(crate) fn prepare_batch(&mut self, batch: &mut Vec<u8>) {
        if self.pending_batch.capacity() > batch.capacity() {
            std::mem::swap(batch, &mut self.pending_batch);
        }
        batch.clear();
    }

    /// Stage one batch of already-framed packets for every live connection.
    /// The staging allocation is reused; the cycle never awaits a socket.
    pub fn broadcast(&mut self, batch: &[u8]) {
        self.pending_batch.clear();
        self.pending_batch.extend_from_slice(batch);
        self.flush();
    }

    /// Ownership-taking form used by the downlink so its batch allocation
    /// can circulate through the actor rather than being copied.
    pub(crate) fn broadcast_buffer(&mut self, batch: &mut Vec<u8>) {
        self.pending_batch.clear();
        std::mem::swap(batch, &mut self.pending_batch);
    }

    /// Publish this cycle's retained replacements and optional live batch in
    /// one recycled thingbuf slot. The swap returns the slot's previous byte
    /// allocations to the cyclic side for the following cycle.
    pub(crate) fn flush(&mut self) {
        if self.pending_retained.is_empty() && self.pending_batch.is_empty() {
            return;
        }
        let Self {
            control_tx,
            retained_spares,
            pending_retained,
            pending_batch,
            ..
        } = self;
        let mut command = control_tx
            .try_send_ref()
            .expect("server drains the bounded control queue between cycles");
        command.kind = CommandKind::Cycle;
        std::mem::swap(&mut command.retained, pending_retained);
        std::mem::swap(&mut command.batch, pending_batch);
        retained_spares.append(pending_retained);
    }

    /// The current live-connection count, for the telemetered gauge.
    pub fn connections(&mut self) -> usize {
        self.metrics.connections.load(Relaxed)
    }

    /// Drain the accumulated counters, for the downlink's health fold.
    pub fn take_stats(&mut self) -> LinkStats {
        LinkStats {
            accepted: self.metrics.accepted.swap(0, Relaxed),
            closed: self.metrics.closed.swap(0, Relaxed),
            conn_dropped: self.metrics.conn_dropped.swap(0, Relaxed),
            inbound_dropped: self.metrics.inbound_dropped.swap(0, Relaxed),
        }
    }

    /// Drain the inbound command queue in arrival order.
    pub fn drain_inbound(&mut self, mut f: impl FnMut(PacketId, &[u8])) {
        while let Ok(msg) = self.inbound_rx.try_recv_ref() {
            f(msg.id, &msg.payload);
        }
    }

    fn command(&self) -> thingbuf::mpsc::SendRef<'_, ServerCommand> {
        self.control_tx
            .try_send_ref()
            .expect("server drains the bounded control queue between cycles")
    }
}

/// A second `set_announces` (or an `add_uplink_msgs` past the freeze) on
/// one server: a mis-ordered link pack, which the caller reports through
/// its health rather than clobbering the replay.
pub(crate) struct AnnouncesAlreadySet;

impl crate::SharedLifecycle for LinkState {
    /// Spawn the accept loop; runs on the coordinator's loop task before the
    /// first attached system's init, so a runtime is up and connections can
    /// arrive before the downlink announces (they park on the replay).
    fn start(&mut self) {
        let name = self
            .name
            .clone()
            .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().into_owned());
        self.advertiser = super::discovery::advertise(&name, self.local_addr);
        let listener = self.listener.take().expect("start runs once");
        let commands = self.control_rx.take().expect("start runs once");
        let inbound = self.inbound_tx.take().expect("start runs once");
        let state = ServerState {
            announce_blob: None,
            retained: Vec::new(),
            pending: Vec::new(),
            conns: Vec::new(),
            inbound,
            metrics: self.metrics.clone(),
        };
        self.accept_guard =
            Some(stellarator::spawn(server_loop(listener, commands, state)).drop_guard());
    }

    /// Dropping the guards cancels the accept loop and every connection
    /// task; the sockets close with them. Shutting down the mDNS daemon
    /// unregisters the advertisement with a goodbye.
    fn shutdown(&mut self) {
        if let Some(advertiser) = self.advertiser.take() {
            let _ = advertiser.shutdown();
        }
        self.accept_guard = None;
        self.metrics.connections.store(0, Relaxed);
    }
}

enum ServerEvent {
    Accept(Result<TcpStream, stellarator::Error>),
    Command,
    Closed,
}

/// Own the accept socket and every mutable server collection. Socket tasks
/// share only channels, atomics, and immutable batches with this actor.
async fn server_loop(listener: TcpListener, commands: ControlReceiver, mut state: ServerState) {
    loop {
        state.prune_closed();
        // A continuously-ready listener must not starve cycle commands.
        // Drain the finite command burst before waiting on accept/control.
        while let Ok(mut command) = commands.try_recv_ref() {
            state.command(&mut command);
        }
        let event = futures_lite::future::race(
            async { ServerEvent::Accept(listener.accept().await) },
            async {
                match commands.recv_ref().await {
                    Some(mut command) => {
                        state.command(&mut command);
                        ServerEvent::Command
                    }
                    None => ServerEvent::Closed,
                }
            },
        )
        .await;
        match event {
            ServerEvent::Accept(Ok(stream)) => state.accept(stream),
            // Accept errors are transient (per-connection resets, fd
            // pressure); the listener itself stays valid.
            ServerEvent::Accept(Err(_)) => {}
            ServerEvent::Command => {}
            // The root state dropped every sender: shutdown.
            ServerEvent::Closed => return,
        }
    }
}

impl ServerState {
    fn command(&mut self, command: &mut ServerCommand) {
        match command.kind {
            CommandKind::InstallAnnounces => {
                let blob = command
                    .announces
                    .take()
                    .expect("install command carries replay");
                assert!(
                    self.announce_blob.replace(blob).is_none(),
                    "LinkState rejects a second announcement replay"
                );
                self.retained.resize(command.retained_slots, Vec::new());
                for stream in std::mem::take(&mut self.pending) {
                    self.activate(stream);
                }
            }
            CommandKind::Cycle => {
                for update in &mut command.retained {
                    std::mem::swap(&mut self.retained[update.slot], &mut update.framed);
                }
                if !command.batch.is_empty() {
                    self.broadcast(&command.batch);
                }
            }
            CommandKind::Idle => unreachable!("only published commands are received"),
        }
    }

    fn accept(&mut self, stream: TcpStream) {
        if self.announce_blob.is_some() {
            self.activate(stream);
        } else {
            self.pending.push(stream);
        }
    }

    fn activate(&mut self, stream: TcpStream) {
        let blob = self
            .announce_blob
            .as_ref()
            .expect("connections activate after announces");
        let initial = seed_replay(blob, &self.retained);
        // bbqueue keeps one byte unused to distinguish a wrapped full queue
        // from an empty one. The externally visible byte cap remains exact.
        let outbound = ArcBBQueue::new_with_storage(BoxedSlice::new(PENDING_CAP + 1));
        let outbound_tx = outbound.stream_producer();
        let outbound_rx = outbound.stream_consumer();
        let status = Rc::new(ConnStatus::default());
        let guard = stellarator::spawn(conn_task(
            self.inbound.clone(),
            self.metrics.clone(),
            status.clone(),
            outbound_rx,
            initial,
            stream,
        ))
        .drop_guard();
        self.metrics.accepted.fetch_add(1, Relaxed);
        self.metrics.connections.fetch_add(1, Relaxed);
        self.conns.push(ServerConn {
            outbound: outbound_tx,
            status,
            _guard: guard,
        });
    }

    fn broadcast(&mut self, batch: &[u8]) {
        self.prune_closed();
        for conn in &self.conns {
            if !enqueue_bytes(&conn.outbound, &conn.status.queued, batch) {
                self.metrics.conn_dropped.fetch_add(1, Relaxed);
            }
        }
    }

    fn prune_closed(&mut self) {
        self.conns.retain(|conn| !conn.status.closed.load(Relaxed));
    }
}

/// Copy one whole batch into a connection's fixed bbqueue storage. The byte
/// counter reserves capacity before any grants are made, preserving the
/// whole-batch drop policy even when a batch wraps into two contiguous
/// grants.
fn enqueue_bytes(outbound: &OutboundProducer, queued: &AtomicUsize, batch: &[u8]) -> bool {
    let used = queued.load(Relaxed);
    if batch.len() > PENDING_CAP.saturating_sub(used) {
        return false;
    }

    let mut remaining = batch;
    while !remaining.is_empty() {
        let mut grant = outbound
            .grant_max_remaining(remaining.len())
            .expect("bbqueue capacity agrees with the reserved byte count");
        let len = grant.len();
        grant.copy_from_slice(&remaining[..len]);
        grant.commit(len);
        remaining = &remaining[len..];
    }
    queued.fetch_add(batch.len(), Relaxed);
    true
}

/// A new connection's opening bytes: the identity + announce blob, then
/// the newest retained record of every Snapshot message tap. Schemas
/// come first, then the latest-wins boot state a late joiner missed live.
fn seed_replay(blob: &[u8], retained: &[Vec<u8>]) -> Vec<u8> {
    let mut seed = blob.to_vec();
    for record in retained {
        seed.extend_from_slice(record);
    }
    seed
}

/// One connection's life: writer and reader race, and whichever half fails
/// first ends both (the socket closes when the halves drop).
async fn conn_task(
    inbound: InboundSender,
    metrics: Rc<ServerMetrics>,
    status: Rc<ConnStatus>,
    outbound: OutboundConsumer,
    initial: Vec<u8>,
    stream: TcpStream,
) {
    let (rx, tx) = stream.split();
    futures_lite::future::race(
        write_half(initial, status.clone(), outbound, tx),
        read_half(inbound, metrics.clone(), rx),
    )
    .await;
    status.closed.store(true, Relaxed);
    metrics.closed.fetch_add(1, Relaxed);
    metrics.connections.fetch_sub(1, Relaxed);
}

/// Coalesce ready batches into one reusable buffer and write it whole. The
/// server only enqueues immutable batches, so neither side needs interior
/// mutability around the writer's buffers.
async fn write_half(
    initial: Vec<u8>,
    status: Rc<ConnStatus>,
    outbound: OutboundConsumer,
    tx: OwnedWriter<TcpStream>,
) {
    let mut spare = initial;
    loop {
        if spare.is_empty() {
            let grant = outbound.wait_read().await;
            spare.extend_from_slice(&grant);
            let len = grant.len();
            grant.release(len);
            status.queued.fetch_sub(len, Relaxed);
            while let Ok(grant) = outbound.read() {
                spare.extend_from_slice(&grant);
                let len = grant.len();
                grant.release(len);
                status.queued.fetch_sub(len, Relaxed);
            }
        }
        let (res, mut buf) = tx.write_all(spare).await;
        buf.clear();
        spare = buf;
        if res.is_err() {
            return;
        }
    }
}

/// Read packets until error or EOF: inbound `Msg` packets queue for the
/// uplink; legacy [`MsgStream`] subscriptions and stray node/link protocol
/// messages (a client's `GetDbInfo` identity probe) are accepted and
/// ignored, so probing never pollutes the command queue; `Table` packets
/// are ignored too.
async fn read_half(inbound: InboundSender, metrics: Rc<ServerMetrics>, rx: OwnedReader<TcpStream>) {
    let mut stream = PacketStream::new(rx);
    let mut buf = vec![0u8; RECV_BUF];
    loop {
        match stream.next_grow(buf).await {
            Ok(OwnedPacket::Msg(m))
                if m.id != MsgStream::ID && !NODE_PROTOCOL_MESSAGES.contains(&m.id) =>
            {
                push_inbound(&inbound, &metrics, m.id, &m.buf);
                buf = m.buf.into_inner().into_inner();
            }
            Ok(pkt) => buf = pkt.into_buf().into_inner(),
            Err(_) => return,
        }
    }
}

/// Queue one inbound msg, dropping (counted) on a full queue.
fn push_inbound(inbound: &InboundSender, metrics: &ServerMetrics, id: PacketId, payload: &[u8]) {
    match inbound.try_send_ref() {
        Ok(mut msg) => {
            msg.id = id;
            msg.payload.clear();
            msg.payload.extend_from_slice(payload);
        }
        Err(thingbuf::mpsc::errors::TrySendError::Full(_)) => {
            metrics.inbound_dropped.fetch_add(1, Relaxed);
        }
        Err(thingbuf::mpsc::errors::TrySendError::Closed(_)) => {}
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::time::Duration;

    use metor_proto::types::LenPacket;
    use stellarator::io::AsyncRead as _;

    use super::*;

    #[test]
    fn inbound_slots_reuse_payload_capacity() {
        let (tx, rx) = thingbuf::mpsc::with_recycle(1, InboundRecycle);
        {
            let mut msg = tx.try_send_ref().unwrap();
            msg.payload.extend_from_slice(&[0x55; RECV_BUF]);
        }
        let capacity = {
            let msg = rx.try_recv_ref().unwrap();
            msg.payload.capacity()
        };
        let msg = tx.try_send_ref().unwrap();
        assert!(msg.payload.is_empty());
        assert_eq!(msg.payload.capacity(), capacity);
    }

    #[test]
    fn outbound_queue_enforces_the_byte_cap_and_reuses_storage() {
        let queue: ArcBBQueue<BoxedSlice, AtomicCoord, MaiNotSpsc> =
            ArcBBQueue::new_with_storage(BoxedSlice::new(PENDING_CAP + 1));
        let tx = queue.stream_producer();
        let rx = queue.stream_consumer();
        let queued = AtomicUsize::new(0);

        assert!(enqueue_bytes(&tx, &queued, &[0x55; PENDING_CAP]));
        assert!(!enqueue_bytes(&tx, &queued, &[0x55]));

        let grant = rx.read().unwrap();
        assert_eq!(grant.len(), PENDING_CAP);
        grant.release(PENDING_CAP);
        queued.fetch_sub(PENDING_CAP, Relaxed);
        assert!(rx.read().is_err());
        assert!(enqueue_bytes(&tx, &queued, &[0xAA; PENDING_CAP]));

        // Reusing a completely consumed ring may wrap once, but bbqueue
        // exposes the two regions without allocating either one.
        let first = rx.read().unwrap();
        let first_len = first.len();
        assert_eq!(&*first, &[0xAA; 1]);
        first.release(first_len);
        let second = rx.read().unwrap();
        assert_eq!(first_len + second.len(), PENDING_CAP);
        assert!(second.iter().all(|byte| *byte == 0xAA));
    }

    #[test]
    fn link_replays_fans_out_and_ingests_without_per_message_buffers() {
        stellarator::run(|| async {
            let mut link = LinkState::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
            let addr = link.local_addr;
            crate::SharedLifecycle::start(&mut link);

            // This connection is accepted before the replay exists and parks.
            let first = TcpStream::connect(addr).await.unwrap();
            let command_id = [0x12, 0x34];
            assert!(link.add_uplink_msgs(&[command_id]).is_ok());
            assert!(link.set_announces(&[]).is_ok());
            link.set_retained_slots(1);

            let info = LinkInfo {
                protocol_version: LINK_PROTOCOL_VERSION,
                features: 0,
                command_ids: vec![command_id],
            }
            .into_len_packet()
            .inner;
            let replay = vec![0; info.len()];
            let (res, replay) = first.read_exact(replay).await;
            res.unwrap();
            assert_eq!(replay, info);

            // Retention and live fan-out travel in one recycled cycle slot.
            let mut retained = b"retained".to_vec();
            link.retain(0, &mut retained);
            let mut live = b"live".to_vec();
            link.broadcast_buffer(&mut live);
            link.flush();

            let live_read = vec![0; 4];
            let (res, live_read) = first.read_exact(live_read).await;
            res.unwrap();
            assert_eq!(live_read, b"live");

            // A late joiner receives schemas first and the actor-owned newest
            // retained bytes second.
            let second = TcpStream::connect(addr).await.unwrap();
            let mut expected = info.clone();
            expected.extend_from_slice(b"retained");
            let replay = vec![0; expected.len()];
            let (res, replay) = second.read_exact(replay).await;
            res.unwrap();
            assert_eq!(replay, expected);

            let mut msg = LenPacket::msg(command_id, 3);
            msg.extend_from_slice(b"cmd");
            first.write_all(msg.inner).await.0.unwrap();
            let mut received = Vec::new();
            for _ in 0..8 {
                stellarator::sleep(Duration::from_millis(1)).await;
                link.drain_inbound(|id, payload| received.push((id, payload.to_vec())));
                if !received.is_empty() {
                    break;
                }
            }
            assert_eq!(received, vec![(command_id, b"cmd".to_vec())]);

            crate::SharedLifecycle::shutdown(&mut link);
        });
    }
}
