use std::collections::HashMap;

use metor_proto::types::{IntoLenPacket, PACKET_HEADER_LEN, PacketId, PacketTy, table_id};
use metor_proto_wkt::{
    LINK_PROTOCOL_VERSION, LinkInfo, MsgMetadata, SetComponentMetadata, SetMsgMetadata, VTableMsg,
};

use crate::record::{MsgCodec, RecordSchema};
use crate::system::PortDef;

/// Encodes a `LinkInfo` packet.
///
/// A link sends this first on every connection to identify itself.
pub(crate) fn link_info(
    command_ids: Vec<PacketId>,
    namespace: Option<&str>,
    link: &str,
) -> Vec<u8> {
    let info = LinkInfo {
        protocol_version: LINK_PROTOCOL_VERSION,
        features: 0,
        command_ids,
        namespace: namespace.map(str::to_string),
        link: link.to_string(),
    };
    (&info).into_len_packet().inner
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
    let mut ids = vec![[0; 2]; defs.len()];
    for (index, def) in defs.iter().enumerate() {
        let RecordSchema::Frame { vtable, metadata } = &def.schema else {
            continue;
        };
        let id = table_id(vtable);
        ids[index] = id;
        let table = VTableMsg {
            id,
            vtable: vtable.clone(),
        };
        blob.extend_from_slice(&table.into_len_packet().inner);
        for component in metadata {
            let packet = SetComponentMetadata(component.clone()).into_len_packet();
            blob.extend_from_slice(&packet.inner);
        }
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

/// The length prefix and header of a packet carrying `payload_len` bytes.
pub(crate) fn packet_head(ty: PacketTy, id: PacketId, payload_len: usize) -> [u8; PACKET_OVERHEAD] {
    let [a, b, c, d] = ((PACKET_HEADER_LEN + payload_len) as u32).to_le_bytes();
    [a, b, c, d, ty as u8, id[0], id[1], 0]
}

#[cfg(test)]
mod tests {
    use metor_proto::types::{LenPacket, Msg};

    use crate::port::Input;
    use crate::record::MsgCodec;
    use crate::tests::utils::{Fixed, Imu};
    use crate::{DefCx, Payload, Record};

    use super::*;

    fn append_packet(batch: &mut Vec<u8>, ty: PacketTy, id: PacketId, payload: &[u8]) {
        batch.extend_from_slice(&packet_head(ty, id, payload.len()));
        batch.extend_from_slice(payload);
    }

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
            ids.push([body[1], body[2]]);
            rest = &body[u32::from_le_bytes(*len) as usize..];
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

    #[test]
    fn test_packet_framing_matches_len_packet() {
        let mut batch = Vec::new();
        append_packet(&mut batch, PacketTy::Table, [7, 9], b"body");
        let mut expected = LenPacket::table([7, 9], 4);
        expected.extend_from_slice(b"body");
        assert_eq!(batch, expected.into_len_packet().inner);
    }

    #[test]
    fn test_frame_empty_payload() {
        let mut batch = Vec::new();
        append_packet(&mut batch, PacketTy::Msg, [0, 1], b"");
        assert_eq!(batch, vec![4, 0, 0, 0, PacketTy::Msg as u8, 0, 1, 0]);
    }

    #[test]
    fn test_append_packets() {
        let mut batch = Vec::new();
        append_packet(&mut batch, PacketTy::Msg, [0, 1], b"a");
        append_packet(&mut batch, PacketTy::Msg, [0, 2], b"bb");
        assert_eq!(batch.len(), 8 + 1 + 8 + 2);
        assert_eq!(&batch[8..9], b"a");
    }
}
