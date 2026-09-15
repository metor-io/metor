//! The frame every system's coordinator-owned `status` output carries.

use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Frame;

/// One system's timing for one cycle, published on an ordinary output ring, so
/// a later system may wire `<id>.status` as an input.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "status")]
#[repr(C)]
pub struct SystemStatus {
    /// The cycle's timestamp, shared by every system in it.
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Wall time spent inside `execute`.
    pub exec_time_ns: u64,
    /// Wall time from the start of the cycle to the start of `execute`.
    pub exec_offset_ns: u64,
}
