//! The connection slots both links share: an outbox per connection and the
//! task serving it.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use futures_lite::future;
use stellarator::io::{AsyncWrite, LengthDelReader, OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::TcpStream;
use stellarator::sync::WaitQueue;

use crate::Stop;

use super::inbox::Inbox;

/// An `Outbox` is the queue between the link and one connection's writer:
/// the link fills it, the writer swaps it out. Both run on one executor, so
/// a `RefCell` and a wake are the whole channel.
struct Outbox {
    pending: RefCell<Vec<u8>>,
    written: Cell<u64>,
    wake: WaitQueue,
    cap: usize,
}

impl Outbox {
    fn new(cap: usize) -> Self {
        Self {
            pending: RefCell::new(Vec::with_capacity(cap)),
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

    /// Whether the writer has something to do.
    fn ready(&self) -> bool {
        !self.pending.borrow().is_empty()
    }
}

/// What a link reports about its sockets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub connections: u32,
    pub bytes_out: u64,
    pub batches_dropped: u64,
}

/// A slot claimed for a connection about to be served.
pub(crate) struct Slot {
    at: usize,
    outbox: Rc<Outbox>,
}

/// What a link's loop and its connection task share: one slot per allowed
/// connection, what every connection is sent first, where every connection's
/// reads go, and the counters.
pub(crate) struct Connections {
    slots: RefCell<Vec<Option<Rc<Outbox>>>>,
    cap: usize,
    seed: Vec<u8>,
    inbox: Rc<Inbox>,
    changed: WaitQueue,
    /// Bytes written by connections that have since closed.
    bytes_out: Cell<u64>,
    batches_dropped: Cell<u64>,
}

impl Connections {
    /// `count` slots, each connection with a `cap`-byte outbox and a `cap`-byte
    /// receive buffer, sent `seed` first and handing every packet it reads to
    /// `inbox`.
    pub(crate) fn new(count: usize, cap: usize, seed: Vec<u8>, inbox: Rc<Inbox>) -> Self {
        Self {
            slots: RefCell::new((0..count).map(|_| None).collect()),
            cap,
            seed,
            inbox,
            changed: WaitQueue::new(),
            bytes_out: Cell::new(0),
            batches_dropped: Cell::new(0),
        }
    }

    /// Waits for a counter to move past `observed`.
    async fn changed(&self, observed: Stats) {
        let _ = self.changed.wait_for(|| self.stats() != observed).await;
    }

    /// Claims a free slot, or `None` when every slot is taken.
    ///
    /// Allocates the connection's outbox; a half ended by `stop` or its peer
    /// may still hold the previous occupant's buffer in an operation.
    pub(crate) fn claim(&self) -> Option<Slot> {
        let mut slots = self.slots.borrow_mut();
        let at = slots.iter().position(Option::is_none)?;
        let outbox = Rc::new(Outbox::new(self.cap));
        slots[at] = Some(outbox.clone());
        Some(Slot { at, outbox })
    }

    /// Frees a slot whose connection ended, keeping its byte count.
    fn free(&self, slot: Slot) {
        self.slots.borrow_mut()[slot.at] = None;
        self.bytes_out
            .set(self.bytes_out.get() + slot.outbox.written.get());
        self.changed.wake_all();
    }

    /// Serves `stream` from `slot` until either half ends or `stop`, then frees
    /// the slot. Each connection's announce is checked on its own.
    pub(crate) async fn serve(&self, slot: Slot, stream: TcpStream, stop: &Stop) {
        self.inbox.connected();
        let (rx, tx) = stream.split();
        let halves = future::or(
            write_half(&slot.outbox, &self.changed, tx, self.seed.clone()),
            read_half(rx, vec![0u8; self.cap], &self.inbox),
        );
        future::or(halves, stop.wait()).await;
        self.free(slot);
    }

    /// Queues one batch for every open connection, counting the ones it does
    /// not fit.
    pub(crate) fn enqueue(&self, batch: &[u8]) {
        for outbox in self.slots.borrow().iter().flatten() {
            if !outbox.enqueue(batch) {
                self.batches_dropped.set(self.batches_dropped.get() + 1);
            }
        }
    }

    pub(crate) fn stats(&self) -> Stats {
        let slots = self.slots.borrow();
        let open = slots.iter().flatten();
        Stats {
            connections: open.clone().count() as u32,
            bytes_out: self.bytes_out.get() + open.map(|o| o.written.get()).sum::<u64>(),
            batches_dropped: self.batches_dropped.get(),
        }
    }
}

pub(crate) enum Event {
    /// The link's own input has something, raised by the link's loop.
    Ready,
    Changed,
    Stop,
}

/// Waits for a counter change or shutdown.
pub(crate) async fn next(conns: &Connections, stop: &Stop) -> Event {
    let observed = conns.stats();
    let changed = async {
        conns.changed(observed).await;
        Event::Changed
    };
    let stopped = async {
        stop.wait().await;
        Event::Stop
    };
    future::or(changed, stopped).await
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
        if outbox.wake.wait_for(|| outbox.ready()).await.is_err() {
            return;
        }
        outbox.swap(&mut buf);
    }
}

/// Reads packet bodies into `inbox` until the peer errors or hangs up,
/// reusing one buffer.
///
/// A packet past the buffer is a read error, so it ends the connection.
async fn read_half(rx: OwnedReader<TcpStream>, buf: Vec<u8>, inbox: &Inbox) {
    let mut reader = LengthDelReader::<_, u32>::new(rx);
    let mut buf = buf;
    loop {
        let Ok(body) = reader.recv(buf).await else {
            return;
        };
        inbox.accept(&body);
        buf = body.into_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conns(count: usize, cap: usize, seed: &[u8]) -> Rc<Connections> {
        let inbox = Rc::new(Inbox::blackhole());
        Rc::new(Connections::new(count, cap, seed.to_vec(), inbox))
    }

    #[test]
    fn test_queue_recovers_after_oversized_batch() {
        let outbox = Outbox::new(8);
        assert!(outbox.enqueue(b"12345"));
        assert!(!outbox.enqueue(b"6789"));
        assert!(outbox.enqueue(b"678"));
        let mut buf = Vec::new();
        outbox.swap(&mut buf);
        assert_eq!(buf, b"12345678");
        let pending = outbox.pending.borrow();
        assert!(pending.is_empty() && pending.capacity() >= 8);
    }

    #[test]
    fn test_empty_link_skips_queueing() {
        let conns = conns(2, 16, b"");
        conns.enqueue(b"batch");
        assert_eq!(conns.stats(), Stats::default());
    }

    #[test]
    fn test_ignore_unchanged_notification() {
        let conns = conns(1, 64, b"");
        conns.changed.wake_all();
        let changed = future::poll_once(conns.changed(conns.stats()));
        assert!(future::block_on(changed).is_none());
    }

    #[test]
    fn test_claim_fills_slots_in_order() {
        let conns = conns(2, 16, b"");
        let first = conns.claim().expect("a free slot");
        let second = conns.claim().expect("another");
        assert!(conns.claim().is_none());
        assert_eq!((first.at, second.at), (0, 1));
        conns.free(first);
        assert_eq!(conns.claim().expect("the freed slot").at, 0);
    }
}
