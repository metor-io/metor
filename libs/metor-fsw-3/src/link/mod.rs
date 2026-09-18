//! `Publish` and `Subscribe`: a target's telemetry out and its commands in.
//!
//! Each owns its sockets and runs on a background thread. The wire format is
//! metor-panel's and metor-db's, reproduced from fsw-2.

// The connection and transport machinery lands before its two callers do.
#![allow(dead_code)]

mod conn;
mod transport;
mod wire;

use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Frame;
use crate::coordinator::SystemTable;

pub use transport::Transport;

/// The prefix every built-in system registers under, as a pack's id would be.
const BUILTIN: &str = "fsw";

/// What a link reports about its sockets, each cycle its counters change.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "link_status")]
#[repr(C)]
pub struct LinkStatus {
    #[frame(timestamp)]
    pub timestamp: Timestamp,
    pub connections: u32,
    #[frame(skip)]
    _pad: u32,
    pub bytes_out: u64,
    pub batches_dropped: u64,
    pub inbound_dropped: u64,
}

impl LinkStatus {
    fn new(timestamp: Timestamp, stats: conn::Stats, inbound_dropped: u64) -> Self {
        Self {
            timestamp,
            connections: stats.connections,
            _pad: 0,
            bytes_out: stats.bytes_out,
            batches_dropped: stats.batches_dropped,
            inbound_dropped,
        }
    }

    /// Whether anything but the stamp differs, which is what a link reports on.
    fn changed(&self, other: &Self) -> bool {
        (self.connections, self.bytes_out) != (other.connections, other.bytes_out)
            || (self.batches_dropped, self.inbound_dropped)
                != (other.batches_dropped, other.inbound_dropped)
    }
}

/// Registers the link systems every target may name, under `fsw.`.
pub fn register_builtins(_table: &mut SystemTable) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Record;

    #[test]
    fn a_status_frame_announces_its_counters() {
        let status = LinkStatus::new(
            Timestamp(1),
            conn::Stats {
                connections: 2,
                bytes_out: 30,
                batches_dropped: 4,
            },
            5,
        );
        assert_eq!(LinkStatus::MAX_LEN, size_of::<LinkStatus>());
        assert_eq!(LinkStatus::decode(status.as_bytes()), Ok(&status));
        assert!(status.changed(&LinkStatus::new(Timestamp(2), Default::default(), 0)));
    }

    #[test]
    fn only_the_counters_count_as_a_change() {
        let stats = conn::Stats {
            connections: 1,
            ..Default::default()
        };
        let first = LinkStatus::new(Timestamp(1), stats, 0);
        assert!(!first.changed(&LinkStatus::new(Timestamp(9), stats, 0)));
    }

    #[test]
    fn the_builtins_are_named_under_the_fsw_pack() {
        let mut table = SystemTable::new();
        register_builtins(&mut table);
        assert!(table.entries().all(|(ty, _)| ty.starts_with(BUILTIN)));
    }
}
