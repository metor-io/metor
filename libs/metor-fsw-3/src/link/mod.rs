//! Links carry telemetry between targets over the network.
//!
//! A link is a built-in system that moves records over TCP, either by
//! listening for peers or by dialing one. There are two core links:
//! [`Publish`] sends the records wired into it to every connected peer, and
//! [`Subscribe`] receives records from connected peers and writes them onto
//! its output ports. `Db`, behind the `db` feature, is a third link that
//! records everything wired into it in an embedded metor-db, which is how a
//! gateway stores telemetry.

mod conn;
#[cfg(feature = "db")]
mod db;
mod inbox;
mod publish;
mod subscribe;
mod transport;
mod wire;

use std::path::PathBuf;

use metor_proto::types::{PacketId, Timestamp};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::Frame;
use crate::coordinator::{BuildError, SystemTable};
use crate::log::Log;
use crate::port::Output;

pub use conn::{ConnCap, ConnCapTooLarge};
#[cfg(feature = "db")]
pub use db::{Db, DbStatus};
pub use publish::{Publish, PublishParams};
pub use subscribe::{Subscribe, SubscribeParams};
pub use transport::Transport;

/// The system type name for [`Publish`].
pub const PUBLISH_TYPE: &str = "fsw.publish";
/// The system type name for [`Subscribe`].
pub const SUBSCRIBE_TYPE: &str = "fsw.subscribe";
/// The system type name for a gateway's embedded database.
pub const DB_TYPE: &str = "fsw.db";
/// Every link system type. Each one's params include [`LinkParams`].
pub const LINK_TYPES: [&str; 3] = [PUBLISH_TYPE, SUBSCRIBE_TYPE, DB_TYPE];

/// The configuration shared by every link.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LinkParams {
    pub transport: Transport,
    /// The target's telemetry namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The link's name, unique within its target.
    #[serde(default)]
    pub link: String,
}

/// The configuration for a `Db`.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DbParams {
    #[serde(flatten)]
    pub link: LinkParams,
    /// The directory the database is stored in. When unset, the database
    /// uses a fresh directory under the system temp dir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// The ids of messages that subscribers receive through this database.
    /// `metor build` fills this in.
    #[serde(default)]
    pub command_ids: Vec<PacketId>,
}

/// Socket statistics for a link. A link writes one on every cycle where a
/// counter changed.
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

    /// Returns true if any field other than the timestamp differs.
    fn changed(&self, other: &Self) -> bool {
        Self {
            timestamp: other.timestamp,
            ..*self
        } != *other
    }
}

/// Writes a link's status when a counter changes.
#[derive(Default)]
struct StatusReporter {
    reported: LinkStatus,
}

impl StatusReporter {
    /// Writes `status` to `link_status` if any counter changed since the last
    /// report.
    ///
    /// If more inbound packets were dropped since the last report, this also
    /// logs a fault with the count.
    fn report(&mut self, status: LinkStatus, link_status: &mut Output<LinkStatus>, log: &mut Log) {
        if status.inbound_dropped > self.reported.inbound_dropped {
            let dropped = status.inbound_dropped - self.reported.inbound_dropped;
            log.fault("inbound_dropped", format!("{dropped} packets dropped"));
        }
        if status.changed(&self.reported) {
            let _ = link_status.write(&status);
            self.reported = status;
        }
    }
}

/// Registers the link systems into `table`.
pub(crate) fn register(table: &mut SystemTable) -> Result<(), BuildError> {
    table.register_async(PUBLISH_TYPE, publish::publish(), Publish::new)?;
    table.register_async(SUBSCRIBE_TYPE, subscribe::subscribe(), Subscribe::new)?;
    #[cfg(feature = "db")]
    table.register_async(DB_TYPE, db::db(), Db::new)?;
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
