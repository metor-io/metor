//! The inbox where a link's connections put the packets they read.

use core::cell::{Cell, RefCell};
use core::mem::size_of;
use std::collections::HashSet;

use metor_fsw_3_ring::{Config, Notifier, RingBuffer, View, WakeSource, Writer};
use metor_proto::types::{Msg, PACKET_HEADER_LEN, PacketHeader, PacketId, PacketTy, Timestamp};
use metor_proto_wkt::{LinkInfo, SetComponentMetadata, SetMsgMetadata, VTableMsg};
use zerocopy::TryFromBytes;

use crate::port::ring_capacity;

const PER_RECORD_OVERHEAD: usize = PACKET_HEADER_LEN + size_of::<Timestamp>();

/// Messages a peer sends to describe itself before it sends data. None of
/// them are routed to a port.
const ANNOUNCE: [PacketId; 4] = [
    LinkInfo::ID,
    VTableMsg::ID,
    SetComponentMetadata::ID,
    SetMsgMetadata::ID,
];

/// The queue of inbound packets shared by all of a link's connections.
///
/// Connections push the packets they read, and the link drains them onto its
/// ports. Packets no port wants never enter the queue.
///
/// An inbox can also watch each peer's announce: the vtable messages a peer
/// sends before its first data packet. The link then logs a fault for any of
/// its tables the peer never announced.
pub(crate) struct Inbox {
    writer: RefCell<Writer<Notifier>>,
    wake: Notifier,
    max_len: usize,
    /// The packet type and id of every record some port takes. A timestamped
    /// message is keyed as a plain `Msg`.
    routes: HashSet<(PacketTy, PacketId)>,
    dropped: Cell<u64>,
    announce: Option<AnnounceWatch>,
}

/// Which of the inbox's tables the current peer has announced.
struct AnnounceWatch {
    /// Each routed table id, and whether the peer announced it.
    tables: RefCell<Vec<(PacketId, bool)>>,
    phase: Cell<Phase>,
}

/// Where the current peer is in its announce.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The peer has not sent data yet.
    Announcing,
    /// The peer sent data, and the link has not checked its announce yet.
    Due,
    /// The announce was checked, or no peer has connected.
    Checked,
}

impl Inbox {
    /// Creates an inbox and the view the link drains it from.
    ///
    /// `routes` lists the packet type and id of each port, in port order.
    /// With `watch_announce`, each connection's announce is tracked until its
    /// first data packet.
    pub(crate) fn new(
        routes: &[(PacketTy, PacketId)],
        inbox_cap: usize,
        record_max_len: usize,
        watch_announce: bool,
    ) -> Option<(Self, View<Notifier>)> {
        let cap = ring_capacity(record_max_len.checked_add(PER_RECORD_OVERHEAD)?, inbox_cap)?;
        let ring = RingBuffer::create_in_memory(Config {
            capacity: cap,
            max_readers: 1,
        })
        .ok()?;
        let wake = Notifier::default();
        // PANIC Safety: the ring is new, so its writer and its one reader slot
        // are both free.
        let writer = ring.writer(wake.clone()).expect("a new ring's writer");
        let view = ring.view(wake.clone()).expect("a new ring's reader slot");
        let announce = watch_announce.then(|| AnnounceWatch {
            tables: RefCell::new(
                routes
                    .iter()
                    .filter(|(ty, _)| *ty == PacketTy::Table)
                    .map(|&(_, id)| (id, false))
                    .collect(),
            ),
            phase: Cell::new(Phase::Checked),
        });
        let inbox = Self {
            writer: RefCell::new(writer),
            wake,
            max_len: record_max_len,
            routes: routes.iter().copied().collect(),
            dropped: Cell::new(0),
            announce,
        };
        Some((inbox, view))
    }

    /// Creates an inbox that routes no packets anywhere.
    pub(crate) fn blackhole() -> Self {
        // PANIC Safety: a ring with room for one empty record always builds.
        Self::new(&[], 1, 0, false).expect("a one-record ring").0
    }

    /// Marks the start of a new connection. A watched inbox forgets what the
    /// previous peer announced.
    pub(crate) fn connected(&self) {
        if let Some(watch) = &self.announce {
            for (_, announced) in watch.tables.borrow_mut().iter_mut() {
                *announced = false;
            }
            watch.phase.set(Phase::Announcing);
        }
    }

    /// Returns true once the current peer has sent data and the link has not
    /// yet checked its announce.
    pub(crate) fn announce_due(&self) -> bool {
        self.announce
            .as_ref()
            .is_some_and(|watch| watch.phase.get() == Phase::Due)
    }

    /// Returns the ids of the tables some port takes that the current peer did
    /// not announce before its first data packet, and marks its announce
    /// checked.
    ///
    /// Yields nothing unless [`announce_due`](Self::announce_due) is true.
    pub(crate) fn take_missing_table_ids(&self) -> impl Iterator<Item = PacketId> + '_ {
        let tables = self
            .announce
            .as_ref()
            .filter(|_| self.announce_due())
            .map(|watch| {
                watch.phase.set(Phase::Checked);
                watch.tables.borrow()
            });
        let len = tables.as_ref().map_or(0, |tables| tables.len());
        (0..len).filter_map(move |index| {
            let (id, announced) = tables.as_ref()?[index];
            (!announced).then_some(id)
        })
    }

    /// Queues one packet body for the link's ports.
    ///
    /// An announce updates the watch and is not queued, nor is a packet no
    /// port takes. A packet whose payload is longer than any port accepts, or
    /// that does not fit in the queue, is counted as dropped.
    pub(crate) fn route(&self, body: &[u8]) {
        let Some((ty, id, bytes)) = packet(body) else {
            return;
        };
        if ty == PacketTy::Msg && ANNOUNCE.contains(&id) {
            if id == VTableMsg::ID {
                self.recognize_vtable(bytes);
            }
            return;
        }
        self.note_data();
        let ty = match ty {
            PacketTy::MsgWithTimestamp => PacketTy::Msg,
            ty => ty,
        };
        if !self.routes.contains(&(ty, id)) {
            return;
        }
        if bytes.len() > self.max_len || self.writer.borrow_mut().try_write(body).is_err() {
            self.dropped.set(self.dropped.get() + 1);
        }
    }

    /// Marks the table a `VTableMsg` announces as one the peer sent, if a port
    /// takes it.
    ///
    /// Postcard encodes a `VTableMsg` with the table id's two bytes first, so
    /// only those are read.
    fn recognize_vtable(&self, bytes: &[u8]) {
        let Some(watch) = self.announce.as_ref() else {
            return;
        };
        let Some(&[a, b]) = bytes.get(..2) else {
            return;
        };
        if watch.phase.get() == Phase::Checked {
            return;
        }
        let mut tables = watch.tables.borrow_mut();
        if let Some((_, announced)) = tables.iter_mut().find(|(id, _)| *id == [a, b]) {
            *announced = true;
        }
    }

    /// Notes that the peer sent data, which ends its announce. On the first
    /// data packet, this wakes the link so it can check the announce.
    fn note_data(&self) {
        let Some(watch) = self.announce.as_ref() else {
            return;
        };
        if watch.phase.get() == Phase::Announcing {
            watch.phase.set(Phase::Due);
            self.wake.notify();
        }
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.get()
    }
}

/// Parses a packet body into its type, id, and payload. Returns `None` for a
/// time series packet, or a body too short for its header, so the type is
/// never `TimeSeries`.
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
        routed(cap, false)
    }

    fn watched(cap: usize) -> (Inbox, View<Notifier>) {
        routed(cap, true)
    }

    /// Routes for message ports `msgs` and frame ports `tables`.
    fn routes(msgs: &[PacketId], tables: &[PacketId]) -> Vec<(PacketTy, PacketId)> {
        let msgs = msgs.iter().map(|&id| (PacketTy::Msg, id));
        msgs.chain(tables.iter().map(|&id| (PacketTy::Table, id)))
            .collect()
    }

    fn routed(cap: usize, watch_announce: bool) -> (Inbox, View<Notifier>) {
        Inbox::new(
            &routes(&[id_of::<Fixed>(), id_of::<Note>()], &[TABLE]),
            cap,
            Note::MAX_LEN,
            watch_announce,
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
        inbox.route(&msg(id_of::<Note>(), b"one"));
        inbox.route(&msg(id_of::<Fixed>(), b"two"));
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
        inbox.route(&body(PacketTy::MsgWithTimestamp, id_of::<Note>(), &stamped));
        assert_eq!(drain(&mut view), vec![(id_of::<Note>(), b"late".to_vec())]);
    }

    #[test]
    fn test_ignore_unknown_record_id() {
        let (inbox, mut view) = inbox(4);
        inbox.route(&msg([9, 9], b"probe"));
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_route_table_by_id() {
        let (inbox, mut view) = inbox(4);
        inbox.route(&body(PacketTy::Table, TABLE, &[0u8; 4]));
        inbox.route(&body(PacketTy::Table, [3, 4], &[0u8; 4]));
        assert_eq!(drain(&mut view), vec![(TABLE, vec![0u8; 4])]);
        assert_eq!(inbox.dropped(), 0);
    }

    fn unannounced(inbox: &Inbox) -> Vec<PacketId> {
        inbox.take_missing_table_ids().collect()
    }

    #[test]
    fn test_watch_reports_unannounced_tables() {
        let (inbox, _view) = watched(4);
        assert!(
            unannounced(&inbox).is_empty(),
            "nothing is due before a peer"
        );
        inbox.connected();
        inbox.route(&vtable([3, 4]));
        assert!(!inbox.announce_due(), "no data yet");
        assert!(unannounced(&inbox).is_empty(), "a read before data is none");
        inbox.route(&msg([9, 9], b"probe"));
        assert!(inbox.announce_due(), "any data packet ends the announce");
        assert_eq!(unannounced(&inbox), vec![TABLE]);
        assert!(
            !inbox.announce_due() && unannounced(&inbox).is_empty(),
            "reported once"
        );
    }

    #[test]
    fn test_watch_passes_announced_table() {
        let (inbox, _view) = watched(4);
        inbox.connected();
        inbox.route(&vtable(TABLE));
        inbox.route(&body(PacketTy::Table, TABLE, &[0u8; 4]));
        assert!(unannounced(&inbox).is_empty());
    }

    #[test]
    fn test_unwatched_inbox_never_checks() {
        let (inbox, _view) = inbox(4);
        inbox.connected();
        inbox.route(&msg([9, 9], b"probe"));
        assert!(!inbox.announce_due(), "a listener watches nothing");
    }

    #[test]
    fn test_each_connection_is_checked_again() {
        let (inbox, _view) = watched(4);
        inbox.connected();
        inbox.route(&vtable(TABLE));
        inbox.route(&body(PacketTy::Table, TABLE, &[0u8; 4]));
        assert!(unannounced(&inbox).is_empty());

        inbox.connected();
        inbox.route(&vtable([3, 4]));
        inbox.route(&msg([9, 9], b"probe"));
        assert_eq!(
            unannounced(&inbox),
            vec![TABLE],
            "a peer rebuilt without the table faults again"
        );
    }

    #[test]
    fn test_announce_is_never_routed() {
        let (inbox, mut view) = watched(4);
        inbox.connected();
        inbox.route(&vtable(TABLE));
        inbox.route(&msg(LinkInfo::ID, b"identity"));
        inbox.route(&msg(SetComponentMetadata::ID, b"component"));
        inbox.route(&msg(SetMsgMetadata::ID, b"message"));
        assert!(drain(&mut view).is_empty());
        assert!(!inbox.announce_due(), "an announce is not data");
    }

    #[test]
    fn test_ignore_malformed_body() {
        let (inbox, mut view) = inbox(4);
        inbox.route(&[PacketTy::Msg as u8, 1]);
        inbox.route(&[0xff, 1, 2, 0, 9]);
        inbox.route(&body(
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
        inbox.route(&msg(id_of::<Fixed>(), b"one"));
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_full_ring_counts_drops_and_recovers() {
        let (inbox, mut view) = inbox(2);
        let offered = 64;
        for _ in 0..offered {
            inbox.route(&msg(id_of::<Fixed>(), b"one"));
        }
        let ones = drain(&mut view);
        assert!(ones.len() >= 2 && inbox.dropped() >= 1);
        assert_eq!(ones.len() as u64 + inbox.dropped(), offered);
        assert!(ones.iter().all(|(_, bytes)| bytes == b"one"));
        // Draining freed room in the ring for the next record.
        let dropped = inbox.dropped();
        inbox.route(&msg(id_of::<Fixed>(), b"two"));
        assert_eq!(drain(&mut view), vec![(id_of::<Fixed>(), b"two".to_vec())]);
        assert_eq!(inbox.dropped(), dropped);
    }

    #[test]
    fn test_reject_oversized_record() {
        let (inbox, mut view) =
            Inbox::new(&routes(&[id_of::<Fixed>()], &[]), 2, Fixed::MAX_LEN, false)
                .expect("a small ring");
        inbox.route(&msg(id_of::<Fixed>(), &[0u8; Fixed::MAX_LEN + 1]));
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 1);
    }

    #[test]
    fn test_reject_ring_too_large_for_memory() {
        assert!(Inbox::new(&[], usize::MAX, 8, false).is_none());
    }
}
