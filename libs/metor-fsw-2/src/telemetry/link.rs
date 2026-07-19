//! The in-FSW link server: one listening socket, pub/sub fan-out.
//!
//! [`LinkState`] is the pack-shared state behind the downlink and uplink
//! systems. It binds its listener at construction (a taken port is a
//! resolve-time error), spawns one accept task at
//! [`start`](crate::SharedLifecycle::start), and serves every connection the
//! same full stream: the announce replay first — the same packets a
//! reconnecting outbound downlink used to send — then each cycle's batch.
//! The read half of every connection feeds one bounded inbound queue the
//! uplink system drains into its minted command ports.
//!
//! # Fan-out and the slow consumer
//!
//! `broadcast` appends the cycle's batch to each live connection's pending
//! buffer and wakes its writer task; the cycle never awaits the sockets. A
//! connection whose pending buffer would exceed [`PENDING_CAP`] misses the
//! whole batch and the loss is counted — the zmq-pub policy: a stalled
//! ground tool is never disconnected, never delays the cycle, and never
//! costs its sibling connections data. With no connections nothing is
//! buffered at all.
//!
//! # Borrow discipline
//!
//! The state's `&mut` grant belongs to the attached systems; everything the
//! spawned tasks touch lives behind the `Rc`'d [`ServerShared`] interior,
//! with `RefCell` borrows that never cross an await (see the
//! [`shared`](crate::shared) module doc).

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use metor_proto::types::{IntoLenPacket, Msg as _, OwnedPacket, PacketId};
use metor_proto_stellar::PacketStream;
use metor_proto_wkt::{MsgStream, SetComponentMetadata, VTableMsg};
use stellarator::JoinHandleDropGuard;
use stellarator::io::{AsyncWrite, OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::{TcpListener, TcpStream};
use stellarator::sync::WaitQueue;

use super::Announce;

/// Per-connection pending-byte cap: a batch that would push a connection's
/// buffered backlog past this is dropped for that connection alone.
const PENDING_CAP: usize = 1 << 20;

/// Inbound command queue cap, across all connections. Commands arrive at
/// human rates; a full queue means nobody is draining it.
const INBOUND_CAP: usize = 256;

/// The initial per-connection receive buffer; `next_grow` grows it to the
/// largest packet seen.
const RECV_BUF: usize = 1024;

/// Wiring params of the link server state (`state type="TcpServer"`).
#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct LinkParams {
    /// The address the FSW listens on.
    pub addr: std::net::SocketAddr,
}

/// One inbound `Msg` packet off any connection, queued for the uplink.
pub(crate) struct InboundMsg {
    pub id: PacketId,
    pub payload: Vec<u8>,
}

/// The counters the downlink drains into health each cycle.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct LinkStats {
    /// Connections accepted.
    pub accepted: u64,
    /// Connections that ended (error, EOF, or hangup).
    pub closed: u64,
    /// Whole batches dropped for one connection over [`PENDING_CAP`].
    pub conn_dropped: u64,
    /// Inbound msgs dropped on a full queue.
    pub inbound_dropped: u64,
}

/// The task-visible interior: connection set, announce replay blob, inbound
/// queue, counters.
struct ServerShared {
    /// The pre-encoded announce packets every new connection receives first.
    /// `None` until the downlink's init supplies them; accepted connections
    /// park on [`announce_ready`](Self::announce_ready) meanwhile.
    announce_blob: RefCell<Option<Rc<Vec<u8>>>>,
    announce_ready: WaitQueue,
    conns: RefCell<Vec<Rc<Conn>>>,
    /// Per-connection task guards, pruned as connections close; clearing
    /// them cancels the tasks (shutdown).
    conn_guards: RefCell<Vec<(Rc<Conn>, JoinHandleDropGuard<()>)>>,
    inbound: RefCell<VecDeque<InboundMsg>>,
    accepted: Cell<u64>,
    closed: Cell<u64>,
    conn_dropped: Cell<u64>,
    inbound_dropped: Cell<u64>,
}

/// One live connection: the double-buffered outbound bytes and the writer's
/// wake. `broadcast` appends into `pending`; the writer swaps it against its
/// own spare and writes.
struct Conn {
    pending: RefCell<Vec<u8>>,
    wake: WaitQueue,
    closed: Cell<bool>,
}

/// The pack-shared link server state. See the module doc.
pub struct LinkState {
    /// Bound at construction, taken by `start`.
    listener: Option<TcpListener>,
    local_addr: std::net::SocketAddr,
    shared: Rc<ServerShared>,
    accept_guard: Option<JoinHandleDropGuard<()>>,
}

impl LinkState {
    /// Bind the listener. Failure (the port is taken) surfaces as the state
    /// declaration's construction error at resolve.
    pub fn bind(addr: std::net::SocketAddr) -> Result<Self, std::io::Error> {
        let listener = TcpListener::bind(addr)?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener: Some(listener),
            local_addr,
            shared: Rc::new(ServerShared {
                announce_blob: RefCell::new(None),
                announce_ready: WaitQueue::new(),
                conns: RefCell::new(Vec::new()),
                conn_guards: RefCell::new(Vec::new()),
                inbound: RefCell::new(VecDeque::new()),
                accepted: Cell::new(0),
                closed: Cell::new(0),
                conn_dropped: Cell::new(0),
                inbound_dropped: Cell::new(0),
            }),
            accept_guard: None,
        })
    }

    /// The bound address — the advertised port when the config said port 0.
    pub fn local_addr(&self) -> std::net::SocketAddr {
        self.local_addr
    }

    /// Encode and install the announce replay every connection receives
    /// before any data. Called once, by the downlink's init; a second
    /// downlink on one server is a config defect its health reports.
    pub(crate) fn set_announces(&self, announces: &[Announce]) -> Result<(), AnnouncesAlreadySet> {
        if self.shared.announce_blob.borrow().is_some() {
            return Err(AnnouncesAlreadySet);
        }
        let mut blob = Vec::new();
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
        *self.shared.announce_blob.borrow_mut() = Some(Rc::new(blob));
        self.shared.announce_ready.wake_all();
        Ok(())
    }

    /// Whether any connection is live; the downlink skips framing entirely
    /// when nothing would receive it (taps still drain).
    pub fn has_connections(&self) -> bool {
        self.shared.conns.borrow_mut().retain(|c| !c.closed.get());
        !self.shared.conns.borrow().is_empty()
    }

    /// Append one batch of already-framed packets to every live connection
    /// and wake the writers. A connection over its pending cap misses the
    /// whole batch (counted); the cycle never awaits a socket.
    pub fn broadcast(&self, batch: &[u8]) {
        let mut conns = self.shared.conns.borrow_mut();
        conns.retain(|c| !c.closed.get());
        for conn in conns.iter() {
            let mut pending = conn.pending.borrow_mut();
            if pending.len() + batch.len() > PENDING_CAP {
                self.shared
                    .conn_dropped
                    .set(self.shared.conn_dropped.get() + 1);
                continue;
            }
            pending.extend_from_slice(batch);
            conn.wake.wake_all();
        }
    }

    /// The current live-connection count, for the telemetered gauge.
    pub fn connections(&self) -> usize {
        self.shared.conns.borrow_mut().retain(|c| !c.closed.get());
        self.shared.conns.borrow().len()
    }

    /// Drain the accumulated counters, for the downlink's health fold.
    pub fn take_stats(&self) -> LinkStats {
        LinkStats {
            accepted: self.shared.accepted.take(),
            closed: self.shared.closed.take(),
            conn_dropped: self.shared.conn_dropped.take(),
            inbound_dropped: self.shared.inbound_dropped.take(),
        }
    }

    /// Drain the inbound command queue in arrival order.
    pub fn drain_inbound(&mut self, mut f: impl FnMut(PacketId, &[u8])) {
        while let Some(msg) = {
            let popped = self.shared.inbound.borrow_mut().pop_front();
            popped
        } {
            f(msg.id, &msg.payload);
        }
    }

    #[cfg(test)]
    pub(crate) fn push_test_conn(&self) -> Rc<Conn> {
        let conn = Rc::new(Conn {
            pending: RefCell::new(
                self.shared
                    .announce_blob
                    .borrow()
                    .as_ref()
                    .map(|b| b.to_vec())
                    .unwrap_or_default(),
            ),
            wake: WaitQueue::new(),
            closed: Cell::new(false),
        });
        self.shared.conns.borrow_mut().push(conn.clone());
        self.shared.accepted.set(self.shared.accepted.get() + 1);
        conn
    }

    #[cfg(test)]
    pub(crate) fn push_inbound(&self, id: PacketId, payload: &[u8]) {
        push_inbound(&self.shared, id, payload);
    }
}

/// A second `set_announces` on one server: two downlinks share it, which
/// the caller reports through its health rather than clobbering the replay.
pub(crate) struct AnnouncesAlreadySet;

#[cfg(test)]
impl Conn {
    pub(crate) fn pending_bytes(&self) -> Vec<u8> {
        self.pending.borrow().clone()
    }

    pub(crate) fn close(&self) {
        self.closed.set(true);
    }
}

impl crate::SharedLifecycle for LinkState {
    /// Spawn the accept loop; runs on the coordinator's loop task before the
    /// first attached system's init, so a runtime is up and connections can
    /// arrive before the downlink announces (they park on the replay).
    fn start(&mut self) {
        let listener = self.listener.take().expect("start runs once");
        let shared = self.shared.clone();
        self.accept_guard = Some(stellarator::spawn(accept_loop(listener, shared)).drop_guard());
    }

    /// Dropping the guards cancels the accept loop and every connection
    /// task; the sockets close with them.
    fn shutdown(&mut self) {
        self.accept_guard = None;
        self.shared.conn_guards.borrow_mut().clear();
        self.shared.conns.borrow_mut().clear();
    }
}

/// Accept connections forever. Each waits for the announce replay, seeds
/// its pending buffer with it (so replay-then-data ordering is the buffer's
/// natural order), joins the connection set, and gets a task racing its
/// writer and reader halves.
async fn accept_loop(listener: TcpListener, shared: Rc<ServerShared>) {
    loop {
        let stream = match listener.accept().await {
            Ok(stream) => stream,
            // Accept errors are transient (per-connection resets, fd
            // pressure); the listener itself stays valid.
            Err(_) => continue,
        };
        let blob = loop {
            let installed = shared.announce_blob.borrow().clone();
            match installed {
                Some(blob) => break blob,
                None => {
                    let _ = shared.announce_ready.wait().await;
                }
            }
        };
        let conn = Rc::new(Conn {
            pending: RefCell::new(blob.to_vec()),
            wake: WaitQueue::new(),
            closed: Cell::new(false),
        });
        shared.accepted.set(shared.accepted.get() + 1);
        shared.conns.borrow_mut().push(conn.clone());
        // Prune guards of connections that already ended; their tasks ran to
        // completion (no awaits past the closed mark), so the drop is a
        // no-op cancel.
        shared
            .conn_guards
            .borrow_mut()
            .retain(|(c, _)| !c.closed.get());
        let task = stellarator::spawn(conn_task(shared.clone(), conn.clone(), stream)).drop_guard();
        shared.conn_guards.borrow_mut().push((conn, task));
    }
}

/// One connection's life: writer and reader race, and whichever half fails
/// first ends both (the socket closes when the halves drop).
async fn conn_task(shared: Rc<ServerShared>, conn: Rc<Conn>, stream: TcpStream) {
    let (rx, tx) = stream.split();
    futures_lite::future::race(write_half(conn.clone(), tx), read_half(shared.clone(), rx)).await;
    conn.closed.set(true);
    shared.closed.set(shared.closed.get() + 1);
    shared.conns.borrow_mut().retain(|c| !c.closed.get());
}

/// Swap-and-write until the connection closes: the pending buffer moves out
/// whole against a recycled spare, so `broadcast` never contends with an
/// in-flight write.
async fn write_half(conn: Rc<Conn>, tx: OwnedWriter<TcpStream>) {
    let mut spare = Vec::new();
    loop {
        {
            let mut pending = conn.pending.borrow_mut();
            if !pending.is_empty() {
                std::mem::swap(&mut *pending, &mut spare);
            }
        }
        if !spare.is_empty() {
            let (res, mut buf) = tx.write_all(spare).await;
            buf.clear();
            spare = buf;
            if res.is_err() {
                return;
            }
            continue;
        }
        if conn.closed.get() {
            return;
        }
        let _ = conn.wake.wait().await;
    }
}

/// Read packets until error or EOF: inbound `Msg` packets queue for the
/// uplink; legacy [`MsgStream`] subscriptions are accepted and ignored
/// (every connection gets the full stream); `Table` packets are ignored.
async fn read_half(shared: Rc<ServerShared>, rx: OwnedReader<TcpStream>) {
    let mut stream = PacketStream::new(rx);
    let mut buf = vec![0u8; RECV_BUF];
    loop {
        match stream.next_grow(buf).await {
            Ok(pkt) => {
                if let OwnedPacket::Msg(m) = &pkt
                    && m.id != MsgStream::ID
                {
                    push_inbound(&shared, m.id, &m.buf);
                }
                buf = pkt.into_buf().into_inner();
            }
            Err(_) => return,
        }
    }
}

/// Queue one inbound msg, dropping (counted) on a full queue.
fn push_inbound(shared: &ServerShared, id: PacketId, payload: &[u8]) {
    let mut inbound = shared.inbound.borrow_mut();
    if inbound.len() >= INBOUND_CAP {
        shared.inbound_dropped.set(shared.inbound_dropped.get() + 1);
        return;
    }
    inbound.push_back(InboundMsg {
        id,
        payload: payload.to_vec(),
    });
}

#[cfg(test)]
mod tests;
