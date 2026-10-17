//! Connection handling shared by `Publish` and `Subscribe`.
//!
//! A link serves up to a fixed number of connections at once. Every batch the
//! link queues is sent to each open connection, and every packet a connection
//! reads goes to the link's [`Inbox`].

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use futures_lite::future;
use metor_fsw_3_ring::{
    Config, InvalidConfig, NoWake, Notifier, OwnedGrant, RingBuffer, View, WakeSink, Writer,
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Deserialize;
use stellarator::buf::IoBuf;
use stellarator::io::{AsyncWrite, LengthDelReader, OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::TcpStream;
use stellarator::sync::WaitQueue;

use crate::Stop;
use crate::port::ring_capacity;

use super::inbox::Inbox;

/// The link's end of the queue to one connection's writer: a ring that holds
/// one record per batch. It is not `Send`.
struct Outbox {
    writer: Writer<Notifier>,
    written: Cell<u64>,
}

/// The connection writer's end of its outbox ring.
struct OutboxReader {
    view: View<NoWake>,
    wake: Notifier,
}

impl Outbox {
    /// Creates an outbox ring with `config`, and the writer's end of it.
    fn new(config: Config) -> Result<(Self, OutboxReader), InvalidConfig> {
        let ring = RingBuffer::create_in_memory(config)?;
        let wake = Notifier::default();
        // PANIC Safety: the ring is new, so its writer and its one reader slot
        // are both free.
        let writer = ring.writer(wake.clone()).expect("a new ring's writer");
        let view = ring.view(NoWake).expect("a new ring's reader slot");
        let outbox = Self {
            writer,
            written: Cell::new(0),
        };
        Ok((outbox, OutboxReader { view, wake }))
    }

    /// Queues `batch` as one record. Returns false if it does not fit.
    fn enqueue(&self, batch: &[u8]) -> bool {
        self.writer.try_write(batch).is_ok()
    }

    /// Counts `len` more bytes written to the socket.
    fn sent(&self, len: usize) {
        self.written.set(self.written.get() + len as u64);
    }
}

/// A record held in its outbox ring for an in-flight socket write.
///
/// The write reads straight from the ring. The grant keeps the record
/// pinned, so the ring's writer cannot reuse those bytes until this drops.
struct GrantBuf(OwnedGrant<NoWake>);

// A dropped op's buffer is kept inline in stellarator's reactor, which has
// room for seven words, until its completion arrives. A larger buffer would be
// dropped while the kernel still reads it.
const _: () = assert!(size_of::<GrantBuf>() <= 7 * size_of::<usize>());

// SAFETY: The pointer is into the ring's heap region. The grant holds the
// ring, so the region outlives the buffer, and it pins the record, so the
// bytes do not change while the buffer lives. Stellarator owns the buffer for
// the whole op and, when the op's future is dropped or cancelled first, keeps
// it until the op's completion arrives (the assert above keeps it small enough
// for that), so the grant outlives every write that reads it. `init_len` is the
// record's unsent payload, all initialized.
unsafe impl IoBuf for GrantBuf {
    fn stable_init_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }

    fn init_len(&self) -> usize {
        self.0.len()
    }

    fn total_len(&self) -> usize {
        self.0.len()
    }
}

/// Socket counters for a link.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stats {
    pub connections: u32,
    pub bytes_out: u64,
    pub batches_dropped: u64,
}

/// A claimed slot, reserved for a connection that is about to be served.
struct ClaimedConnection {
    index: usize,
    outbox: Rc<Outbox>,
    reader: OutboxReader,
}

/// The size in bytes of each connection's pending write buffer, and the
/// outbox ring that holds two batches of that size.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(try_from = "usize")]
pub struct ConnCap {
    bytes: usize,
    ring: Config,
}

impl ConnCap {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Default for ConnCap {
    fn default() -> Self {
        // PANIC Safety: a 1 MiB batch forms a ring on every supported host.
        Self::try_from(1 << 20).expect("the default forms a ring")
    }
}

impl TryFrom<usize> for ConnCap {
    type Error = ConnCapTooLarge;

    fn try_from(bytes: usize) -> Result<Self, ConnCapTooLarge> {
        let ring = Config {
            capacity: ring_capacity(bytes, 2).ok_or(ConnCapTooLarge(bytes))?,
            max_readers: 1,
        };
        ring.region_len().map_err(|_| ConnCapTooLarge(bytes))?;
        Ok(Self { bytes, ring })
    }
}

impl JsonSchema for ConnCap {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        usize::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        usize::json_schema(generator)
    }
}

/// A `conn_cap` too large for any ring to hold two batches of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no ring holds a {0}-byte batch")]
pub struct ConnCapTooLarge(pub usize);

/// The set of connections a link is serving.
///
/// The link's loop queues batches through it, and each connection task
/// serves one slot. A new connection is sent the seed bytes before any batch.
pub(crate) struct Connections {
    slots: RefCell<Vec<Option<Rc<Outbox>>>>,
    cap: ConnCap,
    seed: Vec<u8>,
    inbox: Rc<Inbox>,
    changed: WaitQueue,
    /// Bytes written by connections that have since closed.
    bytes_out: Cell<u64>,
    batches_dropped: Cell<u64>,
}

impl Connections {
    /// Creates `count` empty slots.
    ///
    /// Each connection gets an outbox ring with room for two `cap`-byte
    /// batches, and a `cap`-byte receive buffer. It is sent `seed` first, and
    /// every packet it reads goes to `inbox`.
    pub(crate) fn new(count: usize, cap: ConnCap, seed: Vec<u8>, inbox: Rc<Inbox>) -> Self {
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

    /// Waits until the counters differ from what they are now.
    pub(crate) async fn changed(&self) {
        let observed = self.stats();
        let _ = self.changed.wait_for(|| self.stats() != observed).await;
    }

    /// Claims a free slot, or returns `None` when every slot is taken.
    ///
    /// This allocates a fresh outbox rather than reusing the old one, because
    /// the previous connection's write may still hold a record of its ring.
    fn claim_slot(&self) -> Option<ClaimedConnection> {
        let mut slots = self.slots.borrow_mut();
        let index = slots.iter().position(Option::is_none)?;
        // PANIC Safety: `ConnCap` checked that its config forms a ring.
        let (outbox, reader) = Outbox::new(self.cap.ring).expect("a checked ring config");
        let outbox = Rc::new(outbox);
        slots[index] = Some(outbox.clone());
        Some(ClaimedConnection {
            index,
            outbox,
            reader,
        })
    }

    /// Frees the slot of a closed connection. Its written bytes are added to
    /// the link's running total.
    fn free_slot(&self, index: usize, outbox: &Outbox) {
        self.slots.borrow_mut()[index] = None;
        self.bytes_out
            .set(self.bytes_out.get() + outbox.written.get());
        self.changed.wake_all();
    }

    /// Claims a slot for `stream` and returns the future that serves it, or
    /// `None` when every slot is taken.
    ///
    /// The future runs until the read or write side ends or `stop` is set,
    /// then frees the slot. Each connection checks its own peer's announce.
    pub(crate) fn serve(
        self: &Rc<Self>,
        stream: TcpStream,
        stop: &Stop,
    ) -> Option<impl Future<Output = ()> + 'static> {
        let ClaimedConnection {
            index,
            outbox,
            reader,
        } = self.claim_slot()?;
        let (conns, stop) = (self.clone(), stop.clone());
        Some(async move {
            let (rx, tx) = stream.split();
            let halves = future::or(
                write_half(&outbox, &conns.changed, tx, conns.seed.clone(), reader),
                read_half(rx, conns.cap.bytes, &conns.inbox),
            );
            future::or(halves, stop.wait()).await;
            conns.free_slot(index, &outbox);
        })
    }

    /// Queues `batch` on every open connection. Each connection whose outbox
    /// has no room counts as one dropped batch.
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

/// Writes the seed, then each batch the link queues, straight from the
/// outbox ring.
async fn write_half(
    outbox: &Outbox,
    changed: &WaitQueue,
    tx: OwnedWriter<TcpStream>,
    seed: Vec<u8>,
    reader: OutboxReader,
) {
    let OutboxReader { mut view, wake } = reader;
    let len = seed.len();
    if len > 0 {
        if tx.write_all(seed).await.0.is_err() {
            return;
        }
        outbox.sent(len);
        changed.wake_all();
    }
    loop {
        wake.wait_until(|| view.has_record()).await;
        view = match view.try_read_owned() {
            Ok(Ok(grant)) => {
                let len = grant.len();
                let Some(view) = send(&tx, grant).await else {
                    return;
                };
                outbox.sent(len);
                changed.wake_all();
                view
            }
            Ok(Err(view)) => view,
            Err(_) => return,
        };
    }
}

/// Writes all of `grant`'s record to `tx` and returns its view. Returns
/// `None` if the socket fails.
async fn send(tx: &OwnedWriter<TcpStream>, grant: OwnedGrant<NoWake>) -> Option<View<NoWake>> {
    let mut buf = GrantBuf(grant);
    while !buf.0.is_empty() {
        let (result, back) = tx.write(buf).await;
        buf = back;
        match result {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.0.skip(n),
        }
    }
    Some(buf.0.into_view())
}

/// Reads packet bodies into `inbox` until the peer errors or hangs up. One
/// `cap`-byte buffer is reused for every read.
///
/// A packet larger than the buffer is a read error, so it closes the
/// connection.
async fn read_half(rx: OwnedReader<TcpStream>, cap: usize, inbox: &Inbox) {
    let mut reader = LengthDelReader::<_, u32>::new(rx);
    let mut buf = vec![0u8; cap];
    let mut state = inbox.initial_state();
    loop {
        let Ok(body) = reader.recv(buf).await else {
            return;
        };
        inbox.route(&mut state, &body);
        buf = body.into_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conns(count: usize, cap: usize, seed: &[u8]) -> Rc<Connections> {
        let inbox = Rc::new(Inbox::blackhole());
        let cap = ConnCap::try_from(cap).expect("a small ring");
        Rc::new(Connections::new(count, cap, seed.to_vec(), inbox))
    }

    fn outbox(cap: usize) -> (Outbox, OutboxReader) {
        let config = Config {
            capacity: ring_capacity(cap, 2).expect("a small ring"),
            max_readers: 1,
        };
        Outbox::new(config).expect("a valid config")
    }

    #[test]
    fn test_outbox_drops_batch_that_does_not_fit() {
        let (outbox, OutboxReader { view, .. }) = outbox(8);
        assert!(outbox.enqueue(b"12345678"));
        assert!(outbox.enqueue(b"abc"));
        assert!(!outbox.enqueue(b"full"), "two batches fill the ring");
        let Ok(Ok(grant)) = view.try_read_owned() else {
            panic!("a queued batch")
        };
        assert_eq!(&grant[..], b"12345678");
        drop(grant.into_view());
        assert!(outbox.enqueue(b"room"), "sending a batch frees its space");
    }

    #[test]
    fn test_grant_buf_lends_the_ring_bytes() {
        let (outbox, OutboxReader { view, .. }) = outbox(8);
        assert!(outbox.enqueue(b"12345678"));
        let Ok(Ok(grant)) = view.try_read_owned() else {
            panic!("a queued batch")
        };
        let mut buf = GrantBuf(grant);
        // SAFETY: the buffer is live and `init_len` bytes are initialized.
        let lent = unsafe { core::slice::from_raw_parts(buf.stable_init_ptr(), buf.init_len()) };
        assert_eq!(lent, b"12345678");
        assert!(outbox.enqueue(b"abc"));
        assert!(!outbox.enqueue(b"held"), "the lent record stays pinned");
        buf.0.skip(5);
        assert_eq!((buf.init_len(), buf.total_len()), (3, 3));
        drop(buf);
        assert!(
            outbox.enqueue(b"gone"),
            "dropping the buffer releases the reader"
        );
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
        let changed = future::poll_once(conns.changed());
        assert!(future::block_on(changed).is_none());
    }

    #[test]
    fn test_claim_fills_slots_in_order() {
        let conns = conns(2, 16, b"");
        let first = conns.claim_slot().expect("a free slot");
        let second = conns.claim_slot().expect("another");
        assert!(conns.claim_slot().is_none());
        assert_eq!((first.index, second.index), (0, 1));
        conns.free_slot(first.index, &first.outbox);
        assert_eq!(conns.claim_slot().expect("the freed slot").index, 0);
    }

    #[test]
    fn test_conn_cap_forms_a_ring() {
        let cap: ConnCap = serde_json::from_str("64").expect("a small cap");
        assert_eq!(cap.bytes(), 64);
        assert_eq!(ConnCap::default().bytes(), 1 << 20);
        assert_eq!(ConnCap::try_from(0).map(|cap| cap.bytes()), Ok(0));
        assert_eq!(
            ConnCap::try_from(usize::MAX).map(|cap| cap.bytes()),
            Err(ConnCapTooLarge(usize::MAX))
        );
        assert!(serde_json::from_str::<ConnCap>(&usize::MAX.to_string()).is_err());
    }
}
