//! The connection slots both links share: an outbox per connection and the
//! task serving it.

use core::cell::{Cell, RefCell};
use core::future::Future;
use std::rc::Rc;

use core::mem::MaybeUninit;
use core::ptr::NonNull;

use metor_proto::types::OwnedPacket;
use metor_proto_stellar::PacketStream;
use stellarator::JoinHandleDropGuard;
use stellarator::buf::{IoBuf, IoBufMut, Slice};
use stellarator::io::{AsyncWrite, GrowableBuf, OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::TcpStream;
use stellarator::sync::WaitQueue;

use crate::Stop;

use super::transport::Incoming;

/// A connection's first receive buffer, and the smallest cap a link may set.
pub(crate) const RECV_BUF: usize = 1024;

/// One inbound packet, borrowed by a connection's read half.
pub(crate) type Packet = OwnedPacket<Slice<Capped>>;

/// A receive buffer that never grows past its cap, so the length prefix a peer
/// sends cannot pick the size of a link's allocation.
///
/// Refusing to grow empties the buffer, which fails the read that follows and
/// closes the connection.
pub(crate) struct Capped {
    buf: Vec<u8>,
    cap: usize,
}

impl Capped {
    /// A buffer of [`RECV_BUF`] bytes, growable to `cap` and no further.
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            buf: vec![0u8; RECV_BUF],
            cap: cap.max(RECV_BUF),
        }
    }
}

#[cfg(test)]
impl Capped {
    /// A buffer already holding `bytes`, for a test framing its own packet.
    pub(crate) fn filled(bytes: Vec<u8>) -> Self {
        let cap = bytes.len();
        Self { buf: bytes, cap }
    }
}

impl GrowableBuf for Capped {
    fn grow(&mut self, new_len: usize) {
        match new_len > self.cap {
            true => self.buf.clear(),
            false if new_len > self.buf.len() => self.buf.resize(new_len, 0),
            false => {}
        }
    }
}

// SAFETY: every method delegates to the `Vec` this buffer owns.
unsafe impl IoBuf for Capped {
    fn stable_init_ptr(&self) -> *const u8 {
        self.buf.stable_init_ptr()
    }

    fn init_len(&self) -> usize {
        self.buf.init_len()
    }

    fn total_len(&self) -> usize {
        self.buf.total_len()
    }
}

// SAFETY: as above.
unsafe impl IoBufMut for Capped {
    fn stable_mut_ptr(&mut self) -> NonNull<MaybeUninit<u8>> {
        self.buf.stable_mut_ptr()
    }

    unsafe fn set_init(&mut self, len: usize) {
        // SAFETY: the caller promises `len` bytes of the `Vec` are initialized.
        unsafe { self.buf.set_init(len) }
    }
}

/// An `Outbox` is the queue between the link and one connection's writer:
/// the link fills it, the writer swaps it out. Both run on one executor, so
/// a `RefCell` and a wake are the whole channel.
struct Outbox {
    pending: RefCell<Vec<u8>>,
    closed: Cell<bool>,
    written: Cell<u64>,
    wake: WaitQueue,
    cap: usize,
}

impl Outbox {
    fn new(cap: usize) -> Self {
        Self {
            pending: RefCell::new(Vec::with_capacity(cap)),
            closed: Cell::new(false),
            written: Cell::new(0),
            wake: WaitQueue::new(),
            cap,
        }
    }

    /// Queues a whole batch, or none of it.
    fn enqueue(&self, batch: &[u8]) -> bool {
        {
            let mut pending = self.pending.borrow_mut();
            if pending.len() + batch.len() > self.cap {
                return false;
            }
            pending.extend_from_slice(batch);
        }
        self.wake.wake_all();
        true
    }

    /// Hands the queued bytes to the writer, keeping the queue's capacity.
    fn swap(&self, buf: &mut Vec<u8>) {
        let mut pending = self.pending.borrow_mut();
        core::mem::swap(&mut *pending, buf);
        pending.clear();
        pending.reserve_exact(self.cap);
    }

    fn close(&self) {
        self.closed.set(true);
        self.wake.wake_all();
    }

    /// Whether the writer has something to do.
    fn ready(&self) -> bool {
        self.closed.get() || !self.pending.borrow().is_empty()
    }
}

/// What a link reports about its sockets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub connections: u32,
    pub bytes_out: u64,
    pub batches_dropped: u64,
}

/// One open connection as the link sees it: the send side of its outbox and
/// the task that owns everything else.
struct Open {
    outbox: Rc<Outbox>,
    _task: JoinHandleDropGuard<()>,
}

/// The connections one link serves, one slot each.
pub(crate) struct Connections {
    slots: Vec<Option<Open>>,
    cap: usize,
    recv_cap: usize,
    changed: Rc<WaitQueue>,
    /// Bytes written by connections that have since closed.
    bytes_out: u64,
    batches_dropped: u64,
}

impl Connections {
    /// `count` slots, each connection with a `cap`-byte outbox and a receive
    /// buffer of at most `recv_cap` bytes.
    pub(crate) fn new(count: usize, cap: usize, recv_cap: usize) -> Self {
        Self {
            slots: (0..count).map(|_| None).collect(),
            cap,
            recv_cap,
            changed: Rc::new(WaitQueue::new()),
            bytes_out: 0,
            batches_dropped: 0,
        }
    }

    /// Whether another connection would find a slot.
    pub(crate) fn has_free(&self) -> bool {
        self.slots.iter().any(Option::is_none)
    }

    /// Waits for a completed write or a connection that needs pruning.
    async fn changed(&self, observed: Stats) {
        let _ = self
            .changed
            .wait_for(|| self.any_closed() || self.stats() != observed)
            .await;
    }

    fn any_closed(&self) -> bool {
        self.slots
            .iter()
            .flatten()
            .any(|open| open.outbox.closed.get())
    }

    /// Frees the slots whose connection ended, keeping their byte counts.
    pub(crate) fn prune(&mut self) {
        for slot in self.slots.iter_mut() {
            if slot.as_ref().is_some_and(|open| open.outbox.closed.get()) {
                // PANIC Safety: the guard checked the slot is occupied.
                let open = slot.take().expect("an open slot");
                self.bytes_out += open.outbox.written.get();
            }
        }
    }

    /// Serves `stream` from a free slot, writing `seed` before anything else
    /// and handing every inbound packet to `on_packet`. `false` when full.
    ///
    /// Allocates the connection's buffers; a cancelled task may still hold
    /// the previous occupant's, so they are never reused.
    pub(crate) fn open(
        &mut self,
        stream: TcpStream,
        seed: Vec<u8>,
        on_packet: impl FnMut(&Packet) + 'static,
    ) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) else {
            return false;
        };
        let outbox = Rc::new(Outbox::new(self.cap));
        let (rx, tx) = stream.split();
        let serving = serve(
            outbox.clone(),
            self.changed.clone(),
            rx,
            tx,
            seed,
            Capped::new(self.recv_cap),
            on_packet,
        );
        let task = stellarator::spawn(serving).drop_guard();
        *slot = Some(Open {
            outbox,
            _task: task,
        });
        true
    }

    /// Queues one batch for every open connection, counting the ones it does
    /// not fit.
    pub(crate) fn enqueue(&mut self, batch: &[u8]) {
        for open in self.slots.iter().flatten() {
            if !open.outbox.enqueue(batch) {
                self.batches_dropped += 1;
            }
        }
    }

    pub(crate) fn stats(&self) -> Stats {
        let open = self.slots.iter().flatten();
        Stats {
            connections: open.clone().count() as u32,
            bytes_out: self.bytes_out + open.map(|o| o.outbox.written.get()).sum::<u64>(),
            batches_dropped: self.batches_dropped,
        }
    }
}

pub(crate) enum Event {
    Connected(TcpStream),
    Ready,
    Changed,
    Stop,
}

/// Waits for input, connection activity, or shutdown.
pub(crate) async fn next(
    incoming: &Incoming,
    conns: &Connections,
    ready: impl Future<Output = ()>,
    stop: &Stop,
) -> Event {
    let observed = conns.stats();
    let connected = async { Event::Connected(incoming.next().await) };
    let ready = async {
        ready.await;
        Event::Ready
    };
    let changed = async {
        conns.changed(observed).await;
        Event::Changed
    };
    let stopped = async {
        stop.wait().await;
        Event::Stop
    };
    let first = futures_lite::future::or(connected, ready);
    futures_lite::future::or(futures_lite::future::or(first, changed), stopped).await
}

/// One connection's life: the halves race, and either one ending closes it.
async fn serve(
    outbox: Rc<Outbox>,
    changed: Rc<WaitQueue>,
    rx: OwnedReader<TcpStream>,
    tx: OwnedWriter<TcpStream>,
    seed: Vec<u8>,
    buf: Capped,
    on_packet: impl FnMut(&Packet),
) {
    futures_lite::future::or(
        write_half(&outbox, &changed, tx, seed),
        read_half(rx, buf, on_packet),
    )
    .await;
    outbox.close();
    changed.wake_all();
}

/// Writes the seed, then every batch the link queues, one buffer at a time.
async fn write_half(
    outbox: &Outbox,
    changed: &WaitQueue,
    tx: OwnedWriter<TcpStream>,
    seed: Vec<u8>,
) {
    let mut buf = seed;
    loop {
        if !buf.is_empty() {
            let len = buf.len() as u64;
            let (result, mut written) = tx.write_all(buf).await;
            written.clear();
            buf = written;
            if result.is_err() {
                return;
            }
            outbox.written.set(outbox.written.get() + len);
            changed.wake_all();
        }
        if outbox.wake.wait_for(|| outbox.ready()).await.is_err() || outbox.closed.get() {
            return;
        }
        outbox.swap(&mut buf);
    }
}

/// Reads packets until the peer errors or hangs up, reusing one buffer.
///
/// A packet past the buffer's cap is a read error, so it ends the connection.
async fn read_half(rx: OwnedReader<TcpStream>, buf: Capped, mut on_packet: impl FnMut(&Packet)) {
    let mut stream = PacketStream::new(rx);
    let mut buf = buf;
    loop {
        let Ok(packet) = stream.next_grow(buf).await else {
            return;
        };
        on_packet(&packet);
        buf = packet.into_buf().into_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_batch_past_the_cap_drops_whole_and_a_smaller_one_still_lands() {
        let outbox = Outbox::new(8);
        assert!(outbox.enqueue(b"12345"));
        assert!(!outbox.enqueue(b"6789"));
        assert!(outbox.enqueue(b"678"));
        let mut buf = Vec::new();
        outbox.swap(&mut buf);
        assert_eq!(buf, b"12345678");
    }

    #[test]
    fn a_taken_queue_keeps_its_capacity() {
        let outbox = Outbox::new(64);
        outbox.enqueue(b"one");
        let mut buf = Vec::new();
        outbox.swap(&mut buf);
        assert_eq!(buf, b"one");
        assert!(outbox.pending.borrow().capacity() >= 64);
        assert!(outbox.pending.borrow().is_empty());
    }

    #[test]
    fn a_closed_connection_is_ready_so_its_writer_returns() {
        let outbox = Outbox::new(8);
        assert!(!outbox.ready());
        outbox.close();
        assert!(outbox.ready() && outbox.closed.get());
    }

    #[test]
    fn a_link_with_no_connections_queues_nothing_and_counts_nothing() {
        let mut conns = Connections::new(2, 16, RECV_BUF);
        assert!(conns.has_free());
        conns.enqueue(b"batch");
        assert_eq!(
            conns.stats(),
            Stats {
                connections: 0,
                bytes_out: 0,
                batches_dropped: 0
            }
        );
    }

    #[stellarator::test]
    async fn a_full_slot_list_refuses_the_next_connection() {
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = listener.local_addr().expect("bound");
        let mut conns = Connections::new(1, 64, RECV_BUF);
        for expected in [true, false] {
            let client = TcpStream::connect(addr).await.expect("the listener is up");
            let served = listener.accept().await.expect("a connection");
            assert_eq!(conns.open(served, Vec::new(), |_| {}), expected);
            drop(client);
        }
        assert_eq!(conns.stats().connections, 1);
        assert!(!conns.has_free());
    }

    #[test]
    fn a_notification_without_a_counter_change_keeps_waiting() {
        let conns = Connections::new(1, 64, RECV_BUF);
        conns.changed.wake_all();
        let changed = futures_lite::future::poll_once(conns.changed(conns.stats()));
        assert!(futures_lite::future::block_on(changed).is_none());
    }

    #[stellarator::test]
    async fn a_closed_connection_frees_its_slot_and_keeps_its_bytes() {
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = listener.local_addr().expect("bound");
        let mut conns = Connections::new(1, 64, RECV_BUF);
        let client = TcpStream::connect(addr).await.expect("the listener is up");
        let served = listener.accept().await.expect("a connection");
        assert!(conns.open(served, b"seed".to_vec(), |_| {}));
        let (read, buf) = client.read(vec![0u8; 4]).await;
        assert_eq!(read.expect("the seed arrives"), 4);
        assert_eq!(buf, b"seed");
        drop(client);
        while conns.stats().connections == 1 {
            conns.prune();
            stellarator::yield_now().await;
        }
        assert!(conns.has_free());
        assert_eq!(conns.stats().bytes_out, 4);
    }
}
