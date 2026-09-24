//! The metor-proto-wkt sequence messages as records.
//!
//! Each is postcard, so its packet id is the one the panel matches on.

use metor_proto_wkt::{SequenceChannelEvent, SequenceCommand, SequenceRegistry};

use crate::record::{DecodeError, EncodeError, Record, RecordSchema, postcard};

impl Record for SequenceCommand {
    const NAME: &'static str = "sequence_command";
    const MAX_LEN: usize = 256;
    const DEPTH: usize = 4;
    type Read<'a> = SequenceCommand;

    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        postcard::encode(self, buf)
    }

    fn decode(bytes: &[u8]) -> Result<SequenceCommand, DecodeError> {
        postcard::decode(bytes)
    }

    fn schema() -> RecordSchema {
        RecordSchema::postcard::<SequenceCommand>(Self::NAME)
    }
}

impl Record for SequenceChannelEvent {
    const NAME: &'static str = "sequence_channel_event";
    const MAX_LEN: usize = 1024;
    const DEPTH: usize = 4;
    type Read<'a> = SequenceChannelEvent;

    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        postcard::encode(self, buf)
    }

    fn decode(bytes: &[u8]) -> Result<SequenceChannelEvent, DecodeError> {
        postcard::decode(bytes)
    }

    fn schema() -> RecordSchema {
        RecordSchema::postcard::<SequenceChannelEvent>(Self::NAME)
    }
}

impl Record for SequenceRegistry {
    const NAME: &'static str = "sequence_registry";
    /// A registry that encodes past this is refused where it is built.
    const MAX_LEN: usize = 16 * 1024;
    type Read<'a> = SequenceRegistry;

    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        postcard::encode(self, buf)
    }

    fn decode(bytes: &[u8]) -> Result<SequenceRegistry, DecodeError> {
        postcard::decode(bytes)
    }

    fn schema() -> RecordSchema {
        RecordSchema::postcard::<SequenceRegistry>(Self::NAME)
    }
}
