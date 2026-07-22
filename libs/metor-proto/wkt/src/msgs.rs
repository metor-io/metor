use metor_proto::{
    buf::IoBuf,
    schema::Schema,
    types::{
        ComponentId, Msg, MsgBuf, OwnedTable, OwnedTimeSeries, PacketId, Request, Timestamp,
        TryFromPacket,
    },
    vtable::{Field, Op, VTable},
};
use postcard_schema::schema::owned::OwnedNamedType;
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, path::PathBuf, time::Duration};
use std::{collections::HashMap, ops::Range};

use crate::{LastUpdated, metadata::ComponentMetadata};

#[derive(Serialize, Deserialize, Clone, postcard_schema::Schema)]
pub struct VTableMsg {
    pub id: PacketId,
    pub vtable: VTable<Vec<Op>, Vec<u8>, Vec<Field>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
pub struct Stream {
    #[serde(default)]
    pub behavior: StreamBehavior,
    #[serde(default)]
    pub id: StreamId,
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
pub struct VTableStream {
    pub id: PacketId,
}

impl Request for VTableStream {
    type Reply<B: IoBuf + Clone> = StreamReply<B>;
}

#[derive(Clone)]
pub enum StreamReply<B: IoBuf> {
    Table(OwnedTable<B>),
    VTable(VTableMsg),
}

impl<B: IoBuf + Clone> TryFromPacket<B> for StreamReply<B> {
    fn try_from_packet(
        packet: &metor_proto::types::OwnedPacket<B>,
    ) -> Result<Self, metor_proto::error::Error> {
        match packet {
            metor_proto::types::OwnedPacket::Msg(m) if m.id == VTableMsg::ID => {
                let msg = m.parse::<VTableMsg>()?;
                Ok(Self::VTable(msg))
            }
            metor_proto::types::OwnedPacket::Table(table) => Ok(Self::Table(table.clone())),
            _ => Err(metor_proto::error::Error::InvalidPacket),
        }
    }
}

impl Request for Stream {
    type Reply<B: IoBuf + Clone> = StreamReply<B>;
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, postcard_schema::Schema)]
pub struct FixedRateOp {
    pub stream_id: StreamId,
    pub behavior: FixedRateBehavior,
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
pub struct FixedRateBehavior {
    pub initial_timestamp: InitialTimestamp,
    /// The time interval between each tick in nanoseconds
    pub timestep: u64,
    /// The number of ticks per second
    pub frequency: u64,
}

impl Default for FixedRateBehavior {
    fn default() -> Self {
        Self {
            initial_timestamp: Default::default(),
            timestep: (1e9 / 60.0) as u64,
            frequency: 60,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, postcard_schema::Schema)]
pub enum InitialTimestamp {
    #[default]
    Earliest,
    Latest,
    Manual(Timestamp),
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, postcard_schema::Schema)]
pub enum StreamBehavior {
    #[default]
    RealTime,
    FixedRate(FixedRateBehavior),
}

pub type StreamId = u64;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SetStreamState {
    pub id: StreamId,
    pub playing: Option<bool>,
    pub timestamp: Option<Timestamp>,
    pub time_step: Option<Duration>,
    pub frequency: Option<u64>,
}

impl SetStreamState {
    pub fn rewind(id: StreamId, tick: Timestamp) -> Self {
        Self {
            id,
            playing: None,
            timestamp: Some(tick),
            time_step: None,
            frequency: None,
        }
    }
}

impl Msg for SetStreamState {
    const ID: PacketId = [224, 2];
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GetTimeSeries {
    pub id: PacketId,
    pub range: Range<Timestamp>,
    pub component_id: ComponentId,
    pub limit: Option<usize>,
}

impl Msg for GetTimeSeries {
    const ID: PacketId = [224, 3];
}

impl Request for GetTimeSeries {
    type Reply<B: IoBuf + Clone> = OwnedTimeSeries<B>;
}

#[derive(Serialize, Deserialize)]
pub struct SchemaMsg(pub Schema<Vec<u64>>);
impl Msg for SchemaMsg {
    const ID: PacketId = [224, 4];
}

#[derive(Serialize, Deserialize)]
pub struct GetSchema {
    pub component_id: ComponentId,
}

impl Msg for GetSchema {
    const ID: PacketId = [224, 5];
}

impl Request for GetSchema {
    type Reply<B: IoBuf + Clone> = SchemaMsg;
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GetComponentMetadata {
    pub component_id: ComponentId,
}

impl Msg for GetComponentMetadata {
    const ID: PacketId = [224, 6];
}

impl Request for GetComponentMetadata {
    type Reply<B: IoBuf + Clone> = crate::ComponentMetadata;
}

#[derive(Clone, Serialize, Deserialize, Debug, postcard_schema::Schema)]
#[serde(transparent)]
pub struct SetComponentMetadata(pub ComponentMetadata);

impl SetComponentMetadata {
    pub fn new(component_id: impl Into<ComponentId>, name: impl ToString) -> Self {
        let component_id = component_id.into();
        let name = name.to_string();
        Self(ComponentMetadata {
            component_id,
            metadata: Default::default(),
            name,
        })
    }

    pub fn metadata(mut self, metadata: std::collections::HashMap<String, String>) -> Self {
        self.0.metadata = metadata;
        self
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DumpMetadata;

impl Msg for DumpMetadata {
    const ID: PacketId = [224, 14];
}

impl Request for DumpMetadata {
    type Reply<B: IoBuf + Clone> = DumpMetadataResp;
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DumpMetadataResp {
    pub component_metadata: Vec<ComponentMetadata>,
    pub msg_metadata: Vec<MsgMetadata>,
    pub db_config: DbConfig,
}

impl Msg for DumpMetadataResp {
    const ID: PacketId = [224, 15];
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SubscribeLastUpdated;

impl Msg for SubscribeLastUpdated {
    const ID: PacketId = [224, 17];
}

impl Msg for LastUpdated {
    const ID: PacketId = [224, 18];
}

impl Request for SubscribeLastUpdated {
    type Reply<B: IoBuf + Clone> = LastUpdated;
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct SetDbConfig {
    pub recording: Option<bool>,
    pub metadata: HashMap<String, String>,
}

impl SetDbConfig {
    pub fn schematic_content(kdl: String) -> Self {
        SetDbConfig {
            metadata: [("schematic.content".to_string(), kdl)]
                .into_iter()
                .collect(),
            ..Default::default()
        }
    }
}

impl Msg for SetDbConfig {
    const ID: PacketId = [224, 19];
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Resource))]
pub struct DbConfig {
    pub recording: bool,
    pub default_stream_time_step: Duration,
    pub metadata: HashMap<String, String>,
}

impl DbConfig {
    pub fn set_schematic_path(&mut self, path: String) {
        self.metadata.insert("schematic.path".to_string(), path);
    }

    pub fn schematic_path(&self) -> Option<&str> {
        self.metadata.get("schematic.path").map(String::as_str)
    }

    pub fn set_schematic_content(&mut self, path: String) {
        self.metadata.insert("schematic.content".to_string(), path);
    }

    pub fn schematic_content(&self) -> Option<&str> {
        self.metadata.get("schematic.content").map(String::as_str)
    }
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            recording: true,
            default_stream_time_step: Duration::from_millis(10),
            metadata: Default::default(),
        }
    }
}

impl Msg for DbConfig {
    const ID: PacketId = [224, 20];
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GetDbSettings;

impl Msg for GetDbSettings {
    const ID: PacketId = [224, 21];
}

#[derive(Serialize, Deserialize)]
pub struct NewConnection;

impl Msg for NewConnection {
    const ID: PacketId = [225, 1];
}

macro_rules! impl_user_data_msg {
    ($t: ty) => {
        #[cfg(feature = "mlua")]
        impl mlua::UserData for $t {
            fn add_methods<T: mlua::UserDataMethods<Self>>(methods: &mut T) {
                methods.add_method("msg", |_, this, ()| {
                    use metor_proto::types::IntoLenPacket;
                    let msg = this.into_len_packet().inner;
                    Ok(msg)
                });
            }
        }
        #[cfg(feature = "mlua")]
        impl mlua::FromLua for $t {
            fn from_lua(value: mlua::Value, lua: &mlua::Lua) -> mlua::Result<Self> {
                mlua::LuaSerdeExt::from_value(lua, value)
            }
        }
    };
}

impl_user_data_msg!(VTableStream);
impl_user_data_msg!(VTableMsg);
impl_user_data_msg!(Stream);
impl_user_data_msg!(MsgStream);
impl_user_data_msg!(SetStreamState);
impl_user_data_msg!(SetComponentMetadata);
impl_user_data_msg!(UdpUnicast);
impl_user_data_msg!(UdpVTableStream);

#[derive(Serialize, Deserialize)]
pub struct GetEarliestTimestamp;

impl Msg for GetEarliestTimestamp {
    const ID: PacketId = [224, 22];
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Resource))]
pub struct EarliestTimestamp(pub Timestamp);

impl Msg for EarliestTimestamp {
    const ID: PacketId = [224, 23];
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Resource))]
pub struct DumpSchema;

impl Msg for DumpSchema {
    const ID: PacketId = [224, 24];
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DumpSchemaResp {
    pub schemas: HashMap<ComponentId, Schema<Vec<u64>>>,
}

impl Msg for DumpSchemaResp {
    const ID: PacketId = [224, 25];
}

impl Request for DumpSchema {
    type Reply<B: IoBuf + Clone> = DumpSchemaResp;
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct StreamTimestamp {
    pub timestamp: Timestamp,
    pub stream_id: StreamId,
}

impl Msg for StreamTimestamp {
    const ID: PacketId = [224, 26];
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[repr(transparent)]
pub struct SQLQuery(pub String);

impl Msg for SQLQuery {
    const ID: PacketId = [224, 27];
}

impl_user_data_msg!(SQLQuery);

#[derive(Clone, Serialize, Deserialize, Debug)]
#[repr(transparent)]
pub struct ArrowIPC<'a> {
    pub batch: Option<Cow<'a, [u8]>>,
}

impl Msg for ArrowIPC<'_> {
    const ID: PacketId = [224, 28];
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ErrorResponse {
    pub description: String,
}

impl std::fmt::Display for ErrorResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.description)
    }
}

impl Msg for ErrorResponse {
    const ID: PacketId = [224, 29];
}

impl Request for SQLQuery {
    type Reply<B: IoBuf + Clone> = ArrowIPC<'static>;
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MsgMetadata {
    pub name: String,
    pub schema: OwnedNamedType,
    pub metadata: HashMap<String, String>,
}

impl Msg for MsgMetadata {
    const ID: PacketId = [224, 30];
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SetMsgMetadata {
    pub id: PacketId,
    pub metadata: MsgMetadata,
}

impl Msg for SetMsgMetadata {
    const ID: PacketId = [224, 31];
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
pub struct MsgStream {
    pub msg_id: PacketId,
}

impl Request for MsgStream {
    type Reply<B: IoBuf + Clone> = MsgBuf<B>;
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
pub struct FixedRateMsgStream {
    pub msg_id: PacketId,
    pub fixed_rate: FixedRateOp,
}

impl Request for FixedRateMsgStream {
    type Reply<B: IoBuf + Clone> = MsgBuf<B>;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetMsgMetadata {
    pub msg_id: PacketId,
}

impl Msg for GetMsgMetadata {
    const ID: PacketId = [224, 33];
}

impl Request for GetMsgMetadata {
    type Reply<B: IoBuf + Clone> = MsgMetadata;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GetMsgs {
    pub msg_id: PacketId,
    pub range: Range<Timestamp>,
    pub limit: Option<usize>,
}

impl Msg for GetMsgs {
    const ID: PacketId = [224, 34];
}

impl Request for GetMsgs {
    type Reply<B: IoBuf + Clone> = MsgBatch;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MsgBatch {
    pub data: Vec<(Timestamp, Vec<u8>)>,
}

impl Msg for MsgBatch {
    const ID: PacketId = [224, 35];
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
pub struct UdpUnicast {
    pub stream: Stream,
    pub addr: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
pub struct UdpVTableStream {
    pub id: PacketId,
    pub addr: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
pub struct SaveArchive {
    pub path: PathBuf,
    pub format: ArchiveFormat,
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
pub struct ArchiveSaved {
    pub path: PathBuf,
}

impl Request for SaveArchive {
    type Reply<B: IoBuf + Clone> = ArchiveSaved;
}

#[derive(Serialize, Deserialize, Debug, Clone, postcard_schema::Schema)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    ArrowIpc,
    Parquet,
    Csv,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, postcard_schema::Schema)]
pub struct MeanOp {
    pub window: u16,
}

// `ComponentValue` lives in the `nox`-gated `value` module, so the message that
// carries one is gated with it (a no-default-features build has no such type).
#[cfg(feature = "nox")]
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct UpdateComponent {
    pub id: ComponentId,
    pub value: crate::ComponentValue,
}

#[cfg(feature = "nox")]
impl UpdateComponent {
    pub fn new(id: impl Into<ComponentId>, value: impl Into<crate::ComponentValue>) -> Self {
        Self {
            id: id.into(),
            value: value.into(),
        }
    }
}

#[cfg(feature = "nox")]
impl Msg for UpdateComponent {
    const ID: PacketId = [224, 36];
}

/// Stable, human-readable identity for an alarm definition (e.g. `"BATT_OVERTEMP"`).
/// Re-publishing an [`AlarmDef`] with the same id updates it; the latest wins.
pub type AlarmId = String;

/// Unique id for one *firing* of an alarm. Pairs an [`AlarmRaised`] with its later
/// [`AlarmCleared`] / [`AlarmAck`], so the same definition can fire repeatedly without
/// ambiguity.
pub type OccurrenceId = u64;

/// Alarm severity, ordered low → high so consumers can compute the highest active level.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

/// Which side of a value a display limit sits on. A band is expressed as an `Upper`
/// plus a `Lower` entry.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Upper,
    Lower,
}

/// A single limit line to *display* on a plot. Informational only — limits describe
/// where to draw guide lines, they are **not** the boundary that decides firing. The
/// control system evaluates firing (hysteresis, debounce, rate, …) and reports it via
/// [`AlarmRaised`].
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct AlarmLimit {
    pub kind: LimitKind,
    pub value: f64,
    pub severity: Severity,
    pub label: Option<String>,
}

/// The component (and optional element index) an alarm pertains to, enabling plots to
/// auto-associate limit lines and tinting with the traces that show that data.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct AlarmTarget {
    pub component_id: ComponentId,
    pub element_index: Option<u64>,
}

/// Declaration / description of an alarm, broadcast by the control system. Carries the
/// human-readable identity and the informational display limits. The event time is the
/// message-log timestamp.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct AlarmDef {
    pub id: AlarmId,
    pub name: String,
    pub description: String,
    pub target: Option<AlarmTarget>,
    pub limits: Vec<AlarmLimit>,
    pub default_severity: Severity,
}

/// The full alarm definition set, published as one latest-wins snapshot so
/// the downlink can retain it for late-joining connections — a per-def
/// stream would coalesce to the last alarm under snapshot delivery.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct AlarmDefs {
    pub defs: Vec<AlarmDef>,
}

/// The source of truth that an alarm is *firing*, broadcast by the control system. The
/// event time is the message-log timestamp.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct AlarmRaised {
    pub def_id: AlarmId,
    pub occurrence: OccurrenceId,
    pub severity: Severity,
    pub value: Option<f64>,
    pub message: String,
}

/// Marks a previously raised [`occurrence`](AlarmRaised::occurrence) as resolved,
/// broadcast by the control system.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct AlarmCleared {
    pub def_id: AlarmId,
    pub occurrence: OccurrenceId,
}

/// Operator acknowledgment of an alarm occurrence, published by the panel so every
/// connected client sees the ack.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct AlarmAck {
    pub def_id: AlarmId,
    pub occurrence: OccurrenceId,
    pub operator: String,
    pub note: Option<String>,
}

/// Human-readable, unique channel (slot instance) name, e.g. `"adcs"` — the stable
/// address of a sequence channel. A channel is a slot that holds at most one loaded
/// sequence at a time. Names are capped at [`SEQUENCE_CHANNEL_NAME_CAP`] bytes so they
/// round-trip losslessly into the fixed-size telemetry frames that also carry them; the
/// control system validates the cap at build, and an inbound command whose channel
/// exceeds it simply matches no slot.
pub type SequenceChannelName = String;

/// Byte cap on a [`SequenceChannelName`] — the validation invariant, not a wire
/// encoding (the name is an ordinary postcard `String` on the wire).
pub const SEQUENCE_CHANNEL_NAME_CAP: usize = 48;

/// Human-readable name of a sequence, e.g. `"deploy_solar_array"`. Sequences are
/// referenced by name when loaded into a channel.
pub type SequenceName = String;

/// One channel slot declared by the control system: its name (the channel's stable
/// address) and the set of sequence names that may be loaded into it.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct SequenceChannelSpec {
    pub name: SequenceChannelName,
    pub available: Vec<SequenceName>,
}

/// Whole-registry declaration of the sequence channels, broadcast by the control system.
/// Re-publishing replaces the registry — the latest wins. This is the configuration the
/// sequence UI sources from the control system.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct SequenceRegistry {
    pub channels: Vec<SequenceChannelSpec>,
}

/// Run state of a channel's loaded sequence, as reported by the control system.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SequenceRunState {
    Idle,
    Running,
    Stopped,
    Aborted,
    Completed,
    Failed,
}

/// A single transition in a channel's lifecycle. Events arrive in order through one log,
/// so the per-channel state machine (`Loaded` → `Started` → `Progress`* → terminal) is
/// totally ordered.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SequenceEventKind {
    /// A sequence was loaded into the channel.
    Loaded {
        name: SequenceName,
    },
    /// The channel's loaded sequence was cleared.
    Unloaded,
    Started,
    /// The sequence made progress — sent a message, advanced a step, etc. The `detail`
    /// becomes the channel's latest status line.
    Progress {
        detail: String,
    },
    /// Hard-stopped (dropped). May have left the system in an unsafe state.
    Stopped,
    /// Commanded safe-termination ran to completion.
    Aborted,
    Completed,
    Failed {
        reason: String,
    },
    /// An occupant load began; the matching `Loaded` reports its completion. Emitted only
    /// when the load has a real window (a process occupant's spawn + bind); in-process
    /// loads complete synchronously, so consumers may see a `Loaded` with no preceding
    /// `Loading`.
    Loading {
        name: SequenceName,
    },
    /// A command was rejected by the channel's state machine and changed nothing —
    /// e.g. `Load` while running, or `Start` with no live occupant. Unlike `Failed`
    /// this is not a run-state transition; the channel stays where it was.
    Refused {
        reason: String,
    },
}

/// A granular per-channel state update, broadcast by the control system (control → panel).
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct SequenceChannelEvent {
    pub channel: SequenceChannelName,
    pub kind: SequenceEventKind,
}

/// An operator command on a channel, published by the panel (panel → control). The
/// control system executes it and reports the result via [`SequenceChannelEvent`].
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum SequenceCommandKind {
    /// Load the named sequence into the channel.
    Load {
        name: SequenceName,
    },
    Start,
    /// Commanded safe-termination.
    Abort,
    /// Hard-stop (drop) — may leave the system unsafe.
    Stop,
    /// Rebuild the loaded sequence from the beginning, returning the channel to a ready
    /// (idle) state. Only valid from a terminal `Completed`/`Aborted` state.
    Reset,
}

/// A command targeting one channel by name, published by the panel.
///
/// **Retired ids [224,41], [224,42], [224,43]:** the original
/// `SequenceRegistry`/`SequenceChannelEvent`/`SequenceCommand` addressed channels by a
/// `channel_id: u64` (the slot's build-order index — reordering a target file silently
/// re-addressed commands). The name-addressed types changed the postcard layout, so they
/// took **fresh** `PacketId`s ([224,58..60]) rather than mis-decoding every historical
/// recording persisted under the old ids. The retired ids are reserved forever: an old
/// recording's records must stay cleanly *unmatched*, never reinterpreted.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct SequenceCommand {
    pub channel: SequenceChannelName,
    pub command: SequenceCommandKind,
}

/// Asks the control system to re-read its sequence source(s) (disk, etc.) and re-publish
/// an updated [`SequenceRegistry`]. Global scope. Published by the panel.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone, Default)]
pub struct ReloadSequences {}

/// The complete target wiring IR, broadcast by the control system at startup
/// and re-broadcast on a [`ReloadSequences`] request — the same telemetry
/// pub/sub pattern [`SequenceRegistry`] uses. The payload is the versioned
/// `Wiring` serialized as JSON (the self-describing §6 wire format), so a
/// consumer decodes the whole topology — systems, slots, edges, scopes, and
/// source anchors — with `serde_json` and never needs bundle access. The JSON
/// crosses the ring as a plain postcard `String`; `ir_version` rides alongside
/// it so a consumer can gate before parsing. Because the payload carries the
/// full graph it can exceed the default message-ring cap, so the control
/// system sizes its `wiring` channel from the concrete payload at build.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct WiringManifest {
    /// The IR schema version the `ir_json` was produced against.
    pub ir_version: u32,
    /// The full `Wiring` IR as JSON.
    pub ir_json: String,
}

/// Log severity, ordered low → high so consumers can filter by minimum level.
/// Mirrors the `tracing` level set.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

/// One log line from the flight software, broadcast on the downlink
/// (control → panel). Carries both hand-written lines (a system's health-port
/// log call) and host `tracing` events routed through the forwarding layer.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct LogEvent {
    /// Source-side event time on the FSW clock: health-port lines and
    /// forwarded tracing events are both stamped with the cycle timestamp of
    /// the cycle that flushed them (wall time equals it only under a wall
    /// clock).
    pub timestamp: Timestamp,
    pub level: LogLevel,
    /// Filter key: the emitting system's instance name for health-port lines,
    /// the tracing target for forwarded events.
    pub source: String,
    /// The tracing target (module path); empty for health-port lines.
    pub target: String,
    pub message: String,
    /// Active tracing span scope, root → leaf joined with `:` (e.g.
    /// `"run:load_slot"`). `None` for health-port lines.
    pub span: Option<String>,
    /// Event key-values beyond the message, Debug-formatted.
    pub fields: Vec<(String, String)>,
    /// Source location of the emitting tracing event, when known.
    pub file: Option<String>,
    pub line: Option<u32>,
}

/// Version of the sealed-node transfer protocol ([224,45]..[224,57]).
/// Clients MUST handshake with [`GetDbInfo`] before sending any of these:
/// servers that predate the handshake record unknown request messages as
/// telemetry, so probing with anything else pollutes the remote DB.
/// Messages added after version 1 are additionally feature-gated — see
/// [`DbInfoResp::features`].
pub const NODE_PROTOCOL_VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct GetDbInfo;

impl Msg for GetDbInfo {
    const ID: PacketId = [224, 45];
}

impl Request for GetDbInfo {
    type Reply<B: IoBuf + Clone> = DbInfoResp;
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct DbInfoResp {
    pub protocol_version: u32,
    /// Capability bits; clients gate post-version-1 requests on these.
    /// Bit 0 was the retired envelope protocol ([224,56]/[224,57]) and
    /// must not be reused.
    pub features: u64,
}

impl Msg for DbInfoResp {
    const ID: PacketId = [224, 46];
}

/// Durable summary of one sealed storage node. Defined here because it is
/// both the storage-side seal sidecar and the wire-level manifest entry —
/// `index_len`/`data_len` say exactly which committed bytes are claimed
/// and `checksum` (xxh3_64 over index bytes then data bytes) lets any
/// holder verify a transfer without trusting the source.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealRecord {
    pub start_ts: Timestamp,
    pub end_ts: Timestamp,
    pub count: u64,
    pub index_len: u64,
    pub data_len: u64,
    pub checksum: u64,
    pub element_size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct GetNodeManifest {
    pub component_id: ComponentId,
}

impl Msg for GetNodeManifest {
    const ID: PacketId = [224, 47];
}

impl Request for GetNodeManifest {
    type Reply<B: IoBuf + Clone> = NodeManifestResp;
}

/// Every sealed node the server can currently serve for the component,
/// sorted by start timestamp. The live (unsealed) head is never listed —
/// its samples arrive via streaming instead.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NodeManifestResp {
    pub component_id: ComponentId,
    pub nodes: Vec<SealRecord>,
}

impl Msg for NodeManifestResp {
    const ID: PacketId = [224, 48];
}

/// Ask the server to stream one sealed node's payloads as [`NodeChunk`]s
/// (in order, index file first) followed by a [`FetchNodeDone`], all under
/// this request's id. An [`crate::ErrorResponse`] means the node is gone
/// (e.g. purged since the manifest was fetched).
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct FetchNode {
    pub component_id: ComponentId,
    pub start_ts: Timestamp,
    /// Requested chunk payload size; servers clamp to sane bounds.
    pub chunk_bytes: u32,
}

impl Msg for FetchNode {
    const ID: PacketId = [224, 49];
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeFileKind {
    Index,
    Data,
}

/// One streamed piece of a sealed node's committed payload. Used in both
/// directions: server→client answering [`FetchNode`], client→server
/// uploading after an accepted [`OfferNode`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NodeChunk {
    pub component_id: ComponentId,
    pub start_ts: Timestamp,
    pub file: NodeFileKind,
    pub offset: u64,
    pub payload: Vec<u8>,
}

impl Msg for NodeChunk {
    const ID: PacketId = [224, 50];
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct FetchNodeDone {
    pub seal: SealRecord,
}

impl Msg for FetchNodeDone {
    const ID: PacketId = [224, 51];
}

/// Offer a sealed node for archival. The server auto-creates the
/// component from `schema` if it has never seen it. `already_have` short
/// circuits re-uploads by checksum.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OfferNode {
    pub component_id: ComponentId,
    pub component_name: String,
    pub schema: Schema<Vec<u64>>,
    pub seal: SealRecord,
}

impl Msg for OfferNode {
    const ID: PacketId = [224, 52];
}

impl Request for OfferNode {
    type Reply<B: IoBuf + Clone> = OfferNodeResp;
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct OfferNodeResp {
    pub accept: bool,
    pub already_have: bool,
}

impl Msg for OfferNodeResp {
    const ID: PacketId = [224, 53];
}

/// Sent by the uploader after the last [`NodeChunk`] of an offered node.
/// The server verifies, persists durably (fsync), and answers with a
/// [`NodeAck`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct PushNodeDone {
    pub component_id: ComponentId,
    pub start_ts: Timestamp,
    pub checksum: u64,
}

impl Msg for PushNodeDone {
    const ID: PacketId = [224, 54];
}

impl Request for PushNodeDone {
    type Reply<B: IoBuf + Clone> = NodeAck;
}

/// `durable: true` is the only signal that permits the uploader to purge
/// its local copy — it means the bytes are on the server's disk, fsynced,
/// and checksum-verified.
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct NodeAck {
    pub component_id: ComponentId,
    pub start_ts: Timestamp,
    pub checksum: u64,
    pub durable: bool,
}

impl Msg for NodeAck {
    const ID: PacketId = [224, 55];
}

/// Version of the fsw link identity protocol ([`LinkInfo`]), independent
/// of [`NODE_PROTOCOL_VERSION`].
pub const LINK_PROTOCOL_VERSION: u32 = 1;

/// Identity push from an fsw link server: the FIRST packet on every
/// accepted connection, ahead of the schema announce replay. A metor-db
/// server never sends it, and an fsw link server never answers
/// [`GetDbInfo`] — the pair is how one address form serves both peers: a
/// client probes with `GetDbInfo` and lets the first packet's id decide
/// the mode.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LinkInfo {
    pub protocol_version: u32,
    /// Capability bits, reserved (0 for now).
    pub features: u64,
    /// The uplink's configured command set: the msg ids a ground client
    /// should forward up the link. The uplink's minted ports are
    /// untelemetered, so forwarding these can never echo back down.
    pub command_ids: Vec<PacketId>,
}

impl Msg for LinkInfo {
    const ID: PacketId = [224, 61];
}

/// DNS-SD service type an fsw link advertises itself under, for mDNS
/// discovery on the local link. A ground tool browses this type to find
/// every reachable fsw by name.
pub const FSW_SERVICE_TYPE: &str = "_metor-fsw._tcp.local.";

/// TXT-record key for the advertised [`LINK_PROTOCOL_VERSION`], so a
/// browser can skip an incompatible link before it dials.
pub const TXT_PROTOCOL_VERSION: &str = "pv";

/// TXT-record key for the fsw's telemetry namespace, when it has one — the
/// same bare dotted prefix its components carry.
pub const TXT_NAMESPACE: &str = "ns";

/// TXT-record key naming the advertised role, always `"fsw"` for a link
/// server; reserved so one service type can grow sibling roles later.
pub const TXT_ROLE: &str = "role";

/// Every message of the node/link control protocols. Servers consult
/// this to keep stray protocol messages out of recorded telemetry; other
/// unmatched messages (alarms, sequences, …) are telemetry by design —
/// the msg log is their pub/sub channel.
pub const NODE_PROTOCOL_MESSAGES: &[PacketId] = &[
    GetDbInfo::ID,
    DbInfoResp::ID,
    GetNodeManifest::ID,
    NodeManifestResp::ID,
    FetchNode::ID,
    NodeChunk::ID,
    FetchNodeDone::ID,
    OfferNode::ID,
    OfferNodeResp::ID,
    PushNodeDone::ID,
    NodeAck::ID,
    // The retired envelope protocol (GetEnvelope/EnvelopeResp). Reserved
    // forever: an old client's stray request must stay ignored here, not
    // get recorded as telemetry.
    [224, 56],
    [224, 57],
    LinkInfo::ID,
];

#[cfg(test)]
mod alarm_tests {
    use super::*;

    fn roundtrip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let bytes = postcard::to_allocvec(value).expect("encode");
        postcard::from_bytes(&bytes).expect("decode")
    }

    #[test]
    fn alarm_messages_roundtrip() {
        let def = AlarmDef {
            id: "BATT_OVERTEMP".into(),
            name: "Battery Over-Temperature".into(),
            description: "Pack temperature exceeded safe range".into(),
            target: Some(AlarmTarget {
                component_id: ComponentId::new("power.batt_temp"),
                element_index: Some(0),
            }),
            limits: vec![
                AlarmLimit {
                    kind: LimitKind::Upper,
                    value: 80.0,
                    severity: Severity::Warning,
                    label: Some("Warn".into()),
                },
                AlarmLimit {
                    kind: LimitKind::Upper,
                    value: 95.0,
                    severity: Severity::Critical,
                    label: Some("Crit".into()),
                },
            ],
            default_severity: Severity::Warning,
        };
        let def2 = roundtrip(&def);
        assert_eq!(def.id, def2.id);
        assert_eq!(def.limits.len(), def2.limits.len());
        assert_eq!(def.default_severity, def2.default_severity);

        let raised = AlarmRaised {
            def_id: "BATT_OVERTEMP".into(),
            occurrence: 42,
            severity: Severity::Critical,
            value: Some(96.2),
            message: "temp 96.2C".into(),
        };
        let raised2 = roundtrip(&raised);
        assert_eq!(raised.occurrence, raised2.occurrence);
        assert_eq!(raised.severity, raised2.severity);

        let cleared = AlarmCleared {
            def_id: "BATT_OVERTEMP".into(),
            occurrence: 42,
        };
        assert_eq!(cleared.occurrence, roundtrip(&cleared).occurrence);

        let ack = AlarmAck {
            def_id: "BATT_OVERTEMP".into(),
            occurrence: 42,
            operator: "sphw".into(),
            note: Some("looking into it".into()),
        };
        assert_eq!(ack.operator, roundtrip(&ack).operator);
    }

    #[test]
    fn alarm_packet_ids_are_unique() {
        let ids = [
            AlarmDef::ID,
            AlarmRaised::ID,
            AlarmCleared::ID,
            AlarmAck::ID,
        ];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b, "alarm packet ids must be unique");
            }
        }
        // Distinct from the previously highest assigned id (UpdateComponent, [224,36] —
        // spelled literally: the type is `nox`-gated, its id is reserved regardless).
        for id in ids {
            assert_ne!(id, [224, 36]);
        }
    }

    #[test]
    fn severity_orders_low_to_high() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Critical);
    }
}

#[cfg(test)]
mod sequence_tests {
    use super::*;

    fn roundtrip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let bytes = postcard::to_allocvec(value).expect("encode");
        postcard::from_bytes(&bytes).expect("decode")
    }

    #[test]
    fn sequence_messages_roundtrip() {
        let registry = SequenceRegistry {
            channels: vec![
                SequenceChannelSpec {
                    name: "deploy".into(),
                    available: vec!["deploy_solar_array".into(), "deploy_antenna".into()],
                },
                SequenceChannelSpec {
                    name: "attitude".into(),
                    available: vec!["detumble".into()],
                },
            ],
        };
        let registry2 = roundtrip(&registry);
        assert_eq!(registry.channels.len(), registry2.channels.len());
        assert_eq!(registry.channels[0].name, registry2.channels[0].name);
        assert_eq!(
            registry.channels[0].available,
            registry2.channels[0].available
        );

        let event = SequenceChannelEvent {
            channel: "deploy".into(),
            kind: SequenceEventKind::Loaded {
                name: "deploy_solar_array".into(),
            },
        };
        assert_eq!(event.channel, roundtrip(&event).channel);

        let progress = SequenceChannelEvent {
            channel: "deploy".into(),
            kind: SequenceEventKind::Progress {
                detail: "panel 1 latched".into(),
            },
        };
        assert!(matches!(
            roundtrip(&progress).kind,
            SequenceEventKind::Progress { .. }
        ));

        let command = SequenceCommand {
            channel: "attitude".into(),
            command: SequenceCommandKind::Load {
                name: "detumble".into(),
            },
        };
        assert_eq!(command.channel, roundtrip(&command).channel);

        let reset = SequenceCommand {
            channel: "attitude".into(),
            command: SequenceCommandKind::Reset,
        };
        assert!(matches!(
            roundtrip(&reset).command,
            SequenceCommandKind::Reset
        ));

        let _ = roundtrip(&ReloadSequences {});
    }

    /// Postcard goldens: the exact wire bytes of the three name-addressed Msgs, so the
    /// encoding cannot drift silently (a name is a varint-length postcard `String`).
    #[test]
    fn sequence_messages_pinned_bytes() {
        let registry = SequenceRegistry {
            channels: vec![SequenceChannelSpec {
                name: "adcs".into(),
                available: vec!["detumble".into()],
            }],
        };
        assert_eq!(
            postcard::to_allocvec(&registry).unwrap(),
            // 1 channel; name "adcs" (len 4); 1 available; "detumble" (len 8).
            [
                1, 4, b'a', b'd', b'c', b's', 1, 8, b'd', b'e', b't', b'u', b'm', b'b', b'l', b'e'
            ]
        );

        let event = SequenceChannelEvent {
            channel: "adcs".into(),
            kind: SequenceEventKind::Started,
        };
        assert_eq!(
            postcard::to_allocvec(&event).unwrap(),
            // channel "adcs"; kind variant 2 (Started).
            [4, b'a', b'd', b'c', b's', 2]
        );

        let loading = SequenceChannelEvent {
            channel: "adcs".into(),
            kind: SequenceEventKind::Loading {
                name: "detumble".into(),
            },
        };
        assert_eq!(
            postcard::to_allocvec(&loading).unwrap(),
            // channel "adcs"; kind variant 8 (Loading, appended after Failed so the
            // earlier variants keep their wire form); name "detumble" (len 8).
            [
                4, b'a', b'd', b'c', b's', 8, 8, b'd', b'e', b't', b'u', b'm', b'b', b'l', b'e'
            ]
        );

        let command = SequenceCommand {
            channel: "adcs".into(),
            command: SequenceCommandKind::Start,
        };
        assert_eq!(
            postcard::to_allocvec(&command).unwrap(),
            // channel "adcs"; command variant 1 (Start).
            [4, b'a', b'd', b'c', b's', 1]
        );
    }

    #[test]
    fn sequence_packet_ids_are_unique() {
        let ids = [
            SequenceRegistry::ID,
            SequenceChannelEvent::ID,
            SequenceCommand::ID,
            ReloadSequences::ID,
        ];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b, "sequence packet ids must be unique");
            }
        }
        // Distinct from the alarm ids that precede them.
        for id in ids {
            assert_ne!(id, AlarmAck::ID);
        }
        // The fresh name-addressed ids must not collide with the sealed-node transfer
        // protocol ([224,45..57], incl. the retired envelope pair) nor resurrect the
        // retired u64-`channel_id` ids [224,41..43].
        for id in [
            SequenceRegistry::ID,
            SequenceChannelEvent::ID,
            SequenceCommand::ID,
        ] {
            assert!(
                !NODE_PROTOCOL_MESSAGES.contains(&id),
                "{id:?} collides with the node protocol"
            );
            assert!(
                ![[224u8, 41], [224, 42], [224, 43]].contains(&id),
                "{id:?} resurrects a retired sequence id"
            );
        }
    }

    /// The pinned ids themselves. These are the natural schema-name-hash ids
    /// the blanket `Msg` impl assigns; the pin catches a type rename, which
    /// would silently re-key every persisted recording. (The old hand-held
    /// ids [224,58..60]/[224,44] are retired: historical records stay cleanly
    /// unmatched.)
    #[test]
    fn sequence_packet_ids_are_pinned() {
        assert_eq!(SequenceRegistry::ID, [78, 109]);
        assert_eq!(SequenceChannelEvent::ID, [133, 5]);
        assert_eq!(SequenceCommand::ID, [225, 247]);
        assert_eq!(ReloadSequences::ID, [209, 95]);
    }
}

#[cfg(test)]
mod alarm_id_tests {
    use super::*;

    /// The pinned natural schema-name-hash ids; the pin catches a type
    /// rename, which would silently re-key every persisted recording.
    #[test]
    fn alarm_ids_are_pinned() {
        assert_eq!(AlarmDef::ID, [178, 74]);
        assert_eq!(AlarmRaised::ID, [217, 65]);
        assert_eq!(AlarmCleared::ID, [91, 221]);
        assert_eq!(AlarmAck::ID, [253, 188]);
        for id in [AlarmDef::ID, AlarmRaised::ID, AlarmCleared::ID, AlarmAck::ID] {
            assert!(!NODE_PROTOCOL_MESSAGES.contains(&id));
        }
    }
}

#[cfg(test)]
mod wiring_manifest_tests {
    use super::*;

    #[test]
    fn wiring_manifest_roundtrips() {
        let manifest = WiringManifest {
            ir_version: 1,
            ir_json: r#"{"ir_version":1,"systems":[]}"#.into(),
        };
        let bytes = postcard::to_allocvec(&manifest).expect("encode");
        let back: WiringManifest = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back.ir_version, manifest.ir_version);
        assert_eq!(back.ir_json, manifest.ir_json);
    }

    /// The pinned id — the natural schema-name-hash id; the pin catches a
    /// type rename, which would silently re-key every persisted recording —
    /// and its distinctness from every other assigned id in this module.
    #[test]
    fn wiring_manifest_id_is_pinned_and_unique() {
        assert_eq!(WiringManifest::ID, [9, 81]);
        for id in [
            SequenceRegistry::ID,
            SequenceChannelEvent::ID,
            SequenceCommand::ID,
            ReloadSequences::ID,
            AlarmDef::ID,
            AlarmDefs::ID,
            AlarmRaised::ID,
            AlarmCleared::ID,
            AlarmAck::ID,
        ] {
            assert_ne!(id, WiringManifest::ID);
        }
        assert!(!NODE_PROTOCOL_MESSAGES.contains(&WiringManifest::ID));
    }
}

#[cfg(test)]
mod log_event_tests {
    use super::*;

    #[test]
    fn log_event_roundtrips() {
        let ev = LogEvent {
            timestamp: Timestamp(42),
            level: LogLevel::Warn,
            source: "nav".into(),
            target: "metor_fsw2::coordinator".into(),
            message: "slot load failed".into(),
            span: Some("run:load_slot".into()),
            fields: vec![("slot".into(), "nav".into())],
            file: Some("src/coordinator/slot.rs".into()),
            line: Some(690),
        };
        let bytes = postcard::to_allocvec(&ev).expect("encode");
        let back: LogEvent = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back.level, ev.level);
        assert_eq!(back.source, ev.source);
        assert_eq!(back.message, ev.message);
        assert_eq!(back.span, ev.span);
        assert_eq!(back.fields, ev.fields);
    }

    /// The pinned id — the natural schema-name-hash id; the pin catches a
    /// type rename, which would silently re-key every persisted recording —
    /// and its distinctness from every other assigned id in this module.
    #[test]
    fn log_event_id_is_pinned_and_unique() {
        assert_eq!(LogEvent::ID, [7, 209]);
        for id in [
            SequenceRegistry::ID,
            SequenceChannelEvent::ID,
            SequenceCommand::ID,
            ReloadSequences::ID,
            WiringManifest::ID,
            AlarmDef::ID,
            AlarmDefs::ID,
            AlarmRaised::ID,
            AlarmCleared::ID,
            AlarmAck::ID,
        ] {
            assert_ne!(id, LogEvent::ID);
        }
        assert!(!NODE_PROTOCOL_MESSAGES.contains(&LogEvent::ID));
        // Levels order low → high for min-level filtering.
        assert!(LogLevel::Trace < LogLevel::Debug && LogLevel::Warn < LogLevel::Error);
    }
}

#[cfg(test)]
mod link_info_tests {
    use super::*;

    /// The pinned id — hand-allocated past the retired [224,58..=60] range —
    /// and its stray-drop membership: both servers key their
    /// never-record-as-telemetry hygiene off [`NODE_PROTOCOL_MESSAGES`].
    #[test]
    fn link_info_id_is_pinned_and_protocol_member() {
        assert_eq!(LinkInfo::ID, [224, 61]);
        assert!(NODE_PROTOCOL_MESSAGES.contains(&LinkInfo::ID));
    }

    #[test]
    fn link_info_round_trips() {
        let info = LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids: vec![SequenceCommand::ID, AlarmAck::ID],
        };
        let bytes = postcard::to_allocvec(&info).expect("encode");
        let back: LinkInfo = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back, info);
    }
}
