//! A link's connections over one `Endpoint`: each slot's buffers and the
//! ops that move them, polled once per turn. A listening link listens once
//! and accepts; a dialing link connects.

use core::net::SocketAddr;
use core::time::Duration;

use zerocopy::FromBytes;

use crate::coordinator::MakeError;
use crate::io::{
    Backoff, Buf, Cap, CloseOp, Endpoint, Handle, IoError, IoInit, ListenOp, Listener, Pause,
    Pending, RawAddr, RecvOp, SendOp, Slot, Stream, StreamOp, TimeoutOp,
};

use super::deframe::{Deframer, Oversized};
use super::peer::ConnectionState;

/// The ops `slots` connections have in flight at most: the stream op, a
/// backoff timer, a refused stream's close, a lost listener's close, and
/// per slot a recv, a send, a write timer, and a close.
pub(crate) const fn ops_for(slots: usize) -> usize {
    4 + 4 * slots
}

/// How a link's connections are sized and run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConnsConfig {
    /// The most connections open at once. A dialing link has one.
    pub(crate) slots: usize,
    /// The bytes of each connection's receive buffer.
    pub(crate) rx_cap: usize,
    /// The bytes of each of a connection's two send buffers.
    pub(crate) tx_cap: usize,
    /// Whether the endpoint dials. A dialing link opens a stream only into a
    /// free slot, and checks each peer's announce.
    pub(crate) dialing: bool,
    /// How long a send may go without finishing before its connection
    /// closes.
    pub(crate) write_timeout: Duration,
}

impl ConnsConfig {
    /// The slots the connections take: one for a dialer.
    pub(crate) fn slot_count(&self) -> usize {
        if self.dialing { 1 } else { self.slots }
    }
}

/// Socket counters for a link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub(crate) connections: u32,
    pub(crate) bytes_out: u64,
    pub(crate) batches_dropped: u64,
}

/// What happened to a link's connections, for its log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// A stream opened, from this peer when its address is known.
    Opened(Option<SocketAddr>),
    Closed,
    /// A peer sent a packet of `len` bytes, past the receive buffer, and its
    /// connection closed.
    Oversized {
        len: usize,
    },
    /// A stream arrived with every slot taken, and was closed.
    Refused,
    /// The endpoint failed to listen or open a stream.
    Failed(IoError),
}

/// What one [`Conns::poll`] pass did: the connections it closed, and what
/// became of the stream it reaped.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Turn {
    /// The connections closed for any reason but an oversized packet.
    closed: usize,
    /// The connections closed for an oversized packet.
    oversized: usize,
    /// The largest of those packets' bytes.
    oversized_len: usize,
    stream: Option<Event>,
}

impl Turn {
    /// The pass's events: a close each, then the stream's. Each oversized
    /// close reports the largest packet of the pass.
    pub(crate) fn events(self) -> impl Iterator<Item = Event> {
        let oversized = Event::Oversized {
            len: self.oversized_len,
        };
        core::iter::repeat_n(Event::Closed, self.closed)
            .chain(core::iter::repeat_n(oversized, self.oversized))
            .chain(self.stream)
    }

    fn oversized(&mut self, len: usize) {
        self.oversized += 1;
        self.oversized_len = self.oversized_len.max(len);
    }

    fn any_closed(&self) -> bool {
        self.closed + self.oversized > 0
    }
}

/// One connection slot.
struct Conn {
    stream: Option<Handle<Stream>>,
    closing: Option<Closing<Stream>>,
    open: bool,
    rx: Option<Pending<RecvOp>>,
    deframe: Deframer,
    state: ConnectionState,
    tx: Tx,
}

/// Two send buffers: the link appends to `filling` while `sending` is out.
struct Tx {
    filling: Buf,
    sending: Slot<SendOp>,
    /// The write timer, and whether it was canceled.
    deadline: Option<(Pending<TimeoutOp>, bool)>,
}

/// A listening link's listener.
enum Listening {
    /// Not listening: before the first listen, or after one failed.
    Down,
    Asked(Pending<ListenOp>),
    Up(Handle<Listener>),
    /// Lost, until its close lands.
    Closing(Closing<Listener>),
}

/// A handle's close, from the handle to the op landing.
enum Closing<K> {
    /// Waiting for room on the queue.
    Waiting(Handle<K>),
    Sent(Pending<CloseOp>),
}

/// The arena buffers of a link's connections, allocated at make: per slot
/// a receive buffer and two send buffers, and the peer address the stream
/// op fills.
pub(crate) struct ConnsMem {
    config: ConnsConfig,
    slots: Vec<[Buf; 3]>,
    peer: Buf,
}

/// A link's connection slots and the endpoint they are opened from.
pub(crate) struct Conns {
    ep: Cap<Endpoint>,
    /// Unused by a dialing link.
    listener: Listening,
    next: Slot<StreamOp>,
    pause: Pause,
    /// A stream with no free slot, until its close lands.
    refused: Option<Closing<Stream>>,
    seed: Box<[u8]>,
    slots: Box<[Conn]>,
    config: ConnsConfig,
    bytes_out: u64,
    batches_dropped: u64,
}

impl ConnsMem {
    /// Allocates every arena buffer the connections use.
    ///
    /// # Errors
    /// Fails if the buffers do not fit the arena.
    pub(crate) fn make(init: &mut IoInit<'_>, config: ConnsConfig) -> Result<Self, MakeError> {
        let (rx, tx) = (config.rx_cap, config.tx_cap);
        let slots = (0..config.slot_count())
            .map(|_| Ok([init.alloc(rx)?, init.alloc(tx)?, init.alloc(tx)?]))
            .collect::<Result<_, MakeError>>()?;
        Ok(Self {
            config,
            slots,
            peer: init.alloc(RawAddr::LEN)?,
        })
    }
}

impl Conns {
    /// Opens connections from `ep` into `mem`'s slots. Each new connection
    /// is sent `seed` first, and a dialer checks its peer's announce against
    /// `tables` tables.
    ///
    /// # Errors
    /// Fails if `seed` does not fit a send buffer, or `ep`'s budget is short of
    /// [`ops_for`] the slots.
    pub(crate) fn new(
        mem: ConnsMem,
        ep: &Cap<Endpoint>,
        seed: &[u8],
        tables: usize,
    ) -> Result<Self, MakeError> {
        let ConnsMem {
            config,
            slots,
            peer,
        } = mem;
        let (needed, budget) = (ops_for(slots.len()), ep.budget());
        if needed > budget {
            return Err(MakeError::IoOps { needed, budget });
        }
        if seed.len() > config.tx_cap {
            let (seed, send_cap) = (seed.len(), config.tx_cap);
            return Err(MakeError::SeedTooLarge { seed, send_cap });
        }
        Ok(Self {
            ep: ep.clone(),
            listener: Listening::Down,
            next: Slot::Idle(peer),
            pause: Pause::new(match config.dialing {
                true => Backoff::doubling(),
                false => Backoff::fixed(),
            }),
            refused: None,
            seed: seed.into(),
            slots: slots.into_iter().map(|b| Conn::new(b, tables)).collect(),
            config,
            bytes_out: 0,
            batches_dropped: 0,
        })
    }

    /// One pass over every slot and the stream op. Each whole body read goes
    /// to `on_body`.
    pub(crate) fn poll(&mut self, mut on_body: impl FnMut(&mut ConnectionState, &[u8])) -> Turn {
        let (ep, after) = (&self.ep, self.config.write_timeout);
        let mut turn = Turn::default();
        for conn in &mut self.slots {
            match conn.poll(ep, after, &mut self.bytes_out, &mut on_body) {
                Some(Event::Oversized { len }) => turn.oversized(len),
                Some(_) => turn.closed += 1,
                None => {}
            }
        }
        if turn.any_closed() && self.config.dialing {
            self.pause.start();
        }
        turn.stream = self.take_stream().or_else(|| self.step_listener());
        step(&mut self.refused);
        self.ask_stream();
        turn
    }

    /// Appends `batch` to every open connection's send buffer and starts
    /// its send. A connection without room counts one dropped batch.
    pub(crate) fn broadcast(&mut self, batch: &[u8]) {
        let (ep, after) = (&self.ep, self.config.write_timeout);
        for conn in self.slots.iter_mut().filter(|c| c.open) {
            if conn.tx.filling.spare() < batch.len() {
                self.batches_dropped += 1;
                continue;
            }
            conn.tx.filling.extend(batch);
            conn.send(ep, after);
        }
    }

    /// The bytes of each connection's receive buffer.
    pub(crate) fn rx_cap(&self) -> usize {
        self.config.rx_cap
    }

    pub(crate) fn stats(&self) -> Stats {
        Stats {
            connections: self.slots.iter().filter(|c| c.open).count() as u32,
            bytes_out: self.bytes_out,
            batches_dropped: self.batches_dropped,
        }
    }

    /// Reaps the stream op: a new stream takes a free slot, or is refused;
    /// a failure waits out the backoff, and one that loses the listener
    /// closes it. One the full queue refused, or a listener's peer the
    /// endpoint turned away, is asked for again.
    fn take_stream(&mut self) -> Option<Event> {
        let stream = match self.next.poll()? {
            Err(IoError::QueueFull) => return None,
            Err(IoError::Refused) if !self.config.dialing => return None,
            Ok(stream) => stream,
            Err(e) => {
                self.pause.start();
                if e.is_lost() {
                    self.lose_listener();
                }
                return Some(Event::Failed(e));
            }
        };
        self.pause.backoff.reset();
        let peer = RawAddr::read_from_bytes(self.next.bytes())
            .ok()
            .and_then(|addr| SocketAddr::try_from(addr).ok());
        let watch = self.config.dialing;
        match self.slots.iter_mut().find(|c| c.is_free()) {
            Some(conn) => {
                conn.open(stream, &self.seed, watch);
                Some(Event::Opened(peer))
            }
            None => {
                self.refused = Some(Closing::Waiting(stream));
                Some(Event::Refused)
            }
        }
    }

    /// Reaps the listen, and moves a lost listener's close along. A failed
    /// listen waits out the backoff.
    fn step_listener(&mut self) -> Option<Event> {
        match core::mem::replace(&mut self.listener, Listening::Down) {
            Listening::Asked(mut pending) => match pending.poll() {
                None => self.listener = Listening::Asked(pending),
                Some(Ok(listener)) => self.listener = Listening::Up(listener),
                Some(Err(IoError::QueueFull)) => {}
                Some(Err(e)) => {
                    self.pause.start();
                    return Some(Event::Failed(e));
                }
            },
            Listening::Closing(closing) => {
                if let Some(closing) = closing.step() {
                    self.listener = Listening::Closing(closing);
                }
            }
            other => self.listener = other,
        }
        None
    }

    fn lose_listener(&mut self) {
        if let Listening::Up(listener) = core::mem::replace(&mut self.listener, Listening::Down) {
            self.listener = Listening::Closing(Closing::Waiting(listener));
        }
    }

    /// Asks for the next stream, unless one is asked for, the backoff is
    /// running, or a dialer has no free slot. A listening link listens
    /// first.
    fn ask_stream(&mut self) {
        if self.pause.waiting(&self.ep) || self.refused.is_some() {
            return;
        }
        if self.config.dialing && !self.slots.iter().any(Conn::is_free) {
            return;
        }
        let Some(peer) = self.next.take_idle() else {
            return;
        };
        self.next = match (&self.listener, self.config.dialing) {
            (_, true) => Slot::Busy(self.ep.connect(peer)),
            (Listening::Up(listener), false) => Slot::Busy(listener.accept(peer)),
            (Listening::Down, false) => {
                self.listener = Listening::Asked(self.ep.listen());
                Slot::Idle(peer)
            }
            _ => Slot::Idle(peer),
        };
    }
}

impl Conn {
    fn new([rx, filling, sending]: [Buf; 3], tables: usize) -> Self {
        Self {
            stream: None,
            closing: None,
            open: false,
            rx: None,
            deframe: Deframer::new(rx),
            state: ConnectionState::new(tables),
            tx: Tx {
                filling,
                sending: Slot::Idle(sending),
                deadline: None,
            },
        }
    }

    /// Whether the slot is closed, every op has landed, and every buffer
    /// is back.
    fn is_free(&self) -> bool {
        !self.open
            && self.stream.is_none()
            && self.closing.is_none()
            && !self.deframe.lent()
            && self.rx.is_none()
            && matches!(self.tx.sending, Slot::Idle(_))
            && self.tx.deadline.is_none()
    }

    /// Starts serving `stream`, with `seed` as its first send.
    fn open(&mut self, stream: Handle<Stream>, seed: &[u8], watch: bool) {
        self.deframe.reset();
        self.state.reset(watch);
        self.tx.filling.fill(seed);
        if let Slot::Idle(buf) = &mut self.tx.sending {
            buf.clear();
        }
        self.stream = Some(stream);
        self.open = true;
    }

    /// One pass over the slot. The event that closed the connection, if it
    /// closed in it.
    fn poll(
        &mut self,
        ep: &Cap<Endpoint>,
        after: Duration,
        bytes_out: &mut u64,
        on_body: &mut impl FnMut(&mut ConnectionState, &[u8]),
    ) -> Option<Event> {
        if !self.open {
            self.drain();
            return None;
        }
        let received = self.recv(on_body);
        let sent = self.tx.reap(bytes_out);
        let stuck = self.tx.stuck(sent == Some(true));
        let closed = match received {
            Err(event) => Some(event),
            Ok(()) => (stuck || sent == Some(false)).then_some(Event::Closed),
        };
        match closed {
            Some(_) => self.close(),
            None => self.send(ep, after),
        }
        closed
    }

    /// Reaps the recv and asks for more. The closing event if the peer hung
    /// up, failed, or sent a body past the buffer.
    fn recv(&mut self, on_body: &mut impl FnMut(&mut ConnectionState, &[u8])) -> Result<(), Event> {
        if let Some(pending) = &mut self.rx {
            let Some((result, tail)) = pending.poll() else {
                return Ok(());
            };
            self.rx = None;
            let back = self.deframe.filled(tail).is_ok();
            match result {
                Ok(1..) if back => {
                    while let Some(body) = self.deframe.next() {
                        let body = body.map_err(|Oversized { len }| Event::Oversized { len })?;
                        on_body(&mut self.state, body);
                    }
                }
                Err(IoError::QueueFull) if back => {}
                _ => return Err(Event::Closed),
            }
        }
        let (Some(stream), Some(tail)) = (&self.stream, self.deframe.spare()) else {
            return Err(Event::Closed);
        };
        self.rx = Some(stream.recv(tail));
        Ok(())
    }

    /// Starts a send of what is waiting, and the write timer over it.
    fn send(&mut self, ep: &Cap<Endpoint>, after: Duration) {
        if let Some(stream) = &self.stream {
            self.tx.start(stream);
        }
        if self.tx.deadline.is_none() && matches!(self.tx.sending, Slot::Busy(_)) {
            self.tx.deadline = Some((ep.timeout(after), false));
        }
    }

    /// Cancels the ops in flight and closes the stream. The slot stays
    /// taken until every op has landed.
    fn close(&mut self) {
        self.open = false;
        self.closing = self.stream.take().map(Closing::Waiting);
        if let Some(rx) = &self.rx {
            rx.cancel();
        }
        if let Slot::Busy(send) = &self.tx.sending {
            send.cancel();
        }
        self.tx.cancel_deadline();
        self.drain();
    }

    /// Takes back what a closed slot's ops hand back, and moves its close
    /// along.
    fn drain(&mut self) {
        step(&mut self.closing);
        if let Some((_, tail)) = self.rx.as_mut().and_then(Pending::poll) {
            self.rx = None;
            let _ = self.deframe.filled(tail);
        }
        let _ = self.tx.sending.poll();
        if self
            .tx
            .deadline
            .as_mut()
            .is_some_and(|(d, _)| d.poll().is_some())
        {
            self.tx.deadline = None;
        }
    }
}

impl Tx {
    /// Reaps a finished send, dropping the bytes it sent. `Some(false)` if
    /// it failed or sent nothing; `None` if none landed, or the full queue
    /// refused it and it is sent again.
    fn reap(&mut self, bytes_out: &mut u64) -> Option<bool> {
        match (self.sending.poll()?, &mut self.sending) {
            (Ok(n @ 1..), Slot::Idle(buf)) => {
                buf.advance(n);
                *bytes_out += n as u64;
                Some(true)
            }
            (Err(IoError::QueueFull), _) => None,
            _ => Some(false),
        }
    }

    /// Whether the write timer landed with no send finished since it was
    /// set. A send that finishes cancels it, so the next one gets a new
    /// timer.
    fn stuck(&mut self, sent: bool) -> bool {
        let Some((deadline, canceled)) = &mut self.deadline else {
            return false;
        };
        let canceled = *canceled;
        let Some(landed) = deadline.poll() else {
            if sent {
                self.cancel_deadline();
            }
            return false;
        };
        self.deadline = None;
        landed.is_ok() && !canceled && !sent && matches!(self.sending, Slot::Busy(_))
    }

    fn cancel_deadline(&mut self) {
        if let Some((deadline, canceled @ false)) = &mut self.deadline {
            deadline.cancel();
            *canceled = true;
        }
    }

    /// Sends the rest of the last send, else everything filled since.
    fn start(&mut self, stream: &Handle<Stream>) {
        let Slot::Idle(buf) = &mut self.sending else {
            return;
        };
        if buf.is_empty() {
            buf.clear();
            core::mem::swap(buf, &mut self.filling);
        }
        if buf.is_empty() {
            return;
        }
        if let Some(buf) = self.sending.take_idle() {
            self.sending = Slot::Busy(stream.send(buf));
        }
    }
}

impl<K> Closing<K> {
    /// Queues the close, or reaps it. `None` once it has landed.
    fn step(self) -> Option<Self> {
        match self {
            Self::Waiting(stream) => {
                let mut op = stream.close();
                match op.poll() {
                    Some(Err(IoError::QueueFull)) => Some(Self::Waiting(stream)),
                    Some(_) => None,
                    None => Some(Self::Sent(op)),
                }
            }
            Self::Sent(mut op) => op.poll().is_none().then_some(Self::Sent(op)),
        }
    }
}

/// Moves `closing` along, emptying it once the close has landed.
fn step<K>(closing: &mut Option<Closing<K>>) {
    *closing = closing.take().and_then(Closing::step);
}

#[cfg(test)]
mod tests;
