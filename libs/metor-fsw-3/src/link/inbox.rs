//! The shared inbox for connections

use core::cell::{Cell, RefCell};
use core::mem::size_of;

use metor_fsw_3_ring::{Config, Notifier, RingBuffer, View, Writer};
use metor_proto::types::{PACKET_HEADER_LEN, PacketHeader, PacketId, PacketTy, Timestamp};
use zerocopy::TryFromBytes;

use crate::port::ring_capacity;

const PER_RECORD_OVERHEAD: usize = PACKET_HEADER_LEN + size_of::<Timestamp>();

/// Shared inbox for connections
///
/// Internally this is a wrapper around [`RingBuffer`]
pub(crate) struct Inbox {
    writer: RefCell<Writer<Notifier>>,
    max_len: usize,
    ids: Vec<PacketId>,
    dropped: Cell<u64>,
}

impl Inbox {
    pub(crate) fn new(
        ids: Vec<PacketId>,
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
            max_len: record_max_len,
            ids,
            dropped: Cell::new(0),
        };
        Some((inbox, view))
    }

    /// A blackhole inbox that can accept nothing
    pub(crate) fn blackhole() -> Self {
        // PANIC Safety: one empty record always fits a ring.
        Self::new(Vec::new(), 1, 0).expect("a one-record ring").0
    }

    /// Writes one packet body carrying a message a port asked for; one longer
    /// than any port takes, or one the ring has no room for, is a drop. Every
    /// other packet is the peer's business.
    pub(crate) fn accept(&self, body: &[u8]) {
        let Some((id, bytes)) = message(body) else {
            return;
        };
        if !self.ids.contains(&id) {
            return;
        }
        if bytes.len() > self.max_len {
            return self.drop_one();
        }
        if self.writer.borrow_mut().try_write(body).is_err() {
            self.drop_one();
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

/// The id and bytes of the message a packet body carries; `None` for any
/// other packet, or one too short for its header.
pub(crate) fn message(body: &[u8]) -> Option<(PacketId, &[u8])> {
    let (header, rest) = PacketHeader::try_ref_from_prefix(body).ok()?;
    let bytes = match header.packet_ty {
        PacketTy::Msg => rest,
        PacketTy::MsgWithTimestamp => rest.get(size_of::<Timestamp>()..)?,
        PacketTy::Table | PacketTy::TimeSeries => return None,
    };
    Some((header.id, bytes))
}

#[cfg(test)]
mod tests {
    use crate::record::Record;
    use crate::tests::utils::{Fixed, Note};

    use super::*;

    fn packet(ty: PacketTy, id: PacketId, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![ty as u8, id[0], id[1], 0];
        body.extend_from_slice(payload);
        body
    }

    fn msg(id: PacketId, payload: &[u8]) -> Vec<u8> {
        packet(PacketTy::Msg, id, payload)
    }

    fn id_of<R: Record>() -> PacketId {
        R::schema().packet_id()
    }

    fn inbox(cap: usize) -> (Inbox, View<Notifier>) {
        Inbox::new(vec![id_of::<Fixed>(), id_of::<Note>()], cap, Note::MAX_LEN)
            .expect("a small ring")
    }

    fn drain(view: &mut View<Notifier>) -> Vec<(PacketId, Vec<u8>)> {
        let seen = view
            .drain()
            .filter_map(Result::ok)
            .filter_map(message)
            .map(|(id, bytes)| (id, bytes.to_vec()))
            .collect();
        view.settle();
        seen
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
        let mut body = 7i64.to_le_bytes().to_vec();
        body.extend_from_slice(b"late");
        inbox.accept(&packet(PacketTy::MsgWithTimestamp, id_of::<Note>(), &body));
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
    fn test_ignore_table_packet() {
        let (inbox, mut view) = inbox(4);
        inbox.accept(&packet(PacketTy::Table, [1, 2], &[0u8; 4]));
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 0);
    }

    #[test]
    fn test_ignore_malformed_body() {
        let (inbox, mut view) = inbox(4);
        inbox.accept(&[PacketTy::Msg as u8, 1]);
        inbox.accept(&[0xff, 1, 2, 0, 9]);
        inbox.accept(&packet(
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
        let (inbox, mut view) =
            Inbox::new(vec![id_of::<Fixed>()], 2, Fixed::MAX_LEN).expect("a small ring");
        inbox.accept(&msg(id_of::<Fixed>(), &[0u8; Fixed::MAX_LEN + 1]));
        assert!(drain(&mut view).is_empty());
        assert_eq!(inbox.dropped(), 1);
    }

    #[test]
    fn test_reject_ring_too_large_for_memory() {
        assert!(Inbox::new(Vec::new(), usize::MAX, 8).is_none());
    }
}
