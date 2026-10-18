//! Connection handling shared by `Publish` and `Subscribe`.
//!
//! A link serves up to a fixed number of connections at once. The link's
//! loop owns each open connection's outbox and queues every batch on it. The
//! transport task owns the tasks that serve the connections. Each connection
//! reads into the link's [`Inbox`], and tells the loop what it did through a
//! queue of [`ConnEvent`]s.

use std::rc::Rc;

use futures_lite::future;
use metor_fsw_3_ring::{
    Config, InvalidConfig, NoWake, Notifier, OwnedGrant, RingBuffer, View, WakeSink, Writer,
};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::Deserialize;
use stellarator::JoinHandle;
use stellarator::buf::IoBuf;
use stellarator::io::{AsyncWrite, LengthDelReader, OwnedReader, OwnedWriter, SplitExt};
use stellarator::net::TcpStream;
use thingbuf::mpsc::{Receiver, Sender};
use thingbuf::recycling::Recycle;

use crate::Stop;
use crate::port::ring_capacity;

use super::inbox::Inbox;

/// The events queued per slot before a connection waits for the loop.
const EVENTS_PER_SLOT: usize = 4;

/// What a connection tells its link's loop.
pub(crate) enum ConnEvent {
    /// A connection opened in slot `index`. The loop queues batches on
    /// `outbox`.
    Opened {
        index: usize,
        outbox: Writer<Notifier>,
    },
    /// A connection wrote this many bytes to its socket.
    Sent(u64),
    /// The connection in slot `index` closed.
    Closed { index: usize },
}

/// Empties a queue slot once its event is taken.
#[derive(Clone, Copy, Default)]
pub(crate) struct Taken;

impl Recycle<Option<ConnEvent>> for Taken {
    fn new_element(&self) -> Option<ConnEvent> {
        None
    }

    fn recycle(&self, element: &mut Option<ConnEvent>) {
        *element = None;
    }
}

type Events = Sender<Option<ConnEvent>, Taken>;

/// Creates an outbox ring with `config`: the writer the link's loop queues
/// batches on, and the view a connection sends them from.
fn outbox(config: Config) -> Result<(Writer<Notifier>, View<NoWake>, Notifier), InvalidConfig> {
    let ring = RingBuffer::create_in_memory(config)?;
    let wake = Notifier::default();
    // PANIC Safety: the ring is new, so its writer and its one reader slot
    // are both free.
    let writer = ring.writer(wake.clone()).expect("a new ring's writer");
    let view = ring.view(NoWake).expect("a new ring's reader slot");
    Ok((writer, view, wake))
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

/// The link loop's side of its connections: the outbox of each open slot,
/// and the counters.
pub(crate) struct Connections {
    outboxes: Vec<Option<Writer<Notifier>>>,
    events: Receiver<Option<ConnEvent>, Taken>,
    bytes_out: u64,
    batches_dropped: u64,
}

/// The transport's side of a link's connections: the task serving each slot.
pub(crate) struct Serving {
    tasks: Vec<Option<JoinHandle<()>>>,
    cap: ConnCap,
    seed: Rc<[u8]>,
    inbox: Rc<Inbox>,
    events: Events,
}

impl Connections {
    /// Creates `count` empty slots, and the transport's side of them.
    ///
    /// Each connection gets an outbox ring with room for two `cap`-byte
    /// batches, and a `cap`-byte receive buffer. It is sent `seed` first, and
    /// every packet it reads goes to `inbox`.
    pub(crate) fn new(
        count: usize,
        cap: ConnCap,
        seed: Vec<u8>,
        inbox: Rc<Inbox>,
    ) -> (Self, Serving) {
        let (events, receiver) =
            thingbuf::mpsc::with_recycle(EVENTS_PER_SLOT * count.max(1), Taken);
        let conns = Self {
            outboxes: (0..count).map(|_| None).collect(),
            events: receiver,
            bytes_out: 0,
            batches_dropped: 0,
        };
        let serving = Serving {
            tasks: (0..count).map(|_| None).collect(),
            cap,
            seed: seed.into(),
            inbox,
            events,
        };
        (conns, serving)
    }

    /// Waits for the next connection event, then applies it and every other
    /// one queued.
    pub(crate) async fn changed(&mut self) {
        if let Some(event) = self.events.recv().await {
            self.apply(event);
        }
        while let Ok(event) = self.events.try_recv() {
            self.apply(event);
        }
    }

    fn apply(&mut self, event: Option<ConnEvent>) {
        match event {
            Some(ConnEvent::Opened { index, outbox }) => self.outboxes[index] = Some(outbox),
            Some(ConnEvent::Sent(len)) => self.bytes_out += len,
            Some(ConnEvent::Closed { index }) => self.outboxes[index] = None,
            None => {}
        }
    }

    /// Queues `batch` on every open connection. Each connection whose outbox
    /// has no room counts as one dropped batch.
    pub(crate) fn enqueue(&mut self, batch: &[u8]) {
        for outbox in self.outboxes.iter_mut().flatten() {
            if outbox.try_write(batch).is_err() {
                self.batches_dropped += 1;
            }
        }
    }

    pub(crate) fn stats(&self) -> Stats {
        Stats {
            connections: self.outboxes.iter().flatten().count() as u32,
            bytes_out: self.bytes_out,
            batches_dropped: self.batches_dropped,
        }
    }
}

impl Serving {
    /// Claims a free slot for `stream` and spawns the task that serves it.
    /// Drops `stream` when every slot is taken.
    pub(crate) fn spawn(&mut self, stream: TcpStream, stop: &Stop) {
        for task in &mut self.tasks {
            if task.as_ref().is_some_and(|t| t.0.is_complete()) {
                *task = None;
            }
        }
        let Some(index) = self.tasks.iter().position(Option::is_none) else {
            return;
        };
        let served = self.serve(index, stream, stop);
        self.tasks[index] = Some(stellarator::spawn(served));
    }

    /// The future that serves `stream` in slot `index`, which must be free.
    ///
    /// It runs until the read or write side ends or `stop` is set, then
    /// tells the loop the slot is free. Each connection checks its own peer's
    /// announce.
    pub(crate) fn serve(
        &self,
        index: usize,
        stream: TcpStream,
        stop: &Stop,
    ) -> impl Future<Output = ()> + 'static {
        // PANIC Safety: `ConnCap` checked that its config forms a ring.
        let (writer, view, wake) = outbox(self.cap.ring).expect("a checked ring config");
        let (events, stop) = (self.events.clone(), stop.clone());
        let (seed, inbox, cap) = (self.seed.clone(), self.inbox.clone(), self.cap.bytes);
        async move {
            let opened = ConnEvent::Opened {
                index,
                outbox: writer,
            };
            if events.send(Some(opened)).await.is_err() {
                return;
            }
            let (rx, tx) = stream.split();
            let halves = future::or(
                write_half(&events, tx, &seed, view, wake),
                read_half(rx, cap, &inbox),
            );
            future::or(halves, stop.wait()).await;
            let _ = events.send(Some(ConnEvent::Closed { index })).await;
        }
    }
}

/// Writes the seed, then each batch the link queues, straight from the
/// outbox ring.
async fn write_half(
    events: &Events,
    tx: OwnedWriter<TcpStream>,
    seed: &[u8],
    mut view: View<NoWake>,
    wake: Notifier,
) {
    if !seed.is_empty() {
        if tx.write_all(seed.to_vec()).await.0.is_err() {
            return;
        }
        let _ = events.send(Some(ConnEvent::Sent(seed.len() as u64))).await;
    }
    loop {
        wake.wait_until(|| view.has_record()).await;
        view = match view.try_read_owned() {
            Ok(Ok(grant)) => {
                let len = grant.len() as u64;
                let Some(view) = send(&tx, grant).await else {
                    return;
                };
                let _ = events.send(Some(ConnEvent::Sent(len))).await;
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

    fn conns(count: usize, cap: usize, seed: &[u8]) -> (Connections, Serving) {
        let inbox = Rc::new(Inbox::blackhole());
        let cap = ConnCap::try_from(cap).expect("a small ring");
        Connections::new(count, cap, seed.to_vec(), inbox)
    }

    fn small_outbox(cap: usize) -> (Writer<Notifier>, View<NoWake>) {
        let config = Config {
            capacity: ring_capacity(cap, 2).expect("a small ring"),
            max_readers: 1,
        };
        let (writer, view, _) = outbox(config).expect("a valid config");
        (writer, view)
    }

    #[test]
    fn test_outbox_drops_batch_that_does_not_fit() {
        let (mut conns, _serving) = conns(1, 8, b"");
        let (writer, view) = small_outbox(8);
        conns.apply(Some(ConnEvent::Opened {
            index: 0,
            outbox: writer,
        }));
        conns.enqueue(b"12345678");
        conns.enqueue(b"abc");
        conns.enqueue(b"full");
        assert_eq!(
            conns.stats().batches_dropped,
            1,
            "two batches fill the ring"
        );
        let Ok(Ok(grant)) = view.try_read_owned() else {
            panic!("a queued batch")
        };
        assert_eq!(&grant[..], b"12345678");
        drop(grant.into_view());
        conns.enqueue(b"room");
        assert_eq!(
            conns.stats().batches_dropped,
            1,
            "sending a batch frees its space"
        );
    }

    #[test]
    fn test_grant_buf_lends_the_ring_bytes() {
        let (mut writer, view) = small_outbox(8);
        assert!(writer.try_write(b"12345678").is_ok());
        let Ok(Ok(grant)) = view.try_read_owned() else {
            panic!("a queued batch")
        };
        let mut buf = GrantBuf(grant);
        // SAFETY: the buffer is live and `init_len` bytes are initialized.
        let lent = unsafe { core::slice::from_raw_parts(buf.stable_init_ptr(), buf.init_len()) };
        assert_eq!(lent, b"12345678");
        assert!(writer.try_write(b"abc").is_ok());
        assert!(
            writer.try_write(b"held").is_err(),
            "the lent record stays pinned"
        );
        buf.0.skip(5);
        assert_eq!((buf.init_len(), buf.total_len()), (3, 3));
        drop(buf);
        assert!(
            writer.try_write(b"gone").is_ok(),
            "dropping the buffer releases the reader"
        );
    }

    #[test]
    fn test_empty_link_skips_queueing() {
        let (mut conns, _serving) = conns(2, 16, b"");
        conns.enqueue(b"batch");
        assert_eq!(conns.stats(), Stats::default());
    }

    #[test]
    fn test_changed_waits_for_an_event() {
        let (mut conns, serving) = conns(1, 64, b"");
        assert!(future::block_on(future::poll_once(conns.changed())).is_none());
        let _ = serving.events.try_send(Some(ConnEvent::Sent(3)));
        assert!(future::block_on(future::poll_once(conns.changed())).is_some());
        assert_eq!(conns.stats().bytes_out, 3);
    }

    #[test]
    fn test_events_track_open_slots() {
        let (mut conns, _serving) = conns(2, 16, b"");
        for index in 0..2 {
            let (outbox, _view) = small_outbox(16);
            conns.apply(Some(ConnEvent::Opened { index, outbox }));
        }
        assert_eq!(conns.stats().connections, 2);
        conns.apply(Some(ConnEvent::Closed { index: 0 }));
        assert_eq!(conns.stats().connections, 1);
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
