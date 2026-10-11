//! The [`Record`] trait, which names a ring record and encodes it.

use core::borrow::Borrow;

use metor_proto::types::{ComponentId, Timestamp};
use thiserror::Error;

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

#[cfg(test)]
mod tests {
    use metor_proto::types::ComponentId;
    use zerocopy::IntoBytes;

    use super::*;
    use crate::MaxSize;
    use crate::tests::utils::{Fixed, Imu, Note};

    #[test]
    fn derived_message_infers_name_and_length() {
        assert_eq!(Fixed::NAME, "fixed");
        assert_eq!(Fixed::ID, ComponentId::new("fixed"));
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
