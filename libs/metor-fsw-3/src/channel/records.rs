//! Record impls for the sequence messages from metor-proto-wkt.
//!
//! They are postcard records, so each gets the packet id the panel expects.

use metor_proto_wkt::{SequenceChannelEvent, SequenceCommand, SequenceRegistry};

use crate::record::PostcardRecord;

impl PostcardRecord for SequenceCommand {
    const NAME: &'static str = "sequence_command";
    const MAX_LEN: usize = 256;
    const DEPTH: usize = 4;
}

impl PostcardRecord for SequenceChannelEvent {
    const NAME: &'static str = "sequence_channel_event";
    const MAX_LEN: usize = 1024;
    const DEPTH: usize = 4;
}

impl PostcardRecord for SequenceRegistry {
    const NAME: &'static str = "sequence_registry";
    /// A registry whose encoding is longer than this fails to write.
    const MAX_LEN: usize = 16 * 1024;
}
