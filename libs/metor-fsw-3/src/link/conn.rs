//! The connection slots both links share: pending buffers and socket halves.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use metor_proto::types::OwnedPacket;
use metor_proto_stellar::PacketStream;
use stellarator::JoinHandleDropGuard;
use stellarator::buf::Slice;
use stellarator::io::{AsyncWrite, OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::TcpStream;
use stellarator::sync::WaitQueue;

/// A connection's first receive buffer; `next_grow` grows it to the largest
/// packet the peer sends.
const RECV_BUF: usize = 1024;

/// One inbound packet, borrowed by a connection's read half.
pub(crate) type Packet = OwnedPacket<Slice<Vec<u8>>>;

/// One connection's outbound bytes, not yet written.
struct Pending {
    bytes: Vec<u8>,
    closed: bool,
}

/// A `Conn` is one connection's queue, shared between the link and its halves.
pub(crate) struct Conn {
    pending: RefCell<Pending>,
    wake: WaitQueue,
    cap: usize,
    written: Rc<Cell<u64>>,
}

impl Conn {
    fn new(cap: usize, written: Rc<Cell<u64>>) -> Self {
        Self {
            pending: RefCell::new(Pending {
                bytes: Vec::with_capacity(cap),
                closed: false,
            }),
            wake: WaitQueue::new(),
            cap,
            written,
        }
    }

    /// Queues a whole batch, or none of it.
    fn enqueue(&self, batch: &[u8]) -> bool {
        {
            let mut pending = self.pending.borrow_mut();
            if pending.bytes.len() + batch.len() > self.cap {
                return false;
            }
            pending.bytes.extend_from_slice(batch);
        }
        self.wake.wake_all();
        true
    }

    /// Hands the queued bytes to the writer, keeping the queue's capacity.
    fn take(&self, buf: &mut Vec<u8>) {
        let mut pending = self.pending.borrow_mut();
        core::mem::swap(&mut pending.bytes, buf);
        pending.bytes.clear();
        pending.bytes.reserve_exact(self.cap);
    }

    fn close(&self) {
        self.pending.borrow_mut().closed = true;
        self.wake.wake_all();
    }

    fn is_closed(&self) -> bool {
        self.pending.borrow().closed
    }

    /// Whether the writer has something to do.
    fn ready(&self) -> bool {
        let pending = self.pending.borrow();
        pending.closed || !pending.bytes.is_empty()
    }
}

/// What a link reports about its sockets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub connections: u32,
    pub bytes_out: u64,
    pub batches_dropped: u64,
}

/// One slot: its queue, and the task serving it while it is open.
struct Slot {
    conn: Rc<Conn>,
    task: Option<JoinHandleDropGuard<()>>,
}

/// The connections one link serves, in slots allocated at construction.
pub(crate) struct Connections {
    slots: Vec<Slot>,
    cap: usize,
    written: Rc<Cell<u64>>,
    batches_dropped: u64,
}

impl Connections {
    /// `count` slots, each with a `cap`-byte outbound buffer.
    pub(crate) fn new(count: usize, cap: usize) -> Self {
        let written = Rc::new(Cell::new(0));
        let slots = (0..count)
            .map(|_| Slot {
                conn: Rc::new(Conn::new(cap, written.clone())),
                task: None,
            })
            .collect();
        Self {
            slots,
            cap,
            written,
            batches_dropped: 0,
        }
    }

    /// Whether another connection would find a slot.
    pub(crate) fn has_free(&self) -> bool {
        self.slots.iter().any(|slot| slot.task.is_none())
    }

    /// Frees the slots whose connection ended.
    pub(crate) fn prune(&mut self) {
        for slot in self.slots.iter_mut() {
            if slot.task.is_some() && slot.conn.is_closed() {
                slot.task = None;
            }
        }
    }

    /// Serves `stream` from a free slot, writing `seed` before anything else
    /// and handing every inbound packet to `on_packet`. `false` when full.
    pub(crate) fn open(
        &mut self,
        stream: TcpStream,
        seed: Vec<u8>,
        on_packet: impl FnMut(&Packet) + 'static,
    ) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.task.is_none()) else {
            return false;
        };
        // A slot the last connection's cancelled task still shares needs its
        // own queue; a recycled one keeps its buffer.
        if Rc::strong_count(&slot.conn) > 1 {
            slot.conn = Rc::new(Conn::new(self.cap, self.written.clone()));
        } else {
            let mut pending = slot.conn.pending.borrow_mut();
            pending.bytes.clear();
            pending.closed = false;
        }
        let (rx, tx) = stream.split();
        let conn = slot.conn.clone();
        slot.task = Some(stellarator::spawn(serve(conn, rx, tx, seed, on_packet)).drop_guard());
        true
    }

    /// Queues one batch for every open connection, counting the ones it does
    /// not fit.
    pub(crate) fn enqueue(&mut self, batch: &[u8]) {
        for slot in self.slots.iter() {
            if slot.task.is_some() && !slot.conn.enqueue(batch) {
                self.batches_dropped += 1;
            }
        }
    }

    pub(crate) fn stats(&self) -> Stats {
        Stats {
            connections: self.slots.iter().filter(|s| s.task.is_some()).count() as u32,
            bytes_out: self.written.get(),
            batches_dropped: self.batches_dropped,
        }
    }
}

/// One connection's life: the halves race, and either one ending closes it.
async fn serve(
    conn: Rc<Conn>,
    rx: OwnedReader<TcpStream>,
    tx: OwnedWriter<TcpStream>,
    seed: Vec<u8>,
    on_packet: impl FnMut(&Packet),
) {
    futures_lite::future::or(write_half(&conn, tx, seed), read_half(rx, on_packet)).await;
    conn.close();
}

/// Writes the seed, then every batch the link queues, one buffer at a time.
async fn write_half(conn: &Conn, tx: OwnedWriter<TcpStream>, seed: Vec<u8>) {
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
            conn.written.set(conn.written.get() + len);
        }
        if conn.wake.wait_for(|| conn.ready()).await.is_err() || conn.is_closed() {
            return;
        }
        conn.take(&mut buf);
    }
}

/// Reads packets until the peer errors or hangs up, reusing one buffer.
async fn read_half(rx: OwnedReader<TcpStream>, mut on_packet: impl FnMut(&Packet)) {
    let mut stream = PacketStream::new(rx);
    let mut buf = vec![0u8; RECV_BUF];
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

    fn conn(cap: usize) -> Conn {
        Conn::new(cap, Rc::new(Cell::new(0)))
    }

    #[test]
    fn a_batch_past_the_cap_drops_whole_and_a_smaller_one_still_lands() {
        let conn = conn(8);
        assert!(conn.enqueue(b"12345"));
        assert!(!conn.enqueue(b"6789"));
        assert!(conn.enqueue(b"678"));
        let mut buf = Vec::new();
        conn.take(&mut buf);
        assert_eq!(buf, b"12345678");
    }

    #[test]
    fn a_taken_queue_keeps_its_capacity() {
        let conn = conn(64);
        conn.enqueue(b"one");
        let mut buf = Vec::new();
        conn.take(&mut buf);
        assert_eq!(buf, b"one");
        assert!(conn.pending.borrow().bytes.capacity() >= 64);
        assert!(conn.pending.borrow().bytes.is_empty());
    }

    #[test]
    fn a_closed_connection_is_ready_so_its_writer_returns() {
        let conn = conn(8);
        assert!(!conn.ready());
        conn.close();
        assert!(conn.ready() && conn.is_closed());
    }

    #[test]
    fn a_link_with_no_connections_queues_nothing_and_counts_nothing() {
        let mut conns = Connections::new(2, 16);
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
        let mut conns = Connections::new(1, 64);
        for expected in [true, false] {
            let client = TcpStream::connect(addr).await.expect("the listener is up");
            let served = listener.accept().await.expect("a connection");
            assert_eq!(conns.open(served, Vec::new(), |_| {}), expected);
            drop(client);
        }
        assert_eq!(conns.stats().connections, 1);
        assert!(!conns.has_free());
    }
}
