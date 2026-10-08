//! `io_status`: what each resource's ops have done, written when it changes.

use metor_component::FrameStr;
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Frame;

/// One resource's op counts, as `io_complete` reports them.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "io_status", depth = 16)]
#[repr(C)]
pub struct ResourceStatus {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// The resource's id, cut to the 63 bytes a `FrameStr<64>` holds.
    pub resource: FrameStr<64>,
    pub submitted: u64,
    pub completed: u64,
    pub failed: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
}

/// One resource's running op counts.
#[cfg(all(feature = "host", unix))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    pub(crate) submitted: u64,
    pub(crate) completed: u64,
    pub(crate) failed: u64,
    pub(crate) bytes_in: u64,
    pub(crate) bytes_out: u64,
}

#[cfg(all(feature = "host", unix))]
impl Counters {
    pub(crate) fn status(&self, now: Timestamp, resource: FrameStr<64>) -> ResourceStatus {
        ResourceStatus {
            timestamp: now,
            resource,
            submitted: self.submitted,
            completed: self.completed,
            failed: self.failed,
            bytes_in: self.bytes_in,
            bytes_out: self.bytes_out,
        }
    }
}

/// `id`, cut to the longest whole-character prefix a `FrameStr<64>` holds.
#[cfg(all(feature = "host", unix))]
pub(crate) fn resource_name(id: &str) -> FrameStr<64> {
    let mut end = id.len().min(FrameStr::<64>::CAPACITY);
    while !id.is_char_boundary(end) {
        end -= 1;
    }
    FrameStr::new(&id[..end]).unwrap_or_default()
}
