//! What a link reads from one connection's peer.

use core::mem::size_of;

use metor_proto::types::{Msg, PacketHeader, PacketId, PacketTy, Timestamp};
use metor_proto_wkt::{LinkInfo, SetComponentMetadata, SetMsgMetadata, VTableMsg};
use zerocopy::TryFromBytes;

/// Messages a peer sends to describe itself before it sends data. None of
/// them are routed to a port.
const ANNOUNCE: [PacketId; 4] = [
    LinkInfo::ID,
    VTableMsg::ID,
    SetComponentMetadata::ID,
    SetMsgMetadata::ID,
];

/// Where one connection's peer is in its announce, against a link's tables.
pub(crate) struct ConnectionState {
    /// Whether the peer has sent no data yet on a connection that checks
    /// its announce.
    announcing: bool,
    /// Per table, in the link's order: whether the peer has not announced
    /// it.
    unseen: Box<[bool]>,
}

/// The first field of a `VTableMsg`, which is all an announce needs.
#[derive(serde::Deserialize)]
struct VTableHead {
    id: PacketId,
}

impl ConnectionState {
    /// The state for a link with `tables` tables. It checks nothing until
    /// `reset`.
    pub(crate) fn new(tables: usize) -> Self {
        Self {
            announcing: false,
            unseen: vec![false; tables].into(),
        }
    }

    /// Starts a new connection. With `watch`, its peer must announce every
    /// table before its first data packet.
    pub(crate) fn reset(&mut self, watch: bool) {
        self.announcing = watch;
        self.unseen.fill(watch);
    }

    /// Reads one packet body. Returns the packet unless it is malformed or
    /// part of the announce.
    ///
    /// The first data packet ends the announce, and `missing` is called
    /// with each of `tables` the peer did not announce.
    pub(crate) fn route<'a>(
        &mut self,
        tables: &[PacketId],
        body: &'a [u8],
        mut missing: impl FnMut(PacketId),
    ) -> Option<(PacketTy, PacketId, &'a [u8])> {
        debug_assert_eq!(tables.len(), self.unseen.len(), "one flag per table");
        let (ty, id, bytes) = packet(body)?;
        if ty == PacketTy::Msg && ANNOUNCE.contains(&id) {
            if id == VTableMsg::ID {
                self.recognize(tables, bytes);
            }
            return None;
        }
        if core::mem::take(&mut self.announcing) {
            tables
                .iter()
                .zip(&self.unseen)
                .filter(|(_, unseen)| **unseen)
                .for_each(|(&table, _)| missing(table));
        }
        Some((ty, id, bytes))
    }

    /// Notes the table a `VTableMsg` announces.
    fn recognize(&mut self, tables: &[PacketId], bytes: &[u8]) {
        let Ok((head, _)) = postcard::take_from_bytes::<VTableHead>(bytes) else {
            return;
        };
        for (_, unseen) in tables
            .iter()
            .zip(&mut self.unseen)
            .filter(|(t, _)| **t == head.id)
        {
            *unseen = false;
        }
    }
}

/// Parses a packet body into its type, id, and payload.
///
/// A timestamped message is returned as a plain `Msg` without its timestamp.
/// Returns `None` for a time series packet, or a body too short for its
/// header, so the type is only ever `Msg` or `Table`.
pub(crate) fn packet(body: &[u8]) -> Option<(PacketTy, PacketId, &[u8])> {
    let (header, rest) = PacketHeader::try_ref_from_prefix(body).ok()?;
    let (ty, bytes) = match header.packet_ty {
        PacketTy::Msg | PacketTy::Table => (header.packet_ty, rest),
        PacketTy::MsgWithTimestamp => (PacketTy::Msg, rest.get(size_of::<Timestamp>()..)?),
        PacketTy::TimeSeries => return None,
    };
    Some((ty, header.id, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: PacketId = [1, 2];
    const OTHER: PacketId = [3, 4];
    const TABLES: [PacketId; 2] = [TABLE, OTHER];

    fn body(ty: PacketTy, id: PacketId, payload: &[u8]) -> Vec<u8> {
        let mut body = vec![ty as u8, id[0], id[1], 0];
        body.extend_from_slice(payload);
        body
    }

    fn msg(id: PacketId, payload: &[u8]) -> Vec<u8> {
        body(PacketTy::Msg, id, payload)
    }

    fn vtable(id: PacketId) -> Vec<u8> {
        let msg = VTableMsg {
            id,
            vtable: Default::default(),
        };
        let bytes = postcard::to_allocvec(&msg).expect("encodes");
        body(PacketTy::Msg, VTableMsg::ID, &bytes)
    }

    fn watched() -> ConnectionState {
        let mut state = ConnectionState::new(TABLES.len());
        state.reset(true);
        state
    }

    /// Routes `body`, returning the packet's id and payload, and the tables
    /// reported missing.
    fn route(
        state: &mut ConnectionState,
        body: &[u8],
    ) -> (Option<(PacketId, Vec<u8>)>, Vec<PacketId>) {
        let mut missing = Vec::new();
        let packet = state.route(&TABLES, body, |id| missing.push(id));
        (packet.map(|(_, id, bytes)| (id, bytes.to_vec())), missing)
    }

    #[test]
    fn test_route_returns_the_payload() {
        let mut state = ConnectionState::new(TABLES.len());
        let (packet, missing) = route(&mut state, &msg([9, 9], b"one"));
        assert_eq!(packet, Some(([9, 9], b"one".to_vec())));
        assert!(missing.is_empty());
        let mut missing = Vec::new();
        let table = body(PacketTy::Table, TABLE, &[7; 4]);
        let routed = state.route(&TABLES, &table, |id| missing.push(id));
        assert_eq!(routed, Some((PacketTy::Table, TABLE, &[7u8; 4][..])));
    }

    #[test]
    fn test_route_timestamped_message_without_its_timestamp() {
        let mut state = ConnectionState::new(0);
        state.reset(false);
        let mut stamped = 7i64.to_le_bytes().to_vec();
        stamped.extend_from_slice(b"late");
        let stamped = body(PacketTy::MsgWithTimestamp, [9, 9], &stamped);
        let routed = state.route(&[], &stamped, |_| {});
        assert_eq!(routed, Some((PacketTy::Msg, [9, 9], &b"late"[..])));
    }

    #[test]
    fn test_missing_tables_are_reported_on_first_data() {
        let mut state = watched();
        assert_eq!(route(&mut state, &vtable(OTHER)), (None, Vec::new()));
        let (packet, missing) = route(&mut state, &msg([9, 9], b"probe"));
        assert!(packet.is_some());
        assert_eq!(missing, [TABLE]);
        assert!(
            route(&mut state, &msg([9, 9], b"probe")).1.is_empty(),
            "reported once"
        );
    }

    #[test]
    fn test_announced_tables_are_not_missing() {
        let mut state = watched();
        route(&mut state, &vtable(TABLE));
        route(&mut state, &vtable(OTHER));
        let (_, missing) = route(&mut state, &body(PacketTy::Table, TABLE, &[0; 4]));
        assert!(missing.is_empty());
    }

    #[test]
    fn test_reset_checks_a_new_connection_again() {
        let mut state = watched();
        route(&mut state, &vtable(TABLE));
        route(&mut state, &vtable(OTHER));
        route(&mut state, &msg([9, 9], b"probe"));
        state.reset(true);
        route(&mut state, &vtable(OTHER));
        assert_eq!(
            route(&mut state, &msg([9, 9], b"probe")).1,
            [TABLE],
            "a peer rebuilt without the table faults again"
        );
    }

    #[test]
    fn test_unwatched_connection_never_checks() {
        let mut state = watched();
        state.reset(false);
        assert!(route(&mut state, &msg([9, 9], b"probe")).1.is_empty());
    }

    #[test]
    fn test_announce_is_never_routed() {
        let mut state = watched();
        for announce in [
            vtable(TABLE),
            msg(LinkInfo::ID, b"identity"),
            msg(SetComponentMetadata::ID, b"component"),
            msg(SetMsgMetadata::ID, b"message"),
        ] {
            assert_eq!(route(&mut state, &announce), (None, Vec::new()));
        }
        assert!(state.announcing, "an announce is not data");
    }

    #[test]
    fn test_malformed_bodies_are_skipped() {
        let mut state = watched();
        for bad in [
            vec![PacketTy::Msg as u8, 1],
            vec![0xff, 1, 2, 0, 9],
            body(PacketTy::MsgWithTimestamp, [9, 9], &[0; 3]),
            body(PacketTy::TimeSeries, [9, 9], &[0; 8]),
        ] {
            assert_eq!(route(&mut state, &bad), (None, Vec::new()), "{bad:?}");
        }
        assert!(state.announcing, "a malformed body is not data");
    }

    #[test]
    fn test_duplicate_table_ids_are_announced_together() {
        let mut state = ConnectionState::new(3);
        state.reset(true);
        let tables = [TABLE, OTHER, TABLE];
        let mut missing = Vec::new();
        state.route(&tables, &vtable(OTHER), |id| missing.push(id));
        state.route(&tables, &msg([9, 9], b"probe"), |id| missing.push(id));
        assert_eq!(missing, [TABLE, TABLE], "one report per port");
    }

    #[test]
    fn test_announce_after_data_is_ignored() {
        let mut state = watched();
        route(&mut state, &msg([9, 9], b"probe"));
        assert_eq!(route(&mut state, &vtable(TABLE)), (None, Vec::new()));
        assert!(route(&mut state, &msg([9, 9], b"probe")).1.is_empty());
    }

    #[test]
    fn test_malformed_vtable_announces_nothing() {
        let mut state = watched();
        route(&mut state, &msg(VTableMsg::ID, &[0xff]));
        assert_eq!(route(&mut state, &msg([9, 9], b"probe")).1, TABLES);
    }
}
