use std::collections::HashMap;

use metor_proto::types::{
    ComponentId, IntoLenPacket, PACKET_HEADER_LEN, PacketId, PacketTy, table_id,
};
use metor_proto::vtable::{Op, VTable};
use metor_proto_wkt::{
    ComponentMetadata, LINK_PROTOCOL_VERSION, LinkInfo, MsgMetadata, SetComponentMetadata,
    SetMsgMetadata, VTableMsg,
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

/// The message that is sent at the start of a connection
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
        let (vtable, metadata) = reroot(vtable, metadata, &root(namespace, &def.name), &def.record);
        let id = table_id(&vtable);
        if tables.contains(&id) {
            continue;
        }
        tables.push(id);
        ids[at] = Some(id);
        blob.extend_from_slice(&VTableMsg { id, vtable }.into_len_packet().inner);
        for component in metadata {
            let packet = (&SetComponentMetadata(component)).into_len_packet();
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

/// Where a port's components hang: the namespace, then the port's name.
fn root(namespace: Option<&str>, port: &str) -> String {
    match namespace {
        Some(namespace) => format!("{namespace}.{port}"),
        None => port.to_string(),
    }
}

/// Moves a port-relative frame under `root`, so two links announce one
/// producer's components under one set of ids.
///
/// A port's leaves are named `{record}.{field}`; the announced leaf is
/// `{root}.{field}`. Each leaf id is baked as a standalone eight-byte
/// `Op::Data` blob, so rewriting those blobs is the whole rehash.
fn reroot(
    vtable: &VTable,
    metadata: &[ComponentMetadata],
    root: &str,
    record: &str,
) -> (VTable, Vec<ComponentMetadata>) {
    let renamed: Vec<(ComponentMetadata, u64)> = metadata
        .iter()
        .map(|component| {
            let name = rename(&component.name, root, record);
            let was = ComponentId::new(&component.name).0;
            (
                ComponentMetadata {
                    component_id: ComponentId::new(&name),
                    name,
                    metadata: component.metadata.clone(),
                },
                was,
            )
        })
        .collect();
    let ids: HashMap<u64, u64> = renamed
        .iter()
        .map(|(component, was)| (*was, component.component_id.0))
        .collect();
    let mut vtable = vtable.clone();
    let rewrites = leaf_ids(&vtable, &ids);
    if !rewrites.is_empty() {
        let mut data = vtable.data.to_vec();
        for (offset, id) in rewrites {
            data[offset..offset + 8].copy_from_slice(&id.to_le_bytes());
        }
        vtable.data = data;
    }
    (vtable, renamed.into_iter().map(|(c, _)| c).collect())
}

/// The leaf name under `root`, with the record's own segment dropped.
fn rename(name: &str, root: &str, record: &str) -> String {
    match name
        .strip_prefix(record)
        .and_then(|rest| rest.strip_prefix('.'))
    {
        Some(field) => format!("{root}.{field}"),
        None => format!("{root}.{name}"),
    }
}

/// Every eight-byte `Op::Data` blob holding a leaf id, and what to write there.
fn leaf_ids(vtable: &VTable, ids: &HashMap<u64, u64>) -> Vec<(usize, u64)> {
    let data = vtable.data.as_slice();
    vtable
        .ops
        .iter()
        .filter_map(|op| {
            let Op::Data { offset, len } = op else {
                return None;
            };
            let at = offset.to_index();
            let slot = data.get(at..at.checked_add(*len as usize)?)?;
            let bytes: [u8; 8] = slot.try_into().ok()?;
            Some((at, *ids.get(&u64::from_le_bytes(bytes))?))
        })
        .collect()
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

    use crate::Record;
    use crate::port::Input;
    use crate::record::MsgCodec;
    use crate::tests::utils::{Fixed, Imu};

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

    fn imu_port() -> PortDef {
        Input::<Imu>::def("plant.imu")
    }

    fn announced(defs: &[PortDef]) -> (Vec<u8>, Vec<Option<PacketId>>) {
        announce(defs, Some("cube_sat"), "pub")
    }

    #[test]
    fn test_announce_frame_and_message() {
        let defs = vec![imu_port(), Input::<Fixed>::def("cmds.fixed")];
        let (blob, wire) = announced(&defs);

        let RecordSchema::Frame { vtable, metadata } = &defs[0].schema else {
            panic!("a frame port")
        };
        let (vtable, metadata) = reroot(vtable, metadata, "cube_sat.plant.imu", Imu::NAME);
        let id = table_id(&vtable);
        let info = LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids: Vec::new(),
            namespace: Some("cube_sat".into()),
            link: "pub".into(),
        };
        let mut expected = (&info).into_len_packet().inner;
        expected.extend_from_slice(&VTableMsg { id, vtable }.into_len_packet().inner);
        for component in metadata {
            expected.extend_from_slice(&(&SetComponentMetadata(component)).into_len_packet().inner);
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
    fn test_frame_leaf_paths() {
        let def = imu_port();
        let RecordSchema::Frame { vtable, metadata } = &def.schema else {
            panic!("a frame port")
        };
        let (vtable, metadata) = reroot(vtable, metadata, "cube_sat.plant.imu", Imu::NAME);
        let leaf = ComponentId::new("cube_sat.plant.imu.sample");
        assert_eq!(metadata[0].name, "cube_sat.plant.imu.sample");
        assert_eq!(metadata[0].component_id, leaf);
        assert!(vtable.data.windows(8).any(|w| w == leaf.0.to_le_bytes()));
        assert_ne!(table_id(&vtable), def.schema.packet_id());
    }

    #[test]
    fn test_announce_json_codec() {
        let defs = vec![Input::<Telecommand>::def("cmds.telecommand")];
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
            Input::<Fixed>::def("a.fixed"),
            Input::<Fixed>::def("b.fixed"),
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
