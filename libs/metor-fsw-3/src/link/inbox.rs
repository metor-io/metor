//! The inbox where a link's connections put the packets they read.

use core::cell::{Cell, RefCell};
use core::mem::size_of;

use metor_fsw_3_ring::{Config, Notifier, RingBuffer, View, WakeSource, Writer};
use metor_proto::types::{Msg, PACKET_HEADER_LEN, PacketHeader, PacketId, PacketTy, Timestamp};
use metor_proto_wkt::{LinkInfo, SetComponentMetadata, SetMsgMetadata, VTableMsg};
use zerocopy::TryFromBytes;

use crate::port::ring_capacity;

const PER_RECORD_OVERHEAD: usize = PACKET_HEADER_LEN + size_of::<Timestamp>();

/// Messages a peer sends to describe itself before it sends data. None of
/// them are routed to a port.
const ANNOUNCE: [PacketId; 3] = [LinkInfo::ID, SetComponentMetadata::ID, SetMsgMetadata::ID];

/// The queue of inbound packets shared by all of a link's connections.
///
/// Connections push the packets they read, and the link drains them onto its
/// ports. Packets no port wants never enter the queue.
///
/// An inbox can also watch each peer's announce. The announce is the set of
/// vtable messages a peer sends before its first data packet. While watching,
/// the inbox records which of its tables the peer announced, so the link can
/// log a fault for any table that never arrived. That record resets on every
/// new connection.
pub(crate) struct Inbox {
    writer: RefCell<Writer<Notifier>>,
    wake: Notifier,
    max_len: usize,
    msgs: Vec<PacketId>,
    tables: Vec<PacketId>,
    dropped: Cell<u64>,
    watching: Cell<bool>,
    pending: Cell<bool>,
    /// One flag per entry of `tables`, set when the current peer announces
    /// that table.
    announced: RefCell<Vec<bool>>,
    seen_data: Cell<bool>,
}

impl Inbox {
    pub(crate) fn new(
        msgs: Vec<PacketId>,
        tables: Vec<PacketId>,
        inbox_cap: usize,
        record_max_len: usize,
    ) -> Option<(Self, View<Notifier>)> {
        let cap = ring_capacity(record_max_len.checked_add(PER_RECORD_OVERHEAD)?, inbox_cap)?;
        let ring = RingBuffer::create_in_memory(Config {
            capacity: cap,
            max_readers: 1,
        });
        let wake = Notifier::default();
        // PANIC Safety: the ring is new, so its writer and its one reader slot
        // are both free.
        let writer = ring.writer(wake.clone()).expect("a new ring's writer");
        let view = ring.view(wake.clone()).expect("a new ring's reader slot");
        let inbox = Self {
            writer: RefCell::new(writer),
            wake,
            max_len: record_max_len,
            announced: RefCell::new(vec![false; tables.len()]),
            msgs,
            tables,
            dropped: Cell::new(0),
            watching: Cell::new(false),
            pending: Cell::new(false),
            seen_data: Cell::new(false),
        };
        Some((inbox, view))
    }

    /// Creates an inbox that routes no packets anywhere.
    pub(crate) fn blackhole() -> Self {
        // PANIC Safety: a ring with room for one empty record always builds.
        Self::new(Vec::new(), Vec::new(), 1, 0)
            .expect("a one-record ring")
            .0
    }

    /// Turns on the announce watch. From then on, each connection's announce
    /// is tracked until that connection's first data packet.
    pub(crate) fn watch(&self) {
        self.watching.set(true);
    }

    /// Marks the start of a new connection. If the announce watch is on, this
    /// forgets what the previous peer announced and arms the watch again.
    pub(crate) fn connected(&self) {
        if !self.watching.get() {
            return;
        }
        self.announced.borrow_mut().fill(false);
        self.seen_data.set(false);
        self.pending.set(true);
    }

    /// Returns true once the current peer has sent data and the link has not
    /// yet checked its announce.
    pub(crate) fn check_due(&self) -> bool {
        self.pending.get() && self.seen_data.get()
    }

    /// Returns the tables the current peer did not announce before its first
    /// data packet.
    ///
    /// The first call after [`check_due`](Self::check_due) turns true yields
    /// them and ends the watch for this connection. Every other call yields
    /// nothing.
    pub(crate) fn unannounced_iter(&self) -> impl Iterator<Item = PacketId> + '_ {
        let due = self.check_due();
        if due {
            self.pending.set(false);
        }
        let announced = self.announced.borrow();
        let count = if due { self.tables.len() } else { 0 };
        (0..count)
            .filter(move |&at| !announced[at])
            .map(|at| self.tables[at])
    }

    /// Queues one packet body for the link's ports.
    ///
    /// Packets that no port takes are ignored. A packet whose payload is longer
    /// than any port accepts, or that does not fit in the queue, is counted as
    /// dropped.
    pub(crate) fn accept(&self, body: &[u8]) {
        let Some((ty, id, bytes)) = packet(body) else {
            return;
        };
        let routed = match ty {
            PacketTy::Msg if id == VTableMsg::ID => return self.announce(bytes),
            PacketTy::Msg if ANNOUNCE.contains(&id) => return,
            PacketTy::Table => self.tables.contains(&id),
            PacketTy::Msg | PacketTy::MsgWithTimestamp => self.msgs.contains(&id),
            PacketTy::TimeSeries => return,
        };
        self.data_seen();
        if !routed {
            return;
        }
        if bytes.len() > self.max_len {
            return self.drop_one();
        }
        if self.writer.borrow_mut().try_write(body).is_err() {
            self.drop_one();
        }
    }

    /// Records that the peer announced a table.
    ///
    /// Postcard encodes a `VTableMsg` with the table id's two bytes first, so
    /// only those are read. Tables no port takes are ignored.
    fn announce(&self, bytes: &[u8]) {
        if !self.pending.get() {
            return;
        }
        let Some(&[a, b]) = bytes.get(..2) else {
            return;
        };
        if let Some(at) = self.tables.iter().position(|id| *id == [a, b]) {
            self.announced.borrow_mut()[at] = true;
        }
    }

    /// Notes that the peer sent data, which ends its announce. On the first
    /// data packet, this wakes the link so it can check the announce.
    fn data_seen(&self) {
        if self.pending.get() && !self.seen_data.replace(true) {
            self.wake.notify();
        }
    }

    /// Counts one packet that no port will receive.
    fn drop_one(&self) {
        self.dropped.set(self.dropped.get() + 1);
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.get()
    }
}

/// Parses a packet body into its type, id, and payload. Returns `None` for a
/// time series packet, or a body too short for its header.
pub(crate) fn packet(body: &[u8]) -> Option<(PacketTy, PacketId, &[u8])> {
    let (header, rest) = PacketHeader::try_ref_from_prefix(body).ok()?;
    let bytes = match header.packet_ty {
        PacketTy::Msg | PacketTy::Table => rest,
        PacketTy::MsgWithTimestamp => rest.get(size_of::<Timestamp>()..)?,
        PacketTy::TimeSeries => return None,
    };
    Some((header.packet_ty, header.id, bytes))
}

#[cfg(test)]
mod tests {
    use crate::record::Record;
    use crate::tests::utils::{Fixed, Note};

    use super::*;

    fn body(ty: PacketTy, id: PacketId, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![ty as u8, id[0], id[1], 0];
        body.extend_from_slice(payload);
        body
    }

    fn msg(id: PacketId, payload: &[u8]) -> Vec<u8> {
        body(PacketTy::Msg, id, payload)
    }

    fn id_of<R: Record>() -> PacketId {
        R::schema().packet_id()
    }

    const TABLE: PacketId = [1, 2];

    fn inbox(cap: usize) -> (Inbox, View<Notifier>) {
        Inbox::new(
            vec![id_of::<Fixed>(), id_of::<Note>()],
            vec![TABLE],
            cap,
            Note::MAX_LEN,
        )
        .expect("a small ring")
    }

    fn drain(view: &mut View<Notifier>) -> Vec<(PacketId, Vec<u8>)> {
        let seen = view
            .drain()
            .filter_map(Result::ok)
            .filter_map(packet)
            .map(|(_, id, bytes)| (id, bytes.to_vec()))
            .collect();
        view.settle();
        seen
    }

    fn vtable(id: PacketId) -> Vec<u8> {
        body(PacketTy::Msg, VTableMsg::ID, &[id[0], id[1], 0, 0])
    }

    #[test]
    fn test_drain_yields_in_arrival_order() {
        let (inbox, mut view) = inbox(4);
        inbox.accept(&msg(id_of::<Note>(), b"one"));
        inbox.accept(&msg(id_of::<Fixed>(), b"two"));
        assert_eq!(
            drain(&mut view),
            vec![
                (id_of::<Note>(), b"one".to_vec()),
                (id_of::<Fixed>(), b"two".to_vec())
            ]
        );
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_route_timestamped_message_without_its_timestamp() {
        let (inbox, mut view) = inbox(4);
        let mut stamped = 7i64.to_le_bytes().to_vec();
        stamped.extend_from_slice(b"late");
        inbox.accept(&body(PacketTy::MsgWithTimestamp, id_of::<Note>(), &stamped));
        assert_eq!(drain(&mut view), vec![(id_of::<Note>(), b"late".to_vec())]);
    }

    #[test]
    fn test_ignore_unknown_record_id() {
        let (inbox, mut view) = inbox(4);
        inbox.accept(&msg([9, 9], b"probe"));
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_route_table_by_id() {
        let (inbox, mut view) = inbox(4);
        inbox.accept(&body(PacketTy::Table, TABLE, &[0u8; 4]));
        inbox.accept(&body(PacketTy::Table, [3, 4], &[0u8; 4]));
        assert_eq!(drain(&mut view), vec![(TABLE, vec![0u8; 4])]);
        assert_eq!(inbox.dropped(), 0);
    }

    fn unannounced(inbox: &Inbox) -> Vec<PacketId> {
        inbox.unannounced_iter().collect()
    }

    #[test]
    fn test_watch_reports_unannounced_tables() {
        let (inbox, _view) = inbox(4);
        assert!(unannounced(&inbox).is_empty(), "nothing is due unwatched");
        inbox.watch();
        inbox.connected();
        inbox.accept(&vtable([3, 4]));
        assert!(!inbox.check_due(), "no data yet");
        assert!(unannounced(&inbox).is_empty(), "a read before data is none");
        inbox.accept(&msg([9, 9], b"probe"));
        assert!(inbox.check_due(), "any data packet ends the announce");
        assert_eq!(unannounced(&inbox), vec![TABLE]);
        assert!(
            !inbox.check_due() && unannounced(&inbox).is_empty(),
            "reported once"
        );
    }

    #[test]
    fn test_watch_passes_announced_table() {
        let (inbox, _view) = inbox(4);
        inbox.watch();
        inbox.connected();
        inbox.accept(&vtable(TABLE));
        inbox.accept(&body(PacketTy::Table, TABLE, &[0u8; 4]));
        assert!(unannounced(&inbox).is_empty());
    }

    #[test]
    fn test_unwatched_inbox_never_checks() {
        let (inbox, _view) = inbox(4);
        inbox.connected();
        inbox.accept(&msg([9, 9], b"probe"));
        assert!(!inbox.check_due(), "a listener watches nothing");
    }

    #[test]
    fn test_each_connection_is_checked_again() {
        let (inbox, _view) = inbox(4);
        inbox.watch();
        inbox.connected();
        inbox.accept(&vtable(TABLE));
        inbox.accept(&body(PacketTy::Table, TABLE, &[0u8; 4]));
        assert!(unannounced(&inbox).is_empty());

        inbox.connected();
        inbox.accept(&vtable([3, 4]));
        inbox.accept(&msg([9, 9], b"probe"));
        assert_eq!(
            unannounced(&inbox),
            vec![TABLE],
            "a peer rebuilt without the table faults again"
        );
    }

    #[test]
    fn test_announce_is_never_routed() {
        let (inbox, mut view) = inbox(4);
        inbox.watch();
        inbox.connected();
        inbox.accept(&vtable(TABLE));
        inbox.accept(&msg(LinkInfo::ID, b"identity"));
        inbox.accept(&msg(SetComponentMetadata::ID, b"component"));
        inbox.accept(&msg(SetMsgMetadata::ID, b"message"));
        assert!(drain(&mut view).is_empty());
        assert!(!inbox.check_due(), "an announce is not data");
    }

    #[test]
    fn test_ignore_malformed_body() {
        let (inbox, mut view) = inbox(4);
        inbox.accept(&[PacketTy::Msg as u8, 1]);
        inbox.accept(&[0xff, 1, 2, 0, 9]);
        inbox.accept(&body(
            PacketTy::MsgWithTimestamp,
            id_of::<Note>(),
            &[0u8; 3],
        ));
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_empty_inbox_takes_nothing() {
        let inbox = Inbox::blackhole();
        inbox.accept(&msg(id_of::<Fixed>(), b"one"));
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_full_ring_counts_drops_and_recovers() {
        let (inbox, mut view) = inbox(2);
        let offered = 64;
        for _ in 0..offered {
            inbox.accept(&msg(id_of::<Fixed>(), b"one"));
        }
        let ones = drain(&mut view);
        assert!(ones.len() >= 2 && inbox.dropped() >= 1);
        assert_eq!(ones.len() as u64 + inbox.dropped(), offered);
        assert!(ones.iter().all(|(_, bytes)| bytes == b"one"));
        // Draining freed room in the ring for the next record.
        let dropped = inbox.dropped();
        inbox.accept(&msg(id_of::<Fixed>(), b"two"));
        assert_eq!(drain(&mut view), vec![(id_of::<Fixed>(), b"two".to_vec())]);
        assert_eq!(inbox.dropped(), dropped);
    }

    #[test]
    fn test_reject_oversized_record() {
        let (inbox, mut view) = Inbox::new(vec![id_of::<Fixed>()], Vec::new(), 2, Fixed::MAX_LEN)
            .expect("a small ring");
        inbox.accept(&msg(id_of::<Fixed>(), &[0u8; Fixed::MAX_LEN + 1]));
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 1);
    }

    #[test]
    fn test_reject_ring_too_large_for_memory() {
        assert!(Inbox::new(Vec::new(), Vec::new(), usize::MAX, 8).is_none());
    }
}
