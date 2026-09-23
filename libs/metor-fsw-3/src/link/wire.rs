use std::collections::HashMap;

use metor_proto::types::{IntoLenPacket, PACKET_HEADER_LEN, PacketId, PacketTy, table_id};
use metor_proto_wkt::{
    LINK_PROTOCOL_VERSION, LinkInfo, MsgMetadata, MsgStream, SetComponentMetadata, SetMsgMetadata,
    VTableMsg,
};

use crate::record::{MsgCodec, RecordSchema};
use crate::system::PortDef;

/// The identity packet a link pushes before anything else.
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

/// What a dialer sends first: one stream request per message it takes, so a
/// db peer forwards those logs. A `Publish` peer discards them.
pub(crate) fn msg_streams(ids: &[PacketId]) -> Vec<u8> {
    let mut blob = Vec::new();
    for &msg_id in ids {
        blob.extend_from_slice(&(&MsgStream { msg_id }).into_len_packet().inner);
    }
    blob
}

/// The message that is sent at the start of a connection, and the id each
/// port's records go out under.
///
/// A frame port announces the vtable and components its def carries, which
/// its type built under the port's root. A port whose table id collides with
/// an earlier one has no id: two tables under one id are indistinguishable on
/// the wire.
pub(crate) fn announce(
    defs: &[PortDef],
    namespace: Option<&str>,
    link: &str,
) -> (Vec<u8>, Vec<Option<PacketId>>) {
    let mut blob = link_info(Vec::new(), namespace, link);
    let mut ids = vec![None; defs.len()];
    let mut tables: Vec<PacketId> = Vec::new();
    for (at, def) in defs.iter().enumerate() {
        let RecordSchema::Frame { vtable, metadata } = &def.schema else {
            continue;
        };
        let id = table_id(vtable);
        if tables.contains(&id) {
            continue;
        }
        tables.push(id);
        ids[at] = Some(id);
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
    for (at, def) in defs.iter().enumerate() {
        let RecordSchema::Msg { id, name, codec } = &def.schema else {
            continue;
        };
        ids[at] = Some(*id);
        if msgs.contains(id) {
            continue;
        }
        msgs.push(*id);
        let announce = SetMsgMetadata {
            id: *id,
            metadata: msg_metadata(name, codec),
        };
        blob.extend_from_slice(&announce.into_len_packet().inner);
    }
    (blob, ids)
}

/// A message's schema as the ground decodes it: the type's own for postcard,
/// an opaque payload plus the codec's name for anything else.
fn msg_metadata(name: &str, codec: &MsgCodec) -> MsgMetadata {
    let (name, schema) = match codec {
        MsgCodec::Postcard(schema) => (schema.name.to_string(), schema.clone()),
        _ => (
            name.to_string(),
            <Vec<u8> as postcard_schema::Schema>::SCHEMA.into(),
        ),
    };
    let codec = match codec {
        MsgCodec::Postcard(_) => None,
        MsgCodec::Json => Some("json".to_string()),
        MsgCodec::Bytes => Some("bytes".to_string()),
        MsgCodec::Other(name) => Some(name.clone()),
    };
    MsgMetadata {
        name,
        schema,
        metadata: codec
            .map(|codec| HashMap::from([("codec".to_string(), codec)]))
            .unwrap_or_default(),
    }
}

/// The bytes one packet adds beyond its payload: the length prefix and header.
pub(crate) const PACKET_OVERHEAD: usize = 4 + PACKET_HEADER_LEN;

/// Appends one length-prefixed packet
pub(crate) fn append_packet(batch: &mut Vec<u8>, ty: PacketTy, id: PacketId, payload: &[u8]) {
    batch.extend_from_slice(&((PACKET_HEADER_LEN + payload.len()) as u32).to_le_bytes());
    batch.push(ty as u8);
    batch.extend_from_slice(&id);
    batch.push(0);
    batch.extend_from_slice(payload);
}

#[cfg(test)]
mod tests {
    use metor_proto::types::LenPacket;

    use crate::port::Input;
    use crate::record::MsgCodec;
    use crate::tests::utils::{Fixed, Imu};
    use crate::{DefCx, Record};

    use super::*;

    /// A record whose payload is JSON, so it announces a codec.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Telecommand {
        arm: bool,
    }

    impl Record for Telecommand {
        const NAME: &'static str = "telecommand";
        const MAX_LEN: usize = 16;
        type Read<'a> = Self;

        fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], crate::EncodeError> {
            crate::record::json::encode(self, buf)
        }

        fn decode(bytes: &[u8]) -> Result<Self, crate::DecodeError> {
            crate::record::json::decode(bytes)
        }

        fn schema() -> RecordSchema {
            RecordSchema::msg(Self::NAME, MsgCodec::Json)
        }
    }

    /// The context of instance `plant` on target `cube_sat`.
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

    fn announced(defs: &[PortDef]) -> (Vec<u8>, Vec<Option<PacketId>>) {
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
        let id = table_id(vtable);
        let info = LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids: Vec::new(),
            namespace: Some("cube_sat".into()),
            link: "pub".into(),
        };
        let mut expected = (&info).into_len_packet().inner;
        let table = VTableMsg {
            id,
            vtable: vtable.clone(),
        };
        expected.extend_from_slice(&table.into_len_packet().inner);
        for component in metadata {
            expected.extend_from_slice(
                &SetComponentMetadata(component.clone())
                    .into_len_packet()
                    .inner,
            );
        }
        let RecordSchema::Msg { id: msg, .. } = &defs[1].schema else {
            panic!("a message port")
        };
        let announce = SetMsgMetadata {
            id: *msg,
            metadata: MsgMetadata {
                name: "Fixed".into(),
                schema: <Fixed as postcard_schema::Schema>::SCHEMA.into(),
                metadata: HashMap::new(),
            },
        };
        expected.extend_from_slice(&announce.into_len_packet().inner);

        assert_eq!(blob, expected);
        assert_eq!(wire, vec![Some(id), Some(*msg)]);
    }

    #[test]
    fn test_announce_json_codec() {
        let defs = vec![Input::<Telecommand>::def(
            &DefCx::empty(),
            "cmds.telecommand",
        )];
        let (blob, wire) = announced(&defs);
        let metadata = msg_metadata("telecommand", &MsgCodec::Json);
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
            id: wire[0].expect("an id"),
            metadata,
        };
        assert!(blob.ends_with(&announce.into_len_packet().inner));
    }

    #[test]
    fn test_deduplicate_table_ports() {
        let defs = vec![imu_port(), imu_port()];
        let (blob, wire) = announced(&defs);
        assert!(wire[0].is_some() && wire[1].is_none());
        let once = announced(&defs[..1]).0;
        assert_eq!(blob, once);
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
    fn test_msg_streams_request_each_id() {
        let blob = msg_streams(&[[1, 2], [3, 4]]);
        let mut expected = (&MsgStream { msg_id: [1, 2] }).into_len_packet().inner;
        expected.extend_from_slice(&(&MsgStream { msg_id: [3, 4] }).into_len_packet().inner);
        assert_eq!(blob, expected);
        assert!(msg_streams(&[]).is_empty());
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
