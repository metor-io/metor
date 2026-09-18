//! The [`Record`] trait, which names a ring record and encodes it.

use core::borrow::Borrow;

use metor_proto::types::{ComponentId, Msg, PacketId, Timestamp, msg_id, table_id};
use metor_proto::vtable::VTable;
use metor_proto_wkt::ComponentMetadata;
use postcard_schema::schema::owned::OwnedNamedType;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::frame::Frame;

/// A `Record` is an entry on a ring buffer between systems.
///
/// Records have unique names and IDs, and provide a way to convert themselves to and from bytes
pub trait Record {
    const NAME: &'static str;
    const ID: ComponentId = ComponentId::new(Self::NAME);
    /// Largest record this type writes.
    const MAX_LEN: usize;
    const ALIGN: usize = 1;
    /// Writes per cycle this record expects; a ring holds `DEPTH * ring_depth` records.
    const DEPTH: usize = 1;
    /// What a read yields: a borrow for a fixed record, an owned value otherwise.
    type Read<'a>: Borrow<Self>
    where
        Self: 'a;

    /// Returns the stamp fan-in orders by; `None` leaves producer order.
    fn timestamp(&self) -> Option<Timestamp> {
        None
    }

    /// Encodes this value into buf, returns a slice of the encoded bytes.
    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError>;

    /// Reads one record from the given bytes.
    fn decode(bytes: &[u8]) -> Result<Self::Read<'_>, DecodeError>;

    /// What a port carrying this record announces to the ground.
    fn schema() -> RecordSchema;
}

/// What a port announces about the records it carries.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RecordSchema {
    Frame {
        vtable: VTable,
        metadata: Vec<ComponentMetadata>,
    },
    Msg {
        id: PacketId,
        name: String,
        codec: MsgCodec,
    },
}

/// How a message record's bytes are encoded, so the ground can decode them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MsgCodec {
    Postcard(OwnedNamedType),
    Json,
    Bytes,
    Other(String),
}

impl RecordSchema {
    /// A frame's vtable and its components, both relative to the port.
    pub fn frame<T: Frame>() -> Self {
        Self::Frame {
            vtable: T::as_vtable(),
            metadata: T::metadata(()).collect(),
        }
    }

    /// A postcard message, whose id is the hash of its schema name.
    pub fn postcard<T: Serialize + postcard_schema::Schema>(name: &str) -> Self {
        Self::Msg {
            id: <T as Msg>::ID,
            name: name.to_string(),
            codec: MsgCodec::Postcard(T::SCHEMA.into()),
        }
    }

    /// A message under any other codec, whose id is the hash of its record name.
    pub fn msg(name: &str, codec: MsgCodec) -> Self {
        Self::Msg {
            id: msg_id(name),
            name: name.to_string(),
            codec,
        }
    }

    /// The packet id the ground matches this record on.
    pub fn packet_id(&self) -> PacketId {
        match self {
            Self::Frame { vtable, .. } => table_id(vtable),
            Self::Msg { id, .. } => *id,
        }
    }
}

/// Frames compare by their announced bytes, which is what the ground sees.
impl PartialEq for RecordSchema {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Frame { vtable, metadata },
                Self::Frame {
                    vtable: other_vtable,
                    metadata: other_metadata,
                },
            ) => metadata == other_metadata && vtable_bytes(vtable) == vtable_bytes(other_vtable),
            (
                Self::Msg { id, name, codec },
                Self::Msg {
                    id: other_id,
                    name: other_name,
                    codec: other_codec,
                },
            ) => id == other_id && name == other_name && codec == other_codec,
            _ => false,
        }
    }
}

impl Eq for RecordSchema {}

/// A vtable's postcard bytes; an owned vtable always encodes.
fn vtable_bytes(vtable: &VTable) -> Vec<u8> {
    ::postcard::to_allocvec(vtable).unwrap_or_default()
}

/// An error that occured while encoding
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum EncodeError {
    #[error("value encodes to {len} bytes, more than the record's {max}")]
    Oversize { len: usize, max: usize },
    #[error("value cannot be encoded")]
    Codec,
}

/// An error that occured while decoding
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum DecodeError {
    #[error("record is shorter than the type")]
    Truncated,
    #[error("record bytes do not decode")]
    Codec,
}

/// Decoding for fixed records whose bytes are the value itself.
pub mod fixed {
    use zerocopy::{FromBytes, Immutable, KnownLayout};

    use super::DecodeError;

    /// Reads the fixed prefix of a record as `T`.
    pub fn decode<T: FromBytes + KnownLayout + Immutable>(bytes: &[u8]) -> Result<&T, DecodeError> {
        T::ref_from_prefix(bytes)
            .map(|(value, _)| value)
            .map_err(|_| DecodeError::Truncated)
    }
}

/// Encoding for records carried as postcard.
pub mod postcard {
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use super::{DecodeError, EncodeError};

    /// Serializes `value` into `buf` and returns the bytes it filled.
    pub fn encode<'a, T: Serialize + ?Sized>(
        value: &T,
        buf: &'a mut [u8],
    ) -> Result<&'a [u8], EncodeError> {
        let max = buf.len();
        match ::postcard::to_slice(value, buf) {
            Ok(used) => Ok(used),
            Err(::postcard::Error::SerializeBufferFull) => Err(EncodeError::Oversize {
                len: ::postcard::experimental::serialized_size(value).unwrap_or(usize::MAX),
                max,
            }),
            Err(_) => Err(EncodeError::Codec),
        }
    }

    /// Deserializes one value from `bytes`.
    pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
        ::postcard::from_bytes(bytes).map_err(|_| DecodeError::Codec)
    }
}

/// Encoding for records carried as serde JSON.
pub mod json {
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use super::{DecodeError, EncodeError};

    /// Serializes `value` into `buf` and returns the bytes it filled.
    pub fn encode<'a, T: Serialize + ?Sized>(
        value: &T,
        buf: &'a mut [u8],
    ) -> Result<&'a [u8], EncodeError> {
        let mut out = std::io::Cursor::new(&mut *buf);
        serde_json::to_writer(&mut out, value).map_err(|e| match e.is_io() {
            true => EncodeError::Oversize {
                len: out.position() as usize,
                max: out.get_ref().len(),
            },
            false => EncodeError::Codec,
        })?;
        let len = out.position() as usize;
        Ok(&buf[..len])
    }

    /// Deserializes one value from `bytes`.
    pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
        serde_json::from_slice(bytes).map_err(|_| DecodeError::Codec)
    }
}

/// Encoding for records whose payload is already bytes.
pub mod bytes {
    use super::{DecodeError, EncodeError};

    /// Returns `value` unchanged.
    pub fn encode(value: &[u8]) -> Result<&[u8], EncodeError> {
        Ok(value)
    }

    /// Returns `bytes` unchanged.
    pub fn decode(bytes: &[u8]) -> Result<&[u8], DecodeError> {
        Ok(bytes)
    }
}

/// `Bytes` is the record of a dynamic port: a payload copied without decoding.
///
/// It is never announced; a dynamic port announces the schema of its edge.
pub type Bytes = [u8];

impl Record for Bytes {
    const NAME: &'static str = "bytes";
    const MAX_LEN: usize = 0;
    type Read<'a> = &'a [u8];

    fn encode<'a>(&'a self, _buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        bytes::encode(self)
    }

    fn decode(bytes: &[u8]) -> Result<&[u8], DecodeError> {
        bytes::decode(bytes)
    }

    fn schema() -> RecordSchema {
        RecordSchema::Msg {
            id: [0, 0],
            name: Self::NAME.to_string(),
            codec: MsgCodec::Bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use metor_proto::types::ComponentId;
    use metor_proto_wkt::LogEvent;
    use serde::{Deserialize, Serialize};
    use zerocopy::IntoBytes;

    use super::*;
    use crate::MaxSize;
    use crate::tests::utils::{Fixed, Imu, Note};

    /// A message whose codec is JSON, written by hand as a pack author would.
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct NoteJson {
        text: String,
    }

    impl Record for NoteJson {
        const NAME: &'static str = "note_json";
        const MAX_LEN: usize = 64;
        type Read<'a> = Self;

        fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
            json::encode(self, buf)
        }

        fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
            json::decode(bytes)
        }

        fn schema() -> RecordSchema {
            RecordSchema::msg(Self::NAME, MsgCodec::Json)
        }
    }

    fn component_names(schema: &RecordSchema) -> Vec<&str> {
        let RecordSchema::Frame { metadata, .. } = schema else {
            panic!("a frame announces a vtable")
        };
        metadata.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn a_frame_announces_its_leaves_relative_to_the_port() {
        let schema = Imu::schema();
        assert_eq!(component_names(&schema), vec!["imu.sample"]);
        let RecordSchema::Frame { vtable, metadata } = &schema else {
            panic!("a frame announces a vtable")
        };
        assert_eq!(metadata[0].component_id, ComponentId::new("imu.sample"));
        // The sample plus the timestamp the derive applies to it.
        assert_eq!(vtable.fields.len(), 1);
        assert!(
            vtable
                .ops
                .iter()
                .any(|op| matches!(op, metor_proto::vtable::Op::Timestamp { .. }))
        );
        assert_eq!(schema, Imu::schema());
    }

    #[test]
    fn a_derived_message_announces_its_postcard_schema() {
        let schema = Fixed::schema();
        let RecordSchema::Msg { id, name, codec } = &schema else {
            panic!("a message announces an id")
        };
        assert_eq!(*id, <Fixed as metor_proto::types::Msg>::ID);
        assert_eq!(name, "fixed");
        assert!(matches!(codec, MsgCodec::Postcard(ty) if ty.name == "Fixed"));
        assert_eq!(schema.packet_id(), *id);
        assert_ne!(schema, Note::schema());
    }

    #[test]
    fn a_hand_written_json_record_round_trips_and_announces_its_codec() {
        let mut buf = [0u8; 64];
        let note = NoteJson { text: "hi".into() };
        let bytes = note.encode(&mut buf).expect("fits");
        assert_eq!(bytes, br#"{"text":"hi"}"#);
        assert_eq!(NoteJson::decode(bytes), Ok(note));
        assert_eq!(
            NoteJson::schema(),
            RecordSchema::Msg {
                id: msg_id("note_json"),
                name: "note_json".into(),
                codec: MsgCodec::Json,
            }
        );
    }

    #[test]
    fn an_oversize_json_value_reports_both_lengths() {
        let mut buf = [0u8; 8];
        let note = NoteJson {
            text: "0123456789".into(),
        };
        assert!(matches!(
            note.encode(&mut buf),
            Err(EncodeError::Oversize { max: 8, .. })
        ));
        assert_eq!(NoteJson::decode(b"not json"), Err(DecodeError::Codec));
    }

    /// A postcard message hashes its schema name; every other codec hashes the
    /// record name, so `log` keeps the id the panel already matches on.
    #[test]
    fn a_postcard_id_is_not_the_record_names_hash() {
        assert_eq!(
            LogEvent::schema().packet_id(),
            <LogEvent as metor_proto::types::Msg>::ID
        );
        assert_ne!(LogEvent::schema().packet_id(), msg_id("log"));
    }

    #[test]
    fn the_dynamic_port_record_carries_bytes_unchanged() {
        assert_eq!(<Bytes as Record>::NAME, "bytes");
        assert_eq!(<Bytes as Record>::MAX_LEN, 0);
        assert_eq!(Bytes::decode(b"raw"), Ok(&b"raw"[..]));
        assert_eq!(b"raw"[..].encode(&mut []), Ok(&b"raw"[..]));
        assert!(matches!(
            <Bytes as Record>::schema(),
            RecordSchema::Msg {
                id: [0, 0],
                codec: MsgCodec::Bytes,
                ..
            }
        ));
    }

    #[test]
    fn derived_message_infers_name_and_length() {
        assert_eq!(Fixed::NAME, "fixed");
        assert_eq!(<Fixed as Record>::ID, ComponentId::new("fixed"));
        assert_eq!(Fixed::MAX_LEN, Fixed::POSTCARD_MAX_SIZE);
        assert_eq!(Fixed::DEPTH, 1);
        assert_eq!(Note::NAME, "note");
        assert_eq!(Note::MAX_LEN, 64);
        assert_eq!(Note::DEPTH, 4);
    }

    #[test]
    fn message_round_trips() {
        let mut buf = [0u8; 64];
        let value = Fixed { a: 7, b: 2.5 };
        let bytes = value.encode(&mut buf).expect("fits");
        assert!(bytes.len() <= Fixed::MAX_LEN);
        assert_eq!(Fixed::decode(bytes), Ok(value));
    }

    #[test]
    fn oversize_message_reports_both_lengths() {
        let mut buf = [0u8; 8];
        let note = Note {
            text: "0123456789".into(),
        };
        assert_eq!(
            note.encode(&mut buf),
            Err(EncodeError::Oversize { len: 11, max: 8 })
        );
    }

    #[test]
    fn garbage_message_bytes_are_a_codec_error() {
        assert_eq!(Fixed::decode(&[0xff; 3]), Err(DecodeError::Codec));
    }

    #[test]
    fn frame_encodes_as_its_own_bytes() {
        let imu = Imu::new(3, 1.5);
        let mut buf = [0xaau8; 4];
        let bytes = imu.encode(&mut buf).expect("frames always encode");
        assert_eq!(bytes, imu.as_bytes());
        assert_eq!(buf, [0xaa; 4]);
        assert_eq!(Imu::MAX_LEN, size_of::<Imu>());
        assert_eq!(Imu::ALIGN, align_of::<Imu>());
    }

    #[test]
    fn frame_decodes_by_reference_and_rejects_short_bytes() {
        let imu = Imu::new(3, 1.5);
        assert_eq!(Imu::decode(imu.as_bytes()), Ok(&imu));
        assert_eq!(
            Imu::decode(&imu.as_bytes()[..4]),
            Err(DecodeError::Truncated)
        );
    }
}
