//! `io_status`: what each resource's ops have done, written when it changes.

use metor_component::FrameStr;
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Frame;
#[cfg(all(feature = "host", unix))]
use crate::io::{IoError, OpKind};

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
    /// Bytes a sim wire dropped past a full FIFO.
    pub dropped: u64,
    /// Peers a listener's `allow` list turned away.
    pub rejected: u64,
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
    pub(crate) dropped: u64,
    pub(crate) rejected: u64,
}

#[cfg(all(feature = "host", unix))]
impl Counters {
    /// Counts one finished op of `kind`: its result, and the bytes it moved.
    pub(crate) fn record_completion(&mut self, kind: OpKind, result: &Result<u32, IoError>) {
        self.completed += 1;
        let byte_count = match result {
            Ok(n) => u64::from(*n),
            Err(_) => {
                self.failed += 1;
                0
            }
        };
        match kind {
            OpKind::Read | OpKind::Recv => self.bytes_in += byte_count,
            OpKind::Write | OpKind::Send | OpKind::SendTo => self.bytes_out += byte_count,
            _ => {}
        }
    }

    pub(crate) fn status(&self, now: Timestamp, resource: FrameStr<64>) -> ResourceStatus {
        ResourceStatus {
            timestamp: now,
            resource,
            submitted: self.submitted,
            completed: self.completed,
            failed: self.failed,
            bytes_in: self.bytes_in,
            bytes_out: self.bytes_out,
            dropped: self.dropped,
            rejected: self.rejected,
        }
    }
}
