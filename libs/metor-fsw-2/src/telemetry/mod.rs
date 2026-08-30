//! Serves the telemetry link: downlink fan-out and command ingest over the
//! in-FSW [`LinkState`] server.
//!
//! The link is one pack-shared [`LinkState`] (the `TcpServer` wiring state)
//! and two attached [`CyclicSystem`]s. [`UplinkSystem`] runs early in the
//! cycle: it drains the server's inbound command queue onto minted message
//! output ports, so consumers receive commands over explicit edges the same
//! cycle they arrived. [`TelemetrySystem`] runs last (its `ReceiveAll`
//! capability keys the ordering check): it frames every tapped buffer's
//! pending records into one batch and hands it to the server, which fans it
//! out to every connection. All socket I/O lives on the server's tasks; the
//! cycle never awaits the link.
//!
//! # Taps and wire framing
//!
//! Each tapped registry entry becomes one tap, and the entry's two axes drive
//! it independently. Its [`Delivery`](crate::Delivery) picks how much of the
//! buffer each cycle contributes to the batch:
//!
//! * `Snapshot` entries contribute only the cycle's newest record; a cycle
//!   with nothing new contributes nothing.
//! * `Log` entries contribute every pending record, in commit order. Event
//!   and command records must never be coalesced.
//!
//! The entry's schema picks the wire framing. A table entry is framed as a
//! `Table` packet whose payload is the ring record itself (the bytes a system
//! committed are the bytes on the wire), referencing a `VTable` announced to
//! each connection before any data. A postcard entry is framed as a
//! `Msg` packet whose id is the record's leading two bytes. When the port has
//! a message schema, the replay includes one `SetMsgMetadata` packet for that
//! id. The two axes combine freely. The wire is a plain stream of length-prefixed
//! packets, so a batch is just the cycle's packets concatenated and the
//! receiver never notices the batching.
//!
//! # Loss policy
//!
//! The link must never backpressure the target. Taps drain every cycle no
//! matter what — an undrained view stalls its producer's ring — and framing
//! is skipped entirely with no connections. A connection that cannot keep up
//! misses whole batches behind its byte cap (counted per occurrence, folded
//! into the downlink's health as `link_conn_dropped`) and is never
//! disconnected; see the [`link`] module doc for the server's policies.

mod discovery;
mod link;

pub use link::{LinkParams, LinkState, LinkStats};

use metor_fsw_ring::{NoWake, View};
use metor_proto::types::{PACKET_HEADER_LEN, PacketId, PacketTy, Timestamp};
use metor_proto::vtable::VTable;
use metor_proto_wkt::{ComponentMetadata, MsgMetadata, SetMsgMetadata};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use metor_fsw_2_core::Shared;
use metor_fsw_2_core::health::{HealthPort, LogLevel};
use metor_fsw_2_core::{AllOutputs, RegistryEntry};
use metor_fsw_2_core::{BindPorts, RingSource};
use metor_fsw_2_core::{
    BuildCtx, BuildSystem, ConfigureError, CyclicSystem, HealthOutput, Out, System, SystemOutput,
};
use metor_fsw_2_core::{Declarations, Delivery, PortDesc, SystemDescriptor};
use metor_fsw_2_core::{MsgFanOut, NamedMsg, split_record};

/// Which registry entries the downlink taps.
enum TelemetryMode {
    /// Tap every entry: every system's user frames and their implicit
    /// `health`/`log`, plus the coordinator-owned `health`/`log`/`status`.
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

/// Wiring parameters for the built-in uplink (`type="Uplink"`): the message
/// types to relay off the link. Each `msgs` token is a [`NamedMsg::NAME`]
/// resolved against the registry's [`MsgTable`](crate::MsgTable); the uplink
/// mints one ordinary message output port per msg, so
/// `m.route(uplink, …, msg="…")` edges resolve like any other. An empty
/// `msgs` list means the uplink relays nothing (and warns); there is no
/// built-in default set.
///
/// ```python
/// m.add("uplink", Uplink(msgs=["SequenceCommand", "AlarmAck"]))
/// ```
#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, Clone, Default)]
pub struct UplinkParams {
    /// The [`NamedMsg::NAME`] tokens of the messages to relay.
    #[serde(default)]
    pub msgs: Option<Vec<String>>,
}

impl BuildSystem for UplinkSystem {
    type Params = UplinkParams;

    /// Construct detached, like the downlink's.
    fn new(params: UplinkParams) -> Self {
        Self {
            unresolved: params.msgs.unwrap_or_default(),
            ..Self::new()
        }
    }

    fn configure(&mut self, ctx: &BuildCtx) -> Result<(), ConfigureError> {
        for token in std::mem::take(&mut self.unresolved) {
            let Some((name, id)) = ctx.msgs.get(&token) else {
                return Err(ConfigureError::UnknownMsg {
                    name: token,
                    available: ctx.msgs.names(),
                });
            };
            // A duplicate token folds into one port instead of a duplicate-key
            // error.
            if !self.msgs.iter().any(|&(_, existing)| existing == id) {
                self.msgs.push((name, id));
            }
        }
        Ok(())
    }
}

/// The uplink's output bundle: the two implicit health/log ports first, then a
/// [`MsgFanOut`] holding one ordinary message output per configured msg. A
/// consumer receives a msg only over an explicit edge, and every minted port
/// is untelemetered, so inbound control is never echoed back on the downlink.
///
/// Hand-written rather than [`Out`]-wrapped because the fan-out's port count
/// is decided by configuration. [`MsgFanOut::bind`] drains every ring the
/// source still holds, so the minted ports must be the descriptor's trailing
/// outputs, the reverse of the [`Out`] convention of user ports first. The
/// static [`decls`](SystemOutput::decls) carry only health and log (the
/// empty-config shape); [`UplinkSystem::instance_descriptor`] appends the msg
/// ports in the same config order the bind walk pops rings, keeping the
/// positional contract. Its [`HealthOutput`] impl is what lets the ordinary
/// cyclic runner drive it.
pub struct UplinkOut {
    fan: MsgFanOut,
    health: HealthPort,
}

impl UplinkOut {
    /// Disjoint borrows of the minted ports and the health handle.
    fn split(&mut self) -> (&mut MsgFanOut, &mut HealthPort) {
        (&mut self.fan, &mut self.health)
    }
}

impl SystemOutput for UplinkOut {
    fn decls() -> Declarations {
        vec![
            PortDesc::of::<crate::SystemStatus>(),
            PortDesc::msg_named::<crate::LogEvent>("log"),
        ]
        .into()
    }
}

impl HealthOutput for UplinkOut {
    fn health(&mut self) -> &mut HealthPort {
        &mut self.health
    }
}

impl BindPorts for UplinkOut {
    fn bind<S: RingSource>(src: &mut S) -> Self {
        let health = crate::Output::bind(src);
        let log = metor_fsw_2_core::MsgOut::bind(src);
        let fan = MsgFanOut::bind(src);
        let mut health = HealthPort::new(health, log);
        health.set_instance(src.instance_name());
        Self { fan, health }
    }
}

/// The command ingest system, the read twin of [`TelemetrySystem`]: a
/// [`CyclicSystem`] attached to the shared [`LinkState`] that drains the
/// server's inbound queue each cycle and relays each msg onto its matching
/// minted output. Register it early — before its consumers — and a command
/// received between cycles is consumed the same cycle it is republished. It
/// is a pure pass-through for any message id: the forward set is
/// per-instance configuration (`msgs`, or [`with_msg`](Self::with_msg)
/// programmatically), never a compiled-in list, and payloads are forwarded
/// verbatim, leaving each consumer's own drain to decode (and discard
/// garbage) exactly as on any other message edge.
pub struct UplinkSystem {
    /// The shared link whose inbound queue feeds the ports. `None` on a
    /// detached instance ([`BuildSystem::new`]); the builtin link pack's
    /// ctor attaches it.
    link: Option<Shared<LinkState>>,
    /// The forward set, in config order: one `(NAME, ID)` per msg. Index k is
    /// minted output port k and bound writer k, so this one list keys the
    /// dispatch and the ports.
    msgs: Vec<(&'static str, PacketId)>,
    /// Config name tokens awaiting resolution in
    /// [`configure`](BuildSystem::configure); [`with_msg`](Self::with_msg)
    /// resolves statically instead.
    unresolved: Vec<String>,
}

impl Default for UplinkSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl UplinkSystem {
    /// A detached uplink with an empty forward set; add msgs via
    /// [`with_msg`](Self::with_msg) or the registry's `msgs` params, and
    /// attach the link via the builtin pack (or [`attach`](Self::attach)).
    pub fn new() -> Self {
        Self {
            link: None,
            msgs: Vec::new(),
            unresolved: Vec::new(),
        }
    }

    /// Attach the shared link server this uplink drains.
    pub fn attach(mut self, link: Shared<LinkState>) -> Self {
        self.link = Some(link);
        self
    }

    /// Add `M` to the forward set, the typed twin of the `msgs` config list:
    /// mints an `M`-keyed output port. Idempotent.
    pub fn with_msg<M: NamedMsg>(mut self) -> Self {
        if !self.msgs.iter().any(|&(_, id)| id == M::ID) {
            self.msgs.push((M::NAME, M::ID));
        }
        self
    }
}

impl System for UplinkSystem {
    type Input = ();
    type Output = UplinkOut;
    const NAME: &'static str = "uplink";

    /// Advertise the forward set in the link's identity packet, so a ground
    /// client knows which msg ids to send up. Init order makes this land
    /// before the downlink freezes the replay (the downlink is deferred
    /// last by its `ReceiveAll` capability); a late registration is a
    /// config defect reported here rather than silently unadvertised.
    fn init(&mut self, output: &mut UplinkOut) {
        let (fan, health) = output.split();
        if self.msgs.is_empty() {
            health.log(
                LogLevel::Warn,
                "uplink has no msgs configured; it will relay nothing",
            );
        } else if fan.len() != self.msgs.len() {
            // One writer per configured msg is the bind contract; a mismatch
            // means the registered descriptor and this instance diverged.
            health.error("uplink_bind_mismatch");
        }

        let link = self
            .link
            .as_ref()
            .expect("uplink attached to a TcpServer state (the builtin link pack's ctor)");
        let ids: Vec<PacketId> = self.msgs.iter().map(|&(_, id)| id).collect();
        if link.get().add_uplink_msgs(&ids).is_err() {
            let health = output.health();
            health.error("uplink_announced_late");
            health.log(
                LogLevel::Warn,
                "uplink registered after the downlink; its command set is not advertised to clients",
            );
        }
    }
}

impl CyclicSystem for UplinkSystem {
    /// The static health/log shape plus one untelemetered message port per
    /// configured msg, in config order. The dispatch and the wired ports
    /// both derive from the one `msgs` list, so they cannot diverge.
    fn instance_descriptor(&self) -> SystemDescriptor {
        let mut desc = Self::descriptor();
        desc.outputs.extend(
            self.msgs
                .iter()
                .map(|&(name, id)| PortDesc::msg_dynamic(name, id).untelemetered()),
        );
        desc
    }

    /// Drain the link's inbound queue onto the minted outputs. A msg outside
    /// the configured set bumps `uplink_unroutable` (a sender/config
    /// mismatch), a full ring bumps `uplink_dropped`; either way the queue
    /// drains fully so a bad sender cannot wedge it.
    fn execute(&mut self, _now: Timestamp, _input: &mut (), output: &mut UplinkOut) {
        let (fan, health) = output.split();
        let link = self
            .link
            .as_ref()
            .expect("uplink attached to a TcpServer state (the builtin link pack's ctor)");
        let msgs = &self.msgs;
        link.get().drain_inbound(
            |id, payload| match msgs.iter().position(|&(_, mid)| mid == id) {
                Some(idx) => {
                    if fan.write_raw(idx, id, payload).is_err() {
                        health.error("uplink_dropped");
                    }
                }
                None => health.error("uplink_unroutable"),
            },
        );
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

/// The downlink's output bundle: the telemetered link gauge plus the
/// [`AllOutputs`] receive-all field. `init` reaches the output registry
/// through `all`, the receive-all capability its decl contributes earns the
/// downlink a reader slot on every buffer at sizing time, and its `bind`
/// pulls the host registry rather than consuming a ring.
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
    /// late-joining connection — latest-wins boot state (a wiring manifest,
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
        // Deferred health reports: iterating `output.all` borrows the output
        // bundle, so `output.health()` (a `&mut` borrow) runs after the loop.
        let mut exhausted: Vec<String> = Vec::new();
        for entry in output.all.entries() {
            if !self.mode.matches(entry) {
                continue;
            }
            let view = match entry.view() {
                Ok(v) => v,
                // The buffer has no reader slot left. Build-time sizing makes
                // this unreachable for the known consumers, but a hand-built
                // over-subscription (or too little configured reader slack) is
                // worth diagnosing, so log the buffer by name and skip the tap
                // instead of panicking.
                Err(_) => {
                    exhausted.push(format!("{}.{}", entry.instance, entry.name()));
                    continue;
                }
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
                    // A message channel: announce its payload schema once per
                    // distinct packet id (every system's `log` port shares
                    // `LogEvent::ID`, so the dedup is load-bearing).
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
            let health = output.health();
            health.error("telemetry_reader_slot");
            health.log(
                LogLevel::Warn,
                &format!("no reader slot left on `{key}` — raise CoordinatorConfig::reader_slack"),
            );
        }

        let link = self
            .link
            .as_ref()
            .expect("downlink attached to a TcpServer state (the builtin link pack's ctor)");
        if link.get().set_announces(&announces).is_err() {
            // A second downlink on one server would corrupt the replay every
            // connection decodes against.
            let health = output.health();
            health.error("link_announce_conflict");
            health.log(
                LogLevel::Warn,
                "another downlink already announced on this link; this instance streams nothing",
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

        // Fold the server's counters into this cycle's health and the gauge.
        let stats = link.take_stats();
        for _ in 0..stats.closed {
            output.health().error("link_disconnect");
        }
        for _ in 0..stats.conn_dropped {
            output.health().error("link_conn_dropped");
        }
        for _ in 0..stats.inbound_dropped {
            output.health().error("link_inbound_dropped");
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
        // still drain below — records are consumed and DISCARDED — because an
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
                            // held for future connections' replays — even
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
                        Err(_) => output.health().error("telemetry_input_corrupt"),
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
                        output.health().error("telemetry_input_corrupt");
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
