//! Record impls for the sequence messages from metor-proto-wkt.
//!
//! They are postcard records, so each gets the packet id the panel expects.

use metor_proto_wkt::{SequenceCommand, SequenceRegistry};

use crate::record::{DecodeError, EncodeError, Payload, Record, RecordSchema, postcard};

impl Payload for SequenceCommand {
    type Read<'a> = Self;

    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        postcard::encode(self, buf)
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        postcard::decode(bytes)
    }
}

impl Record for SequenceCommand {
    const NAME: &'static str = "sequence_command";
    const MAX_LEN: usize = 256;
    const DEPTH: usize = 4;

    fn schema_with_root(_root: &str) -> RecordSchema {
        RecordSchema::postcard::<Self>(Self::NAME)
    }
}

impl Payload for SequenceRegistry {
    type Read<'a> = Self;

    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        postcard::encode(self, buf)
    }

    fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        postcard::decode(bytes)
    }
}

impl Record for SequenceRegistry {
    const NAME: &'static str = "sequence_registry";
    /// A registry whose encoding is longer than this fails to write.
    const MAX_LEN: usize = 16 * 1024;

    fn schema_with_root(_root: &str) -> RecordSchema {
        RecordSchema::postcard::<Self>(Self::NAME)
    }
}
