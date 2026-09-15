//! The frame every system's coordinator-owned `status` output carries.

use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Frame;

/// The status of a system for once cycle
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "status")]
#[repr(C)]
pub struct SystemStatus {
    /// The cycle's timestamp, shared by every system in it.
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    /// Wall time spent inside `execute` in nanoseconds.
    pub exec_time_ns: u64,
    /// Wall time from the start of the cycle to the start of `execute` in nanoseconds.
    pub exec_offset_ns: u64,
}
