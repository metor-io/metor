//! Link is responsible for connecting targets telemetry together
//!
//! [`publish`] and [`subscribe`] are the two core systems. Publish sends data to targets, and Subscribe collects data from a target.
//! [`db`], behind the `db` feature, is the embedded database a gateway records into.

mod conn;
#[cfg(feature = "db")]
mod db;
mod inbox;
mod publish;
mod subscribe;
mod transport;
mod wire;

use std::net::SocketAddr;
use std::sync::Mutex;

use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Frame;
use crate::coordinator::{BuildError, SystemTable};
use crate::log::Log;
use crate::port::Output;

#[cfg(feature = "db")]
pub use db::{Db, DbParams, DbStatus};
pub use publish::{Publish, PublishParams};
pub use subscribe::{Subscribe, SubscribeParams};
pub use transport::Transport;

/// The prefix every built-in system registers under, as a pack's id would be.
const BUILTIN: &str = "fsw";

/// Bytes a link holds for one connection before it drops a batch.
fn default_conn_cap() -> usize {
    1 << 20
}

/// The bound sockets for this process
static BOUND: Mutex<Vec<(String, SocketAddr)>> = Mutex::new(Vec::new());

/// Records a bound endpoint
fn record_bound(link: &str, endpoint: &transport::Endpoint) {
    let Some(addr) = endpoint.local_addr() else {
        return;
    };
    // PANIC Safety: nothing panics while it holds this lock.
    BOUND
        .lock()
        .expect("an unpoisoned registry")
        .push((link.to_string(), addr));
}

/// Returns the bound endpoints for this process
pub fn bound_ports() -> Vec<(String, SocketAddr)> {
    // PANIC Safety: as above.
    BOUND.lock().expect("an unpoisoned registry").clone()
}

/// What a link reports about its sockets, each cycle its counters change.
#[derive(
    Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, Default, PartialEq,
)]
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
    fn new(stats: conn::Stats, inbound_dropped: u64) -> Self {
        Self {
            timestamp: Timestamp::now(),
            connections: stats.connections,
            bytes_out: stats.bytes_out,
            batches_dropped: stats.batches_dropped,
            inbound_dropped,
            ..Default::default()
        }
    }

    /// Whether anything but the stamp differs, which is what a link reports on.
    fn changed(&self, other: &Self) -> bool {
        Self {
            timestamp: other.timestamp,
            ..*self
        } != *other
    }
}

/// Writes the status when its counters moved, faulting the packets no port saw.
fn report(
    status: &LinkStatus,
    reported: &mut LinkStatus,
    link_status: &Output<LinkStatus>,
    log: &mut Log,
) {
    if status.inbound_dropped > reported.inbound_dropped {
        let dropped = status.inbound_dropped - reported.inbound_dropped;
        log.fault("inbound_dropped", format!("{dropped} packets dropped"));
    }
    if status.changed(reported) {
        let _ = link_status.write(status);
        *reported = *status;
    }
}

/// Registers the link systems every target may name, under `fsw.`.
pub fn register_builtins(table: &mut SystemTable) -> Result<(), BuildError> {
    table.register_async(
        &format!("{BUILTIN}.publish"),
        publish::publish(),
        Publish::new,
    )?;
    table.register_async(
        &format!("{BUILTIN}.subscribe"),
        subscribe::subscribe(),
        Subscribe::new,
    )?;
    table.register(
        &format!("{BUILTIN}.channel_registry"),
        crate::channel::channel_registry(),
        crate::channel::Registry::new,
    )?;
    table.insert(crate::dl::SLOT_TYPE, crate::dl::slot::entry())?;
    #[cfg(feature = "db")]
    table.register_async(crate::deploy::DB_TYPE, db::db(), Db::new)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_changes_on_counters_only() {
        let status = LinkStatus {
            connections: 1,
            ..Default::default()
        };
        let restamped = LinkStatus {
            timestamp: Timestamp(9),
            ..status
        };
        assert!(!status.changed(&restamped));
        for moved in [
            LinkStatus {
                connections: 2,
                ..status
            },
            LinkStatus {
                bytes_out: 1,
                ..status
            },
            LinkStatus {
                batches_dropped: 1,
                ..status
            },
            LinkStatus {
                inbound_dropped: 1,
                ..status
            },
        ] {
            assert!(status.changed(&moved), "{moved:?}");
        }
    }
}
