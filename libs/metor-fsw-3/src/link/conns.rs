//! A link's connections over one `Endpoint`: each slot's buffers and the
//! ops that move them, polled once per turn.

use core::net::SocketAddr;
use core::time::Duration;

use zerocopy::FromBytes;

use crate::coordinator::MakeError;
use crate::io::{
    Buf, CloseOp, Endpoint, Handle, IoError, IoInit, Pending, RawAddr, RecvOp, SendOp, Slot,
    Stream, StreamOp, TimeoutOp,
};

use super::deframe::Deframer;
use super::peer::ConnectionState;

/// The wait after a failed or lost stream. Each failure in a row doubles a
/// dialer's, up to [`BACKOFF_MAX`].
const BACKOFF_INITIAL: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(10);

/// The ops `slots` connections have in flight at most: the stream op, a
/// backoff timer, a refused stream's close, and per slot a recv, a send, a
/// write timer, and a close.
pub(crate) const fn ops_for(slots: usize) -> usize {
    3 + 4 * slots
}

/// How a link's connections are sized and run.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ConnsConfig {
    /// The most connections open at once. A dialing link has one.
    pub(crate) slots: usize,
    /// The bytes of each connection's receive buffer and of each of its two
    /// send buffers.
    pub(crate) conn_cap: usize,
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
    /// A stream arrived with every slot taken, and was closed.
    Refused,
    /// The endpoint failed to open a stream.
    Failed(IoError),
}

/// What one [`Conns::poll`] pass did: the connections it closed, and what
/// became of the stream it reaped.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Turn {
    closed: usize,
    stream: Option<Event>,
}

impl Turn {
    pub(crate) fn events(self) -> impl Iterator<Item = Event> {
        core::iter::repeat_n(Event::Closed, self.closed).chain(self.stream)
    }
}

/// One connection slot.
struct Conn {
    stream: Option<Handle<Stream>>,
    closing: Option<Closing>,
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

/// The wait before the next stream.
struct Backoff {
    wait: Option<Pending<TimeoutOp>>,
    /// A wait is due but not yet on the queue.
    owed: bool,
    delay: Duration,
    doubles: bool,
}

/// A stream's close, from its handle to the op landing.
enum Closing {
    /// Waiting for room on the queue.
    Waiting(Handle<Stream>),
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
    ep: Handle<Endpoint>,
    next: Slot<StreamOp>,
    backoff: Backoff,
    /// A stream with no free slot, until its close lands.
    refused: Option<Closing>,
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
        let cap = config.conn_cap;
        let slots = (0..config.slot_count())
            .map(|_| Ok([init.alloc(cap)?, init.alloc(cap)?, init.alloc(cap)?]))
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
    /// Fails if `seed` does not fit `conn_cap`, or `ep`'s budget is short of
    /// [`ops_for`] the slots.
    pub(crate) fn new(
        mem: ConnsMem,
        ep: &Handle<Endpoint>,
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
        if seed.len() > config.conn_cap {
            let (seed, conn_cap) = (seed.len(), config.conn_cap);
            return Err(MakeError::SeedTooLarge { seed, conn_cap });
        }
        Ok(Self {
            ep: ep.clone(),
            next: Slot::Idle(peer),
            backoff: Backoff::new(config.dialing),
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
        let mut closed = 0;
        for conn in &mut self.slots {
            closed += usize::from(conn.poll(ep, after, &mut self.bytes_out, &mut on_body));
        }
        if closed > 0 && self.config.dialing {
            self.backoff.start();
        }
        let stream = self.take_stream();
        step(&mut self.refused);
        self.ask_stream();
        Turn { closed, stream }
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

    pub(crate) fn stats(&self) -> Stats {
        Stats {
            connections: self.slots.iter().filter(|c| c.open).count() as u32,
            bytes_out: self.bytes_out,
            batches_dropped: self.batches_dropped,
        }
    }

    /// Reaps the stream op: a new stream takes a free slot, or is refused;
    /// a failure waits out the backoff. One the full queue refused, or a
    /// listener's peer the endpoint turned away, is asked for again.
    fn take_stream(&mut self) -> Option<Event> {
        let stream = match self.next.poll()? {
            Err(IoError::QueueFull) => return None,
            Err(IoError::Refused) if !self.config.dialing => return None,
            Ok(stream) => stream,
            Err(e) => {
                self.backoff.start();
                return Some(Event::Failed(e));
            }
        };
        self.backoff.delay = BACKOFF_INITIAL;
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

    /// Asks the endpoint for the next stream, unless one is asked for, the
    /// backoff is running, or a dialer has no free slot.
    fn ask_stream(&mut self) {
        if self.backoff.waiting(&self.ep) || self.refused.is_some() {
            return;
        }
        if self.config.dialing && !self.slots.iter().any(Conn::is_free) {
            return;
        }
        if let Some(peer) = self.next.take_idle() {
            self.next = Slot::Busy(self.ep.stream(peer));
        }
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

    /// One pass over the slot. True if the connection closed in it.
    fn poll(
        &mut self,
        ep: &Handle<Endpoint>,
        after: Duration,
        bytes_out: &mut u64,
        on_body: &mut impl FnMut(&mut ConnectionState, &[u8]),
    ) -> bool {
        if !self.open {
            self.drain();
            return false;
        }
        let hung_up = self.recv(on_body);
        let sent = self.tx.reap(bytes_out);
        let stuck = self.tx.stuck(sent == Some(true));
        if hung_up || stuck || sent == Some(false) {
            self.close();
            return true;
        }
        self.send(ep, after);
        false
    }

    /// Reaps the recv and asks for more. True if the peer hung up, failed,
    /// or sent a body past the buffer.
    fn recv(&mut self, on_body: &mut impl FnMut(&mut ConnectionState, &[u8])) -> bool {
        if let Some(pending) = &mut self.rx {
            let Some((result, tail)) = pending.poll() else {
                return false;
            };
            self.rx = None;
            let back = self.deframe.filled(tail).is_ok();
            match result {
                Ok(1..) if back => {
                    while let Some(body) = self.deframe.next() {
                        let Ok(body) = body else {
                            return true;
                        };
                        on_body(&mut self.state, body);
                    }
                }
                Err(IoError::QueueFull) if back => {}
                _ => return true,
            }
        }
        let (Some(stream), Some(tail)) = (&self.stream, self.deframe.spare()) else {
            return true;
        };
        self.rx = Some(stream.recv(tail));
        false
    }

    /// Starts a send of what is waiting, and the write timer over it.
    fn send(&mut self, ep: &Handle<Endpoint>, after: Duration) {
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

impl Backoff {
    fn new(doubles: bool) -> Self {
        Self {
            wait: None,
            owed: false,
            delay: BACKOFF_INITIAL,
            doubles,
        }
    }

    /// Makes a wait due before the next stream, unless one is running.
    fn start(&mut self) {
        self.owed |= self.wait.is_none();
    }

    /// Whether a wait is due or running. Puts a due wait on the queue, and
    /// doubles the next one if it doubles.
    fn waiting(&mut self, ep: &Handle<Endpoint>) -> bool {
        if self.wait.as_mut().is_some_and(|w| w.poll().is_some()) {
            self.wait = None;
        }
        if self.owed {
            let mut wait = ep.timeout(self.delay);
            match wait.poll() {
                Some(Err(IoError::QueueFull)) => return true,
                Some(_) => {}
                None => self.wait = Some(wait),
            }
            self.owed = false;
            if self.doubles {
                self.delay = (self.delay * 2).min(BACKOFF_MAX);
            }
        }
        self.wait.is_some()
    }
}

impl Closing {
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
fn step(closing: &mut Option<Closing>) {
    *closing = closing.take().and_then(Closing::step);
}

#[cfg(test)]
mod tests;
