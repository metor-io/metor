//! Downlink framing and command ingest over a shared [`LinkState`].
//!
//! [`UplinkSystem`] runs before command consumers. [`TelemetrySystem`] runs
//! last and batches the graph's output records for the link's socket tasks.
//! Snapshot taps contribute their newest record; log taps drain every record.
//! Table packets carry frame bytes, while message packets carry postcard data.
//!
//! Taps drain even without connections so producers are not backpressured.
//! Slow connections drop whole batches at their byte limit. See [`link`] for
//! queue and replay policies.

mod discovery;
mod link;
mod uplink;

pub use uplink::{UplinkParams, UplinkSystem};

pub use link::{LinkParams, LinkState, LinkStats};

use metor_fsw_2_core::log::LogLevel;
use metor_fsw_2_core::{
    AllOutputs, BuildSystem, CyclicSystem, Delivery, Out, RegistryEntry, Shared, System,
    split_record,
};
use metor_fsw_ring::{NoWake, View};
use metor_proto::types::{PACKET_HEADER_LEN, PacketId, PacketTy, Timestamp};
use metor_proto::vtable::VTable;
use metor_proto_wkt::{ComponentMetadata, MsgMetadata, SetMsgMetadata};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Which registry entries the downlink taps.
enum TelemetryMode {
    /// Tap every entry: every system's user frames and their implicit
    /// `system_status`/`log`, plus the coordinator-owned `system_status`/`log`/`status`.
    All,
    /// Tap only the entries whose instance name or frame name appears in the
    /// configured lists; matching either is enough.
    Subset {
        instances: Vec<String>,
        frames: Vec<String>,
    },
}

impl TelemetryMode {
    /// Whether `entry` is tapped. The `frames` list matches
    /// [`RegistryEntry::name`], which covers frame names and channel names
    /// alike.
    fn matches(&self, entry: &RegistryEntry) -> bool {
        match self {
            TelemetryMode::All => true,
            TelemetryMode::Subset { instances, frames } => {
                instances.iter().any(|i| i.as_str() == &*entry.instance)
                    || frames.iter().any(|f| f.as_str() == entry.name())
            }
        }
    }
}

/// Wiring parameters for the built-in downlink (`type="Downlink"`): an
/// optional tap subset. With both lists absent every entry is tapped; with
/// either present an entry is tapped when its instance or its frame/channel
/// name is listed. The link itself is the target's `TcpServer` state
/// declaration, not a per-system address.
///
/// ```python
/// # optional; omit both lists to tap everything
/// m.add("telemetry", Downlink(instances=["nav", "imu"], frames=["gyro_b"]))
/// ```
#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, Clone, Default)]
pub struct DownlinkParams {
    /// Instance names to tap; `None` (with `frames` also `None`) taps everything.
    #[serde(default)]
    pub instances: Option<Vec<String>>,
    /// Frame/channel names to tap.
    #[serde(default)]
    pub frames: Option<Vec<String>>,
}

impl BuildSystem for TelemetrySystem {
    type Params = DownlinkParams;

    /// Construct detached: the builtin link pack's ctor attaches the shared
    /// server ([`attach`](TelemetrySystem::attach)) right after.
    fn new(params: DownlinkParams) -> Self {
        let mode = match (params.instances, params.frames) {
            (None, None) => TelemetryMode::All,
            (instances, frames) => TelemetryMode::Subset {
                instances: instances.unwrap_or_default(),
                frames: frames.unwrap_or_default(),
            },
        };
        Self {
            link: None,
            mode,
            taps: Vec::new(),
            batch: Vec::new(),
            retain_scratch: Vec::new(),
            status: LinkStatus::default(),
        }
    }
}

/// One announced tap's wire schema, replayed to each new connection: a table
/// tap's vtable + component metadata, or a message channel's payload schema.
pub(crate) enum Announce {
    Table {
        packet_id: PacketId,
        vtable: VTable,
        metadata: Vec<ComponentMetadata>,
    },
    Msg(SetMsgMetadata),
}

/// Link status and access to the output registry.
///
/// [`AllOutputs`] grants a reader slot on every registered output.
#[derive(crate::SystemOutput)]
pub struct TelemetryPorts {
    status: metor_fsw_2_core::Output<LinkStatus>,
    all: AllOutputs,
}

/// The link gauge the downlink publishes when it changes: who is connected
/// and what the link has dropped, on the wire like any frame.
#[derive(crate::Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Default, Clone, PartialEq)]
#[repr(C)]
#[metor_fsw(name = "link_status")]
pub struct LinkStatus {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    /// Live connections.
    pub connections: u64,
    /// Connections accepted over the run.
    pub accepted: u64,
    /// Whole batches dropped for one connection over its pending cap.
    pub dropped: u64,
}

/// A read view into one tapped buffer plus the delivery axis and the [`Wire`]
/// framing projected from the entry.
struct Tap {
    view: View<NoWake>,
    delivery: Delivery,
    wire: Wire,
    /// Snapshot taps only: the ring's `committed` at the last contribution, so
    /// a cycle with no new record contributes nothing (the pinned newest
    /// record is not re-sent). `u64::MAX` means nothing contributed yet.
    last_committed: u64,
    /// Snapshot *message* taps: this tap's slot in the link's retained
    /// store. The newest framed record is held there and replayed to every
    /// late-joining connection: latest-wins boot state (a wiring manifest,
    /// a sequence registry) that would otherwise stream exactly once.
    /// Continuously-republished frames need no retention (a new connection
    /// sees them within a cycle), so frame taps stay `None`.
    retain_slot: Option<usize>,
}

/// How a tap frames a drained record, projected from the entry's schema.
enum Wire {
    /// A `Table` packet under the announce-assigned packet id.
    Table { packet_id: PacketId },
    /// A self-describing `Msg` packet; the id is the record's first two bytes.
    Msg,
}

/// A [`CyclicSystem`] that frames every tapped output buffer's pending
/// records into one batch per cycle and hands it to the shared [`LinkState`]
/// for fan-out. Register it after every other cyclic system, or let the
/// wiring resolver defer it there; its `ReceiveAll` capability is what the
/// build-time ordering check keys on.
///
/// Read views are claimed in `init`, which runs after earlier-registered
/// systems' `init`s, so a frame or message emitted during another system's
/// `init` is not downlinked (the view starts at the live edge past it). Values
/// that must reach the ground should be published from the first `execute`
/// onward.
pub struct TelemetrySystem {
    /// The shared link server. `None` on a detached instance
    /// ([`BuildSystem::new`]); the builtin link pack's ctor attaches it.
    link: Option<Shared<LinkState>>,
    mode: TelemetryMode,
    /// The resolved taps; each carries its own delivery axis and wire framing.
    taps: Vec<Tap>,
    /// The cycle's batch scratch, cleared and refilled in place.
    batch: Vec<u8>,
    /// One framed record, reused across retained-tap updates.
    retain_scratch: Vec<u8>,
    /// The last published gauge, so the frame goes out on change only.
    status: LinkStatus,
}

impl TelemetrySystem {
    /// Attach the shared link server this downlink streams through.
    pub fn attach(mut self, link: Shared<LinkState>) -> Self {
        self.link = Some(link);
        self
    }
}

impl System for TelemetrySystem {
    type Input = ();
    type Output = Out<TelemetryPorts>;
    const NAME: &'static str = "telemetry";

    /// Resolve the tap set, claim one read `View` per tapped buffer, and hand
    /// the announce set to the link server, whose accepted connections replay
    /// it before any data.
    fn init(&mut self, output: &mut Self::Output) {
        // `AllOutputs::entries()` is already filtered to telemetered entries,
        // so a command channel or an opted-out frame never reaches the matcher.
        let mut taps = Vec::new();
        let mut announces = Vec::new();
        let mut n_tables = 0usize;
        let mut n_retained = 0usize;
        let mut announced_msgs = std::collections::HashSet::new();
        // Deferred log reports: iterating `output.all` borrows the output
        // bundle, so `output.log()` (a `&mut` borrow) runs after the loop.
        let mut exhausted: Vec<String> = Vec::new();
        for entry in output.all.entries() {
            if !self.mode.matches(entry) {
                continue;
            }
            let Ok(view) = entry.view() else {
                exhausted.push(format!("{}.{}", entry.instance, entry.name()));
                continue;
            };
            // Delivery and wire are independent projections of the entry:
            // delivery picks how much each cycle contributes, schema picks
            // the framing.
            let wire = match entry.announce() {
                Some((vtable, metadata)) => {
                    let packet_id = (n_tables as u16).to_le_bytes();
                    n_tables += 1;
                    announces.push(Announce::Table {
                        packet_id,
                        vtable,
                        metadata,
                    });
                    Wire::Table { packet_id }
                }
                None => {
                    // Several ports may share a message ID, such as LogEvent.
                    if let crate::PortSchema::Postcard {
                        id,
                        schema: Some(schema),
                    } = &entry.desc.schema
                        && announced_msgs.insert(*id)
                    {
                        announces.push(Announce::Msg(SetMsgMetadata {
                            id: *id,
                            metadata: MsgMetadata {
                                name: schema.name.clone(),
                                schema: (**schema).clone(),
                                metadata: Default::default(),
                            },
                        }));
                    }
                    Wire::Msg
                }
            };
            let retain_slot = (entry.delivery() == Delivery::Snapshot && matches!(wire, Wire::Msg))
                .then(|| {
                    let slot = n_retained;
                    n_retained += 1;
                    slot
                });
            taps.push(Tap {
                view,
                delivery: entry.delivery(),
                wire,
                last_committed: u64::MAX,
                retain_slot,
            });
        }

        for key in &exhausted {
            output.log().fault(
                LogLevel::Warn,
                "telemetry_reader_slot",
                &format!("no reader slot left on `{key}` — raise CoordinatorConfig::reader_slack"),
                &[],
            );
        }

        let link = self
            .link
            .as_ref()
            .expect("downlink attached to a TcpServer state (the builtin link pack's ctor)");
        if link.get().set_announces(&announces).is_err() {
            // A second downlink on one server would corrupt the replay every
            // connection decodes against.
            output.log().fault(
                LogLevel::Warn,
                "link_announce_conflict",
                "another downlink already announced on this link; this instance streams nothing",
                &[],
            );
            self.link = None;
            return;
        }
        link.get().set_retained_slots(n_retained);
        self.taps = taps;
    }
}

/// Append one length-prefixed packet to `batch`: the framing [`LenPacket`]
/// builds (`metor_proto::types::LenPacket`), minus the intermediate
/// allocation.
fn append_packet(batch: &mut Vec<u8>, ty: PacketTy, id: PacketId, payload: &[u8]) {
    batch.extend_from_slice(&((PACKET_HEADER_LEN + payload.len()) as u32).to_le_bytes());
    batch.push(ty as u8);
    batch.extend_from_slice(&id);
    batch.push(0); // req_id
    batch.extend_from_slice(payload);
}

/// Frame one drained record onto `batch` per the tap's [`Wire`]: a `Table`
/// packet under the announce-assigned id, or a self-describing `Msg` packet
/// keyed by the record's own leading id (skipped if the record is too short
/// to carry one).
fn append_record(batch: &mut Vec<u8>, wire: &Wire, rec: &[u8]) {
    match wire {
        Wire::Table { packet_id } => append_packet(batch, PacketTy::Table, *packet_id, rec),
        Wire::Msg => {
            if let Some((id, payload)) = split_record(rec) {
                append_packet(batch, PacketTy::Msg, id, payload);
            }
        }
    }
}

impl CyclicSystem for TelemetrySystem {
    fn execute(&mut self, now: Timestamp, _input: &mut Self::Input, output: &mut Self::Output) {
        // An announce conflict detached this instance; taps were never taken,
        // so there is nothing to drain.
        let Some(link_token) = &self.link else {
            return;
        };
        let mut link = link_token.get();

        // Report the server's counters on this cycle's log and the gauge.
        let stats = link.take_stats();
        if stats.closed > 0 {
            output.log().fault(
                LogLevel::Info,
                "link_disconnect",
                "link connections closed",
                &[("closed", &stats.closed)],
            );
        }
        if stats.conn_dropped > 0 {
            output.log().fault(
                LogLevel::Warn,
                "link_conn_dropped",
                "client batches dropped",
                &[("dropped", &stats.conn_dropped)],
            );
        }
        if stats.inbound_dropped > 0 {
            output.log().fault(
                LogLevel::Warn,
                "link_inbound_dropped",
                "inbound command queue overflowed",
                &[("dropped", &stats.inbound_dropped)],
            );
        }
        let connections = link.connections();
        let status = LinkStatus {
            timestamp: now,
            connections: connections as u64,
            accepted: self.status.accepted + stats.accepted,
            dropped: self.status.dropped + stats.conn_dropped,
        };
        if status.connections != self.status.connections
            || status.accepted != self.status.accepted
            || status.dropped != self.status.dropped
        {
            output.status.publish(&status);
            self.status = status;
        }

        // With no connections the batch is skipped entirely, but the taps
        // still drain below (records are consumed and DISCARDED) because an
        // undrained tap view stalls its producer's ring and freezes every
        // consumer of that output, not just telemetry.
        link.prepare_batch(&mut self.batch);
        let mut batch = (connections != 0).then_some(&mut self.batch);
        for tap in &mut self.taps {
            match tap.delivery {
                // An unchanged `committed` means no new record this cycle;
                // contribute nothing rather than re-sending the pinned record.
                Delivery::Snapshot => {
                    let committed = tap.view.committed();
                    if committed == tap.last_committed {
                        continue;
                    }
                    tap.last_committed = committed;
                    match tap.view.try_latest() {
                        Ok(Some(grant)) => match tap.retain_slot {
                            // A retained tap frames once and the bytes go
                            // both ways: appended to this cycle's batch and
                            // held for future connections' replays, even
                            // with no connection live right now.
                            Some(slot) => {
                                self.retain_scratch.clear();
                                append_record(&mut self.retain_scratch, &tap.wire, &grant);
                                if let Some(batch) = &mut batch {
                                    batch.extend_from_slice(&self.retain_scratch);
                                }
                                link.retain(slot, &mut self.retain_scratch);
                            }
                            None => {
                                if let Some(batch) = &mut batch {
                                    append_record(batch, &tap.wire, &grant);
                                }
                            }
                        },
                        Ok(None) => {}
                        Err(_) => output.log().fault(
                            LogLevel::Error,
                            "telemetry_input_corrupt",
                            "tap ring read corrupt",
                            &[],
                        ),
                    }
                }
                // Every record, in order.
                Delivery::Log => {
                    let wire = &tap.wire;
                    let batch = &mut batch;
                    let result = metor_fsw_2_core::drain_view(&mut tap.view, |rec| {
                        if let Some(batch) = batch.as_mut() {
                            append_record(batch, wire, rec);
                        }
                    });
                    if result.is_err() {
                        output.log().fault(
                            LogLevel::Error,
                            "telemetry_input_corrupt",
                            "tap ring read corrupt",
                            &[],
                        );
                    }
                }
            }
        }
        if let Some(batch) = batch
            && !batch.is_empty()
        {
            link.broadcast_buffer(batch);
        }
        link.flush();
    }
}
