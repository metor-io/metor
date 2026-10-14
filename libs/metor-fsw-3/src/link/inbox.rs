//! The inbox where a link's connections put the packets they read.

use core::cell::Cell;
use core::mem::size_of;
use std::collections::HashSet;

use metor_fsw_3_ring::{Config, NoWake, Notifier, RingBuffer, View, Writer};
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
/// A dialing link also checks each peer's announce: the vtable messages a peer
/// sends before its first data packet. Each connection tracks its own
/// [`Announce`], and the tables its peer never announced are queued for the
/// link to log.
pub(crate) struct Inbox {
    writer: Writer<Notifier>,
    /// One record per table id a peer did not announce.
    missing: Writer<Notifier>,
    max_len: usize,
    /// The packet type and id of every record some port takes. A timestamped
    /// message is keyed as a plain `Msg`.
    routes: HashSet<(PacketTy, PacketId)>,
    /// The table ids some port takes, in port order.
    tables: Vec<PacketId>,
    watch_announce: bool,
    dropped: Cell<u64>,
}

/// Where one connection's peer is in its announce.
pub(crate) enum Announce {
    /// The peer has sent no data yet. `expected` holds the tables some port
    /// takes that the peer has not announced.
    Announcing {
        expected: HashSet<PacketId>,
    },
    Streaming,
}

impl Announce {
    /// Notes a table a `VTableMsg` announces.
    ///
    /// Postcard encodes a `VTableMsg` with the table id's two bytes first, so
    /// only those are read.
    fn recognize_vtable(&mut self, bytes: &[u8]) {
        if let (Self::Announcing { expected }, Some(&[a, b])) = (self, bytes.get(..2)) {
            expected.remove(&[a, b]);
        }
    }

    /// Ends the announce, returning the tables the peer never announced the
    /// first time this is called.
    fn start_streaming(&mut self) -> Option<HashSet<PacketId>> {
        match core::mem::replace(self, Self::Streaming) {
            Self::Announcing { expected } => Some(expected),
            Self::Streaming => None,
        }
    }
}

impl Inbox {
    /// Creates an inbox, the view the link drains packets from, and the view
    /// of the table ids peers did not announce.
    ///
    /// `routes` lists the packet type and id of each port, in port order.
    /// With `watch_announce`, each connection checks its peer's announce.
    pub(crate) fn new(
        routes: &[(PacketTy, PacketId)],
        inbox_cap: usize,
        record_max_len: usize,
        watch_announce: bool,
    ) -> Option<(Self, View<Notifier>, View<NoWake>)> {
        let ring = |max_len: usize, depth: usize| {
            let config = Config {
                capacity: ring_capacity(max_len, depth)?,
                max_readers: 1,
            };
            RingBuffer::create_in_memory(config).ok()
        };
        let packets = ring(record_max_len.checked_add(PER_RECORD_OVERHEAD)?, inbox_cap)?;
        let tables: Vec<PacketId> = routes
            .iter()
            .filter(|(ty, _)| *ty == PacketTy::Table)
            .map(|&(_, id)| id)
            .collect();
        let missing = ring(size_of::<PacketId>(), 4 * tables.len().max(1))?;
        let wake = Notifier::default();
        // PANIC Safety: the rings are new, so their writers and their one
        // reader slots are free.
        let writer = |ring: &RingBuffer| ring.writer(wake.clone()).expect("a new ring's writer");
        let inbox = Self {
            writer: writer(&packets),
            missing: writer(&missing),
            max_len: record_max_len,
            routes: routes.iter().copied().collect(),
            tables,
            watch_announce,
            dropped: Cell::new(0),
        };
        let packets = packets.view(wake).expect("a new ring's reader slot");
        let missing = missing.view(NoWake).expect("a new ring's reader slot");
        Some((inbox, packets, missing))
    }

    /// Creates an inbox that routes no packets anywhere.
    pub(crate) fn blackhole() -> Self {
        // PANIC Safety: a ring with room for one empty record always builds.
        Self::new(&[], 1, 0, false).expect("a one-record ring").0
    }

    /// Where a new connection starts: announcing, if this inbox checks
    /// announces, with every table some port takes expected.
    pub(crate) fn announce(&self) -> Announce {
        match self.watch_announce {
            true => Announce::Announcing {
                expected: self.tables.iter().copied().collect(),
            },
            false => Announce::Streaming,
        }
    }

    /// Queues one packet body that a connection in `announce` read.
    ///
    /// An announce updates `announce` and is not queued. The first data
    /// packet ends the announce and queues the tables the peer never
    /// announced. A packet no port takes is not queued, and one whose payload
    /// is longer than any port accepts, or that does not fit, is counted as
    /// dropped.
    pub(crate) fn route(&self, announce: &mut Announce, body: &[u8]) {
        let Some((ty, id, bytes)) = packet(body) else {
            return;
        };
        if ty == PacketTy::Msg && ANNOUNCE.contains(&id) {
            if id == VTableMsg::ID {
                announce.recognize_vtable(bytes);
            }
            return;
        }
        if let Some(expected) = announce.start_streaming() {
            for table in self.tables.iter().filter(|id| expected.contains(*id)) {
                let _ = self.missing.try_write(table);
            }
        }
        let ty = match ty {
            PacketTy::MsgWithTimestamp => PacketTy::Msg,
            ty => ty,
        };
        if !self.routes.contains(&(ty, id)) {
            return;
        }
        if bytes.len() > self.max_len || self.writer.try_write(body).is_err() {
            self.dropped.set(self.dropped.get() + 1);
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
        let (inbox, view, _) = routed(cap, false);
        (inbox, view)
    }

    /// An inbox that checks announces, and its view of missing tables.
    fn watched() -> (Inbox, View<NoWake>) {
        let (inbox, _, missing) = routed(4, true);
        (inbox, missing)
    }

    /// Routes `body` on a connection that does not check announces.
    fn route(inbox: &Inbox, body: &[u8]) {
        inbox.route(&mut Announce::Streaming, body);
    }

    fn missing(view: &mut View<NoWake>) -> Vec<PacketId> {
        let ids = view
            .drain()
            .filter_map(Result::ok)
            .map(|id| [id[0], id[1]])
            .collect();
        view.settle();
        ids
    }

    /// Routes for message ports `msgs` and frame ports `tables`.
    fn routes(msgs: &[PacketId], tables: &[PacketId]) -> Vec<(PacketTy, PacketId)> {
        let msgs = msgs.iter().map(|&id| (PacketTy::Msg, id));
        msgs.chain(tables.iter().map(|&id| (PacketTy::Table, id)))
            .collect()
    }

    fn routed(cap: usize, watch_announce: bool) -> (Inbox, View<Notifier>, View<NoWake>) {
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
        route(&inbox, &msg(id_of::<Note>(), b"one"));
        route(&inbox, &msg(id_of::<Fixed>(), b"two"));
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
        route(
            &inbox,
            &body(PacketTy::MsgWithTimestamp, id_of::<Note>(), &stamped),
        );
        assert_eq!(drain(&mut view), vec![(id_of::<Note>(), b"late".to_vec())]);
    }

    #[test]
    fn test_ignore_unknown_record_id() {
        let (inbox, mut view) = inbox(4);
        route(&inbox, &msg([9, 9], b"probe"));
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_route_table_by_id() {
        let (inbox, mut view) = inbox(4);
        route(&inbox, &body(PacketTy::Table, TABLE, &[0u8; 4]));
        route(&inbox, &body(PacketTy::Table, [3, 4], &[0u8; 4]));
        assert_eq!(drain(&mut view), vec![(TABLE, vec![0u8; 4])]);
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_missing_tables_are_queued_on_first_data() {
        let (inbox, mut view) = watched();
        let mut announce = inbox.announce();
        inbox.route(&mut announce, &vtable([3, 4]));
        assert!(missing(&mut view).is_empty(), "no data yet");
        inbox.route(&mut announce, &msg([9, 9], b"probe"));
        assert_eq!(missing(&mut view), vec![TABLE]);
        inbox.route(&mut announce, &msg([9, 9], b"probe"));
        assert!(missing(&mut view).is_empty(), "reported once");
    }

    #[test]
    fn test_announced_table_is_not_missing() {
        let (inbox, mut view) = watched();
        let mut announce = inbox.announce();
        inbox.route(&mut announce, &vtable(TABLE));
        inbox.route(&mut announce, &body(PacketTy::Table, TABLE, &[0u8; 4]));
        assert!(missing(&mut view).is_empty());
    }

    #[test]
    fn test_unwatched_inbox_never_checks() {
        let (inbox, _, mut view) = routed(4, false);
        let mut announce = inbox.announce();
        assert!(
            matches!(announce, Announce::Streaming),
            "a listener checks nothing"
        );
        inbox.route(&mut announce, &msg([9, 9], b"probe"));
        assert!(missing(&mut view).is_empty());
    }

    #[test]
    fn test_each_connection_is_checked_again() {
        let (inbox, mut view) = watched();
        let mut first = inbox.announce();
        inbox.route(&mut first, &vtable(TABLE));
        inbox.route(&mut first, &body(PacketTy::Table, TABLE, &[0u8; 4]));
        let mut second = inbox.announce();
        inbox.route(&mut second, &vtable([3, 4]));
        inbox.route(&mut second, &msg([9, 9], b"probe"));
        assert_eq!(
            missing(&mut view),
            vec![TABLE],
            "a peer rebuilt without the table faults again"
        );
    }

    #[test]
    fn test_announce_is_never_routed() {
        let (inbox, mut packets, mut view) = routed(4, true);
        let mut announce = inbox.announce();
        inbox.route(&mut announce, &vtable(TABLE));
        inbox.route(&mut announce, &msg(LinkInfo::ID, b"identity"));
        inbox.route(&mut announce, &msg(SetComponentMetadata::ID, b"component"));
        inbox.route(&mut announce, &msg(SetMsgMetadata::ID, b"message"));
        assert!(drain(&mut packets).is_empty());
        assert!(
            matches!(announce, Announce::Announcing { .. }),
            "an announce is not data"
        );
        assert!(missing(&mut view).is_empty());
    }

    #[test]
    fn test_ignore_malformed_body() {
        let (inbox, mut view) = inbox(4);
        route(&inbox, &[PacketTy::Msg as u8, 1]);
        route(&inbox, &[0xff, 1, 2, 0, 9]);
        route(
            &inbox,
            &body(PacketTy::MsgWithTimestamp, id_of::<Note>(), &[0u8; 3]),
        );
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_empty_inbox_takes_nothing() {
        let inbox = Inbox::blackhole();
        route(&inbox, &msg(id_of::<Fixed>(), b"one"));
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_full_ring_counts_drops_and_recovers() {
        let (inbox, mut view) = inbox(2);
        let offered = 64;
        for _ in 0..offered {
            route(&inbox, &msg(id_of::<Fixed>(), b"one"));
        }
        let ones = drain(&mut view);
        assert!(ones.len() >= 2 && inbox.dropped() >= 1);
        assert_eq!(ones.len() as u64 + inbox.dropped(), offered);
        assert!(ones.iter().all(|(_, bytes)| bytes == b"one"));
        // Draining freed room in the ring for the next record.
        let dropped = inbox.dropped();
        route(&inbox, &msg(id_of::<Fixed>(), b"two"));
        assert_eq!(drain(&mut view), vec![(id_of::<Fixed>(), b"two".to_vec())]);
        assert_eq!(inbox.dropped(), dropped);
    }

    #[test]
    fn test_reject_oversized_record() {
        let (inbox, mut view, _) =
            Inbox::new(&routes(&[id_of::<Fixed>()], &[]), 2, Fixed::MAX_LEN, false)
                .expect("a small ring");
        route(&inbox, &msg(id_of::<Fixed>(), &[0u8; Fixed::MAX_LEN + 1]));
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 1);
    }

    #[test]
    fn test_reject_ring_too_large_for_memory() {
        assert!(Inbox::new(&[], usize::MAX, 8, false).is_none());
    }
}
