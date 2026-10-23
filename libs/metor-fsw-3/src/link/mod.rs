//! Links carry telemetry between targets over the network.
//!
//! A link is a built-in system that moves records over TCP, either by
//! listening for peers or by dialing one. There are two core links:
//! [`Publish`] sends the records wired into it to every connected peer, and
//! [`Subscribe`] receives records from connected peers and writes them onto
//! its output ports. `Db`, behind the `db` feature, is a third link that
//! records everything wired into it in an embedded metor-db, which is how a
//! gateway stores telemetry.

mod batch;
mod conns;
#[cfg(feature = "db")]
mod db;
mod deframe;
mod peer;
mod publish;
mod subscribe;
mod wire;

use core::num::NonZeroUsize;
use core::time::Duration;

use metor_proto::types::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::coordinator::{BuildError, SystemTable};
use crate::def::{DefCx, DefError};
use crate::log::Log;
use crate::port::Output;
use crate::system::PortDef;
use crate::{Frame, SeqError};

use conns::{Conns, ConnsConfig, Event, Stats, Turn};

#[cfg(feature = "db")]
pub use db::{Db, DbParams, DbStatus};
pub use publish::Publish;
pub use subscribe::Subscribe;

/// The system type name for [`Publish`].
pub const PUBLISH_TYPE: &str = "fsw.publish";
/// The system type name for [`Subscribe`].
pub const SUBSCRIBE_TYPE: &str = "fsw.subscribe";
/// The system type name for a gateway's embedded database.
pub const DB_TYPE: &str = "fsw.db";

/// The configuration of a [`Publish`] or a [`Subscribe`]. Its endpoint is
/// the resource granted to `ep`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct LinkParams {
    /// The target's telemetry namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// The link's name, unique within its target.
    #[serde(default)]
    pub link: String,
    /// The most peers served at once. A dialing link serves one.
    #[serde(default = "max_connections")]
    pub max_connections: NonZeroUsize,
    /// Whether `ep` dials its peer rather than listening.
    #[serde(default)]
    pub dialing: bool,
    /// The bytes of each connection's buffers in the direction records
    /// travel: a `Publish`'s two send buffers and its batch, or a
    /// `Subscribe`'s receive buffer. The other direction carries control
    /// packets and takes [`CONTROL_CAP`] at most. Every port's records must
    /// fit, with their packet overhead.
    #[serde(default = "conn_cap")]
    pub conn_cap: usize,
    /// How long a send may stay in flight with none finishing before its
    /// connection closes, in milliseconds.
    #[serde(default = "write_timeout_ms")]
    pub write_timeout_ms: u64,
}

fn max_connections() -> NonZeroUsize {
    // PANIC Safety: evaluated at compile time.
    const { NonZeroUsize::new(8).expect("8 is non-zero") }
}

fn conn_cap() -> usize {
    1 << 20
}

fn write_timeout_ms() -> u64 {
    5_000
}

/// The most bytes a link's buffers take in the direction that carries only
/// control packets.
pub const CONTROL_CAP: usize = 64 << 10;

impl LinkParams {
    /// The connections of a link that sends records and receives control.
    fn sending(&self) -> ConnsConfig {
        self.conns(self.conn_cap.min(CONTROL_CAP), self.conn_cap)
    }

    /// The connections of a link that receives records and sends control.
    fn receiving(&self) -> ConnsConfig {
        self.conns(self.conn_cap, self.conn_cap.min(CONTROL_CAP))
    }

    fn conns(&self, rx_cap: usize, tx_cap: usize) -> ConnsConfig {
        ConnsConfig {
            slots: self.max_connections.get(),
            rx_cap,
            tx_cap,
            dialing: self.dialing,
            write_timeout: Duration::from_millis(self.write_timeout_ms),
        }
    }

    /// Decodes a link's params, `None` when the config gives none.
    fn of(cx: &DefCx<'_>) -> Result<Option<Self>, DefError> {
        Option::<Self>::deserialize(cx.params).map_err(|e| DefError::Params {
            message: e.to_string(),
        })
    }
}

/// A link's op budget: [`conns::ops_for`] the slots its params ask for.
fn link_ops(cx: &DefCx<'_>) -> Result<u16, DefError> {
    let params = LinkParams::of(cx)?;
    let slots = params.map_or(max_connections().get(), |p| p.sending().slot_count());
    u16::try_from(conns::ops_for(slots)).map_err(|_| DefError::Params {
        message: format!("{slots} connections need more ops than a system holds"),
    })
}

/// Rejects the first port whose records, framed as packets, do not fit the
/// link's `conn_cap`.
fn check_conn_cap(cx: &DefCx<'_>, ports: &[PortDef]) -> Result<(), DefError> {
    let conn_cap = LinkParams::of(cx)?.map_or(conn_cap(), |p| p.conn_cap);
    match ports
        .iter()
        .find(|port| !wire::fits(port.max_len, conn_cap))
    {
        Some(port) => Err(DefError::PortExceedsConnCap {
            port: port.name.to_string(),
            max_len: port.max_len,
            conn_cap,
        }),
        None => Ok(()),
    }
}

/// The error a link's body ends with when its buffers do not fit its peers.
fn failed(error: impl core::fmt::Display) -> SeqError {
    SeqError::Failed(error.to_string().into())
}

/// The error a link's body ends with when its buffers were already taken.
fn made_once() -> SeqError {
    SeqError::Failed("a link runs once per make".into())
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
    fn new(stats: Stats, inbound_dropped: u64, now: Timestamp) -> Self {
        Self {
            timestamp: now,
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

/// Writes a link's status when a counter changes, and logs its
/// connections' events.
#[derive(Default)]
struct StatusReporter {
    reported: LinkStatus,
}

impl StatusReporter {
    /// Logs `turn`'s events, and writes `conns`' status to `link_status` if
    /// any counter changed since the last report.
    ///
    /// If more inbound packets were dropped since the last report, this also
    /// logs a fault with the count.
    fn report(
        &mut self,
        turn: Turn,
        conns: &Conns,
        inbound_dropped: u64,
        now: Timestamp,
        link_status: &mut Output<LinkStatus>,
        log: &mut Log,
    ) {
        let rx_cap = conns.rx_cap();
        turn.events().for_each(|event| log_event(event, rx_cap, log));
        let status = LinkStatus::new(conns.stats(), inbound_dropped, now);
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

/// Logs `event`. `rx_cap` is the bytes of a connection's receive buffer.
fn log_event(event: Event, rx_cap: usize, log: &mut Log) {
    match event {
        Event::Opened(Some(peer)) => log.info(format!("connected to {peer}")),
        Event::Opened(None) => log.info("connected"),
        Event::Closed => log.info("disconnected"),
        Event::Oversized { len } => log.fault(
            "oversized_packet",
            format!(
                "closed a peer that sent a {len}-byte packet past the {rx_cap}-byte receive buffer"
            ),
        ),
        Event::Refused => log.warn("refused a peer: every connection is taken"),
        Event::Failed(error) => log.warn(format!("no stream opened: {error}")),
    }
}

/// Registers the link systems into `table`.
pub(crate) fn register(table: &mut SystemTable) -> Result<(), BuildError> {
    table.register_sequence(PUBLISH_TYPE, publish::publish(), Publish::new)?;
    table.register_sequence(SUBSCRIBE_TYPE, subscribe::subscribe(), Subscribe::new)?;
    for ty in [PUBLISH_TYPE, SUBSCRIBE_TYPE] {
        if let Some(entry) = table.get_mut(ty) {
            entry.set_ops(link_ops);
        }
    }
    #[cfg(feature = "db")]
    table.register_async(DB_TYPE, db::db(), Db::new)?;
    Ok(())
}

#[cfg(test)]
mod tests;
