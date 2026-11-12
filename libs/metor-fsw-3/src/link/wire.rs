use std::collections::HashMap;

use metor_proto::types::{
    IntoLenPacket, PACKET_HEADER_LEN, PacketHeader, PacketId, PacketTy, table_id,
};
use metor_proto::vtable::{Field, Op, VTable};
use metor_proto_wkt::{
    CommandInfo, CommandKind, ComponentMetadata, LINK_PROTOCOL_VERSION, LinkInfo, MsgMetadata,
    MsgStream, SetComponentMetadata, SetMsgMetadata, VTableMsg,
};
use zerocopy::IntoBytes;

use crate::record::{MsgCodec, RecordSchema};
use crate::system::PortDef;

/// Encodes a `LinkInfo` packet that takes `commands`.
///
/// A link sends this first on every connection to identify itself, with the
/// commands it takes described so a ground tool can build them.
pub(crate) fn link_info(
    commands: Vec<CommandInfo>,
    namespace: Option<&str>,
    link: &str,
) -> Vec<u8> {
    let info = LinkInfo {
        protocol_version: LINK_PROTOCOL_VERSION,
        features: 0,
        command_ids: commands.iter().map(|command| command.id).collect(),
        namespace: namespace.map(str::to_string),
        link: link.to_string(),
        commands,
    };
    (&info).into_len_packet().inner
}

/// Encodes what a listening `Subscribe` sends each new peer: a `LinkInfo`
/// that takes every port as a command, then each frame port's vtable and
/// component metadata, so the ground can name a frame command's fields.
pub(crate) fn command_announce<'a>(
    defs: impl Iterator<Item = &'a PortDef>,
    namespace: Option<&str>,
    link: &str,
) -> Vec<u8> {
    let defs: Vec<&PortDef> = defs.collect();
    let commands = defs.iter().map(|def| def.schema.command(&def.record));
    let mut blob = link_info(commands.collect(), namespace, link);
    for def in defs {
        if let RecordSchema::Frame { vtable, metadata } = &def.schema {
            announce_frame(vtable, metadata, &mut blob);
        }
    }
    blob
}

/// Encodes what a dialing `Subscribe` asks its peer for: a `MsgStream`
/// for each port's id, frame ports first.
///
/// A db answers a frame command's request with its vtable at once, so
/// that announce precedes any record a message stream sends. A `Publish`
/// ignores them.
pub(crate) fn stream_requests<'a>(defs: impl Iterator<Item = &'a PortDef>) -> Vec<u8> {
    let (mut frames, mut msgs) = (Vec::new(), Vec::new());
    for def in defs {
        let request = (&MsgStream {
            msg_id: def.schema.packet_id(),
        })
            .into_len_packet()
            .inner;
        match def.schema {
            RecordSchema::Frame { .. } => frames.extend_from_slice(&request),
            RecordSchema::Msg { .. } => msgs.extend_from_slice(&request),
        }
    }
    frames.extend_from_slice(&msgs);
    frames
}

/// Appends a frame's vtable and component metadata to `blob`.
fn announce_frame(
    vtable: &VTable<Vec<Op>, Vec<u8>, Vec<Field>>,
    metadata: &[ComponentMetadata],
    blob: &mut Vec<u8>,
) {
    let table = VTableMsg {
        id: table_id(vtable),
        vtable: vtable.clone(),
    };
    blob.extend_from_slice(&table.into_len_packet().inner);
    for component in metadata {
        let packet = SetComponentMetadata(component.clone()).into_len_packet();
        blob.extend_from_slice(&packet.inner);
    }
}

/// Encodes the announce a `Publish` sends on every new connection.
///
/// Also returns the packet id each port's records are sent under. The
/// announce is a `LinkInfo`, then each frame port's vtable and component
/// metadata, then each message port's metadata. Frame ports have distinct
/// table ids; `Publish`'s definition rejects a collision.
pub(crate) fn announce(
    defs: &[PortDef],
    namespace: Option<&str>,
    link: &str,
) -> (Vec<u8>, Vec<PacketId>) {
    let mut blob = link_info(Vec::new(), namespace, link);
    let mut ids = vec![[0; 8]; defs.len()];
    for (index, def) in defs.iter().enumerate() {
        let RecordSchema::Frame { vtable, metadata } = &def.schema else {
            continue;
        };
        ids[index] = table_id(vtable);
        announce_frame(vtable, metadata, &mut blob);
    }
    let mut msgs: Vec<PacketId> = Vec::new();
    for (index, def) in defs.iter().enumerate() {
        let RecordSchema::Msg { id, name, codec } = &def.schema else {
            continue;
        };
        ids[index] = *id;
        if msgs.contains(id) {
            continue;
        }
        msgs.push(*id);
        let announce = SetMsgMetadata {
            id: *id,
            metadata: codec.msg_metadata(name),
        };
        blob.extend_from_slice(&announce.into_len_packet().inner);
    }
    (blob, ids)
}

impl RecordSchema {
    /// Describes this schema as a command, for a record called `record`.
    ///
    /// A frame command's id is its table id.
    pub(crate) fn command(&self, record: &str) -> CommandInfo {
        match self {
            Self::Frame { vtable, .. } => CommandInfo {
                id: table_id(vtable),
                kind: CommandKind::Frame {
                    name: record.to_string(),
                    vtable: vtable.clone(),
                },
            },
            Self::Msg { id, name, codec } => CommandInfo {
                id: *id,
                kind: CommandKind::Msg(codec.msg_metadata(name)),
            },
        }
    }
}

impl MsgCodec {
    /// Builds the metadata the ground uses to decode a message called `name`.
    ///
    /// A postcard message uses the type's own schema. Any other codec is
    /// described as an opaque byte payload, tagged with the codec's name.
    pub(crate) fn msg_metadata(&self, name: &str) -> MsgMetadata {
        let (name, schema) = match self {
            Self::Postcard(schema) => (schema.name.to_string(), schema.clone()),
            _ => (
                name.to_string(),
                <Vec<u8> as postcard_schema::Schema>::SCHEMA.into(),
            ),
        };
        let codec = match self {
            Self::Postcard(_) => None,
            Self::Json => Some("json".to_string()),
            Self::Bytes => Some("bytes".to_string()),
            Self::Other(name) => Some(name.clone()),
        };
        MsgMetadata {
            name,
            schema,
            metadata: codec
                .map(|codec| HashMap::from([("codec".to_string(), codec)]))
                .unwrap_or_default(),
        }
    }
}

/// The bytes a packet adds on top of its payload: the length prefix and the
/// header.
pub(crate) const PACKET_OVERHEAD: usize = 4 + PACKET_HEADER_LEN;

/// Whether a packet carrying up to `max_len` bytes fits a `conn_cap`-byte
/// buffer.
pub(crate) fn fits(max_len: usize, conn_cap: usize) -> bool {
    max_len
        .checked_add(PACKET_OVERHEAD)
        .is_some_and(|n| n <= conn_cap)
}

/// The length prefix and header of a packet carrying `payload_len` bytes.
pub(crate) fn packet_head(ty: PacketTy, id: PacketId, payload_len: usize) -> [u8; PACKET_OVERHEAD] {
    let mut head = [0; PACKET_OVERHEAD];
    head[..4].copy_from_slice(&((PACKET_HEADER_LEN + payload_len) as u32).to_le_bytes());
    head[4..].copy_from_slice(PacketHeader::new(ty, id, 0).as_bytes());
    head
}

/// A packet body in its wire format, without the length prefix.
#[cfg(test)]
pub(crate) fn body(ty: PacketTy, id: PacketId, payload: &[u8]) -> Vec<u8> {
    let mut body = packet_head(ty, id, payload.len())[size_of::<u32>()..].to_vec();
    body.extend_from_slice(payload);
    body
}

#[cfg(test)]
mod tests {
    use metor_proto::types::{LenPacket, Msg};

    use crate::port::Input;
    use crate::record::MsgCodec;
    use crate::tests::utils::{Fixed, Imu};
    use crate::{DefCx, Payload, Record};

    use super::super::peer::packet;
    use super::*;

    /// A record encoded as JSON, so its metadata names a codec.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Telecommand {
        arm: bool,
    }

    impl Payload for Telecommand {
        type Read<'a> = Self;

        fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], crate::EncodeError> {
            crate::record::json::encode(self, buf)
        }

        fn decode(bytes: &[u8]) -> Result<Self, crate::DecodeError> {
            crate::record::json::decode(bytes)
        }
    }

    impl Record for Telecommand {
        const NAME: &'static str = "telecommand";
        const MAX_LEN: usize = 16;

        fn schema_with_root(_root: &str) -> RecordSchema {
            RecordSchema::msg(Self::NAME, MsgCodec::Json)
        }
    }

    /// Returns the context for instance `plant` on target `cube_sat`.
    fn plant() -> DefCx<'static> {
        DefCx {
            id: "plant",
            namespace: Some("cube_sat"),
            ..DefCx::empty()
        }
    }

    fn imu_port() -> PortDef {
        Input::<Imu>::def(&plant(), "imu")
    }

    fn announced(defs: &[PortDef]) -> (Vec<u8>, Vec<PacketId>) {
        announce(defs, Some("cube_sat"), "pub")
    }

    #[test]
    fn test_announce_frame_and_message() {
        let defs = vec![
            imu_port(),
            Input::<Fixed>::def(&DefCx::empty(), "cmds.fixed"),
        ];
        let (blob, wire) = announced(&defs);
        let RecordSchema::Frame { vtable, metadata } = &defs[0].schema else {
            panic!("a frame port")
        };
        assert_eq!(metadata[0].name, "cube_sat.plant.imu.sample");
        assert_eq!(wire, vec![table_id(vtable), Fixed::schema().packet_id()]);

        let mut ids = Vec::new();
        let mut rest = &blob[..];
        while let Some((len, body)) = rest.split_first_chunk::<4>() {
            let len = u32::from_le_bytes(*len) as usize;
            ids.push(packet(&body[..len]).expect("a packet").1);
            rest = &body[len..];
        }
        let mut expected = vec![LinkInfo::ID, VTableMsg::ID];
        expected.extend(metadata.iter().map(|_| SetComponentMetadata::ID));
        expected.push(SetMsgMetadata::ID);
        assert_eq!(ids, expected);
    }

    #[test]
    fn test_announce_json_codec() {
        let defs = vec![Input::<Telecommand>::def(
            &DefCx::empty(),
            "cmds.telecommand",
        )];
        let (blob, wire) = announced(&defs);
        let metadata = MsgCodec::Json.msg_metadata("telecommand");
        assert_eq!(
            metadata.metadata.get("codec").map(String::as_str),
            Some("json")
        );
        assert_eq!(
            metadata.schema,
            <Vec<u8> as postcard_schema::Schema>::SCHEMA.into()
        );
        assert_eq!(metadata.name, "telecommand");
        let announce = SetMsgMetadata {
            id: wire[0],
            metadata,
        };
        assert!(blob.ends_with(&announce.into_len_packet().inner));
    }

    #[test]
    fn test_shared_message_id_routes_both_ports() {
        let defs = vec![
            Input::<Fixed>::def(&DefCx::empty(), "a.fixed"),
            Input::<Fixed>::def(&DefCx::empty(), "b.fixed"),
        ];
        let (blob, wire) = announced(&defs);
        assert_eq!(wire[0], wire[1]);
        assert_eq!(blob, announced(&defs[..1]).0);
    }

    /// Splits a blob of length-prefixed packets into each one's id and
    /// payload.
    fn packets(blob: &[u8]) -> Vec<(PacketId, &[u8])> {
        let mut packets = Vec::new();
        let mut rest = blob;
        while let Some((len, body)) = rest.split_first_chunk::<4>() {
            let len = u32::from_le_bytes(*len) as usize;
            let (_, id, payload) = packet(&body[..len]).expect("a packet");
            packets.push((id, payload));
            rest = &body[len..];
        }
        packets
    }

    #[test]
    fn test_command_announce_describes_every_port() {
        let defs = [
            Input::<Fixed>::def(&DefCx::empty(), "cmds.fixed"),
            imu_port(),
        ];
        let blob = command_announce(defs.iter(), Some("cube_sat"), "cmds");
        let packets = packets(&blob);
        let info: LinkInfo = postcard::from_bytes(packets[0].1).expect("a link identity");
        let RecordSchema::Frame { vtable, metadata } = &defs[1].schema else {
            panic!("a frame port")
        };
        let table = table_id(vtable);
        assert_eq!(info.command_ids, [Fixed::schema().packet_id(), table]);
        assert_eq!(
            info.commands,
            [
                defs[0].schema.command(Fixed::NAME),
                CommandInfo {
                    id: table,
                    kind: CommandKind::Frame {
                        name: Imu::NAME.into(),
                        vtable: vtable.clone(),
                    },
                },
            ]
        );
        assert!(matches!(info.commands[0].kind, CommandKind::Msg(_)));
        let ids: Vec<PacketId> = packets[1..].iter().map(|(id, _)| *id).collect();
        let mut expected = vec![VTableMsg::ID];
        expected.extend(metadata.iter().map(|_| SetComponentMetadata::ID));
        assert_eq!(ids, expected, "only the frame port's table follows");
    }

    #[test]
    fn test_stream_requests_ask_for_frames_first() {
        let defs = [
            Input::<Fixed>::def(&DefCx::empty(), "cmds.fixed"),
            imu_port(),
        ];
        let blob = stream_requests(defs.iter());
        let asked: Vec<PacketId> = packets(&blob)
            .iter()
            .map(|(id, payload)| {
                assert_eq!(*id, MsgStream::ID);
                let request: MsgStream = postcard::from_bytes(payload).expect("a request");
                request.msg_id
            })
            .collect();
        assert_eq!(
            asked,
            [defs[1].schema.packet_id(), Fixed::schema().packet_id()]
        );
    }

    #[test]
    fn test_packet_head_matches_len_packet() {
        const WIDE: PacketId = [7, 9, 1, 2, 3, 4, 5, 6];
        for ty in [
            PacketTy::Msg,
            PacketTy::Table,
            PacketTy::TimeSeries,
            PacketTy::MsgWithTimestamp,
        ] {
            let mut batch = packet_head(ty, WIDE, 4).to_vec();
            batch.extend_from_slice(b"body");
            let mut expected = LenPacket::new(ty, WIDE, 4);
            expected.extend_from_slice(b"body");
            assert_eq!(batch, expected.into_len_packet().inner);
        }
    }
}
