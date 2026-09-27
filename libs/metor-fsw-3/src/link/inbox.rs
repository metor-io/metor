//! The shared inbox for connections

use core::cell::{Cell, RefCell};
use core::mem::size_of;

use metor_fsw_3_ring::{Config, Notifier, RingBuffer, View, WakeSource, Writer};
use metor_proto::types::{Msg, PACKET_HEADER_LEN, PacketHeader, PacketId, PacketTy, Timestamp};
use metor_proto_wkt::{LinkInfo, SetComponentMetadata, SetMsgMetadata, VTableMsg};
use zerocopy::TryFromBytes;

use crate::port::ring_capacity;

const PER_RECORD_OVERHEAD: usize = PACKET_HEADER_LEN + size_of::<Timestamp>();

/// What a peer sends about itself before its data; none of it is data.
const ANNOUNCE: [PacketId; 3] = [LinkInfo::ID, SetComponentMetadata::ID, SetMsgMetadata::ID];

/// Shared inbox for connections
///
/// Internally this is a wrapper around [`RingBuffer`]. With the announce
/// watch on, it also records which of its tables the peer announced before
/// its first data packet, so the link can fault a port whose table never
/// came. Every connection starts that record over.
pub(crate) struct Inbox {
    writer: RefCell<Writer<Notifier>>,
    wake: Notifier,
    max_len: usize,
    msgs: Vec<PacketId>,
    tables: Vec<PacketId>,
    dropped: Cell<u64>,
    watching: Cell<bool>,
    pending: Cell<bool>,
    /// One flag per entry of `tables`, set by this connection's announce.
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
        // PANIC Safety: the ring is new, so its writer and reader slot are free.
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

    /// A blackhole inbox that can accept nothing
    pub(crate) fn blackhole() -> Self {
        // PANIC Safety: one empty record always fits a ring.
        Self::new(Vec::new(), Vec::new(), 1, 0)
            .expect("a one-record ring")
            .0
    }

    /// Watches every connection's announce, each until its first data packet.
    pub(crate) fn watch(&self) {
        self.watching.set(true);
    }

    /// Starts one connection: what the last peer announced is forgotten and
    /// the watch, if this inbox has one, is armed again.
    pub(crate) fn connected(&self) {
        if !self.watching.get() {
            return;
        }
        self.announced.borrow_mut().fill(false);
        self.seen_data.set(false);
        self.pending.set(true);
    }

    /// Whether the watch has what it needs and the link has not read it.
    pub(crate) fn check_due(&self) -> bool {
        self.pending.get() && self.seen_data.get()
    }

    /// The tables this peer never announced, once its data started; ends this
    /// connection's watch. Empty before then, and after.
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

    /// Writes one packet body carrying a message or a table a port asked for;
    /// one longer than any port takes, or one the ring has no room for, is a
    /// drop. Every other packet is the peer's business.
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

    /// Notes an announced table: postcard lays a `VTableMsg` out with its id's
    /// two bytes first. A table no port of this inbox takes is the peer's
    /// business.
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

    /// The first data packet ends the announce, which the watching link is
    /// woken to read.
    fn data_seen(&self) {
        if self.pending.get() && !self.seen_data.replace(true) {
            self.wake.notify();
        }
    }

    /// Counts one packet no port will see.
    fn drop_one(&self) {
        self.dropped.set(self.dropped.get() + 1);
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.get()
    }
}

/// The type, id, and payload of the record a packet body carries; `None` for
/// a time series, or a body too short for its header.
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
        // The drained space takes the next record.
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
