use metor_proto::{
    buf::IoBuf,
    schema::Schema,
    types::{
        ComponentId, Msg, MsgBuf, OwnedTable, OwnedTimeSeries, PacketId, Request, Timestamp,
        TryFromPacket, short_id,
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
    const ID: PacketId = short_id([224, 2]);
}

/// Asks a db for one component's samples within `range` (end exclusive),
/// in a [`TimeSeriesReply`]. Gated on [`FEATURE_RANGE_FETCH`]: older servers
/// read only the leading fields and reply in a layout without `covered`.
///
/// Every way a client reads history it doesn't hold goes through this one
/// request: pages of a range (`from: Oldest`, paging on `covered.end`), the
/// sample shown at an instant (`range: ..t + 1, from: Newest, limit: 1`),
/// and a decimated pick for fast playback (`stride`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GetTimeSeries {
    pub id: PacketId,
    pub range: Range<Timestamp>,
    pub component_id: ComponentId,
    /// At most this many samples, counted from `from`. A timestamp is never
    /// split across replies, so a reply can hold more when the limit falls
    /// inside a run of equal timestamps.
    pub limit: Option<usize>,
    /// Which end of `range` the reply starts from, and so which end its
    /// `covered` range keeps when the limit or size budget cuts it short.
    pub from: SeriesEnd,
    /// At most one sample per epoch-aligned cell of this width (cell `k`
    /// is `[k * stride, (k + 1) * stride)` in microseconds): the last real
    /// sample in the cell, unchanged. Aligned like the LoD buckets, so
    /// replies to different requests merge. `limit` then counts cells.
    pub stride: Option<Duration>,
}

impl GetTimeSeries {
    /// The oldest samples of `range`, as many as fit one reply.
    pub fn range(component_id: ComponentId, range: Range<Timestamp>) -> Self {
        Self {
            id: short_id([0, 0]),
            range,
            component_id,
            limit: None,
            from: SeriesEnd::Oldest,
            stride: None,
        }
    }

    /// The sample a reader shows at `t`: the newest at or before it.
    pub fn instant(component_id: ComponentId, t: Timestamp) -> Self {
        Self {
            from: SeriesEnd::Newest,
            limit: Some(1),
            ..Self::range(
                component_id,
                Timestamp(i64::MIN)..Timestamp(t.0.saturating_add(1)),
            )
        }
    }

    pub fn limit(self, limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..self
        }
    }

    pub fn newest(self) -> Self {
        Self {
            from: SeriesEnd::Newest,
            ..self
        }
    }

    pub fn stride(self, stride: Duration) -> Self {
        Self {
            stride: Some(stride),
            ..self
        }
    }
}

/// The end of a range a [`GetTimeSeries`] counts its limit from.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SeriesEnd {
    /// From the start forward: pages of history, oldest first.
    #[default]
    Oldest,
    /// From the end backward: the latest samples before a point.
    Newest,
}

impl Msg for GetTimeSeries {
    const ID: PacketId = short_id([224, 3]);
}

impl Request for GetTimeSeries {
    type Reply<B: IoBuf + Clone> = OwnedTimeSeries<B>;
}

/// What a [`GetTimeSeries`] gets back: an [`OwnedTimeSeries`], whose
/// `covered` range is the stretch the server vouches for.
pub type TimeSeriesReply<B> = OwnedTimeSeries<B>;

#[derive(Serialize, Deserialize)]
pub struct SchemaMsg(pub Schema<Vec<u64>>);
impl Msg for SchemaMsg {
    const ID: PacketId = short_id([224, 4]);
}

#[derive(Serialize, Deserialize)]
pub struct GetSchema {
    pub component_id: ComponentId,
}

impl Msg for GetSchema {
    const ID: PacketId = short_id([224, 5]);
}

impl Request for GetSchema {
    type Reply<B: IoBuf + Clone> = SchemaMsg;
}

#[derive(Clone, Serialize, Deserialize)]
pub struct GetComponentMetadata {
    pub component_id: ComponentId,
}

impl Msg for GetComponentMetadata {
    const ID: PacketId = short_id([224, 6]);
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
    const ID: PacketId = short_id([224, 14]);
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
    const ID: PacketId = short_id([224, 15]);
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SubscribeLastUpdated;

impl Msg for SubscribeLastUpdated {
    const ID: PacketId = short_id([224, 17]);
}

impl Msg for LastUpdated {
    const ID: PacketId = short_id([224, 18]);
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
    const ID: PacketId = short_id([224, 19]);
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
    const ID: PacketId = short_id([224, 20]);
}

#[derive(Serialize, Deserialize, Debug)]
pub struct GetDbSettings;

impl Msg for GetDbSettings {
    const ID: PacketId = short_id([224, 21]);
}

#[derive(Serialize, Deserialize)]
pub struct NewConnection;

impl Msg for NewConnection {
    const ID: PacketId = short_id([225, 1]);
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
    const ID: PacketId = short_id([224, 22]);
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Resource))]
pub struct EarliestTimestamp(pub Timestamp);

impl Msg for EarliestTimestamp {
    const ID: PacketId = short_id([224, 23]);
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[cfg_attr(feature = "bevy", derive(bevy::prelude::Resource))]
pub struct DumpSchema;

impl Msg for DumpSchema {
    const ID: PacketId = short_id([224, 24]);
}

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct DumpSchemaResp {
    pub schemas: HashMap<ComponentId, Schema<Vec<u64>>>,
}

impl Msg for DumpSchemaResp {
    const ID: PacketId = short_id([224, 25]);
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
    const ID: PacketId = short_id([224, 26]);
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[repr(transparent)]
pub struct SQLQuery(pub String);

impl Msg for SQLQuery {
    const ID: PacketId = short_id([224, 27]);
}

impl_user_data_msg!(SQLQuery);

#[derive(Clone, Serialize, Deserialize, Debug)]
#[repr(transparent)]
pub struct ArrowIPC<'a> {
    pub batch: Option<Cow<'a, [u8]>>,
}

impl Msg for ArrowIPC<'_> {
    const ID: PacketId = short_id([224, 28]);
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
    const ID: PacketId = short_id([224, 29]);
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
    const ID: PacketId = short_id([224, 30]);
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SetMsgMetadata {
    pub id: PacketId,
    pub metadata: MsgMetadata,
}

impl Msg for SetMsgMetadata {
    const ID: PacketId = short_id([224, 31]);
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
    const ID: PacketId = short_id([224, 33]);
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
    const ID: PacketId = short_id([224, 34]);
}

impl Request for GetMsgs {
    type Reply<B: IoBuf + Clone> = MsgBatch;
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MsgBatch {
    pub data: Vec<(Timestamp, Vec<u8>)>,
}

impl Msg for MsgBatch {
    const ID: PacketId = short_id([224, 35]);
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
    const ID: PacketId = short_id([224, 36]);
}

/// Stable, human-readable identity for an alarm definition (e.g. `"BATT_OVERTEMP"`).
/// Re-publishing an [`AlarmDef`] with the same id updates it; the latest wins.
pub type AlarmId = String;

/// Unique id for one *firing* of an alarm. Pairs an [`AlarmRaised`] with its later
/// [`AlarmCleared`] / [`AlarmAck`], so the same definition can fire repeatedly without
/// ambiguity.
pub type OccurrenceId = u64;

/// Alarm severity, ordered low → high so consumers can compute the highest active level.
#[derive(
    Serialize,
    Deserialize,
    postcard_schema::Schema,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
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

/// Operator suppression of an alarm definition until `until`, published by the panel.
///
/// Shelving is an operator-console function (ISA-18.2): the control system keeps
/// evaluating and reporting the alarm, and never subscribes to this message — it exists
/// so every connected console agrees on which points are shelved and for how long.
/// `until` is wall-clock, produced and compared by the consoles.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct AlarmShelved {
    pub def_id: AlarmId,
    pub until: Timestamp,
    pub reason: Option<String>,
    pub operator: String,
}

/// Operator cancellation of a live [`AlarmShelved`], published by the panel.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct AlarmUnshelved {
    pub def_id: AlarmId,
    pub operator: String,
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

/// One channel declared by the control system: its name (the channel's stable
/// address), the sequences that may be loaded into it, and the components that
/// carry its state.
///
/// The control system telemeters a channel's state as components every cycle,
/// and writes a message component only when something happens; this entry
/// names each of them in full, so a UI reads them like any other component and
/// never derives a name. Commands are the only messages a
/// UI sends, and the registry the only one besides commands it reads.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone, Default)]
pub struct SequenceChannelSpec {
    pub name: SequenceChannelName,
    pub available: Vec<SequenceName>,
    /// The wrapped system's type, shown in place of a sequence for a channel
    /// that loads nothing (`available` is empty).
    pub system: String,
    pub components: SequenceChannelComponents,
}

/// The full component name of each field of a channel's state.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone, Default)]
pub struct SequenceChannelComponents {
    /// A [`SequenceChannelState`] code.
    pub state: String,
    /// The loaded sequence's index in the channel's `available` list, or
    /// `u32::MAX` when nothing is loaded.
    pub loaded: String,
    /// The file name of the loaded occupant's library, or empty.
    pub artifact: String,
    /// The blake3 hash of the loaded occupant's library, or zeros.
    pub hash: String,
    /// The commands the channel would accept now, as [`SequenceAccepts`] bits.
    pub accepts: String,
    /// What happened on the channel: a transition, a refused command, or how
    /// its system ended. It has one sample per message, stamped when
    /// the message was raised, so its samples are the channel's history.
    pub message: String,
    /// Seconds the loaded sequence has run. It pauses while the channel is
    /// stopped and holds once the run ends.
    pub elapsed: String,
    /// Seconds since a commanded abort began, counting while the sequence winds
    /// down gracefully; zero when no abort is in progress.
    pub abort_elapsed: String,
}

/// A channel's state, as its `state` component carries it.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum SequenceChannelState {
    Running = 0,
    Stopped = 1,
    Completed = 2,
    Aborted = 3,
    Failed = 4,
    Panicked = 5,
    Unloaded = 6,
    /// Waiting for the system's work in flight to finish before a reset or
    /// load takes effect.
    Quiescing = 7,
}

impl SequenceChannelState {
    pub const ALL: [Self; 8] = [
        Self::Running,
        Self::Stopped,
        Self::Completed,
        Self::Aborted,
        Self::Failed,
        Self::Panicked,
        Self::Unloaded,
        Self::Quiescing,
    ];

    /// The code the `state` component carries.
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The state a `state` component's code names, if any.
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.code() == code)
    }
}

/// The commands a channel would accept now, one bit per command, as its
/// `accepts` component carries them.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SequenceAccepts(pub u32);

impl SequenceAccepts {
    pub const LOAD: u32 = 1 << 0;
    pub const START: u32 = 1 << 1;
    pub const ABORT: u32 = 1 << 2;
    pub const STOP: u32 = 1 << 3;
    pub const RESET: u32 = 1 << 4;

    /// The bit for `command`.
    pub fn bit(command: &SequenceCommandKind) -> u32 {
        match command {
            SequenceCommandKind::Load { .. } => Self::LOAD,
            SequenceCommandKind::Start => Self::START,
            SequenceCommandKind::Abort => Self::ABORT,
            SequenceCommandKind::Stop => Self::STOP,
            SequenceCommandKind::Reset => Self::RESET,
        }
    }

    /// Whether `command` would be accepted.
    pub fn allows(self, command: &SequenceCommandKind) -> bool {
        self.contains(Self::bit(command))
    }

    /// Whether the command `bit` names would be accepted.
    pub fn contains(self, bit: u32) -> bool {
        self.0 & bit != 0
    }
}

/// Whole-registry declaration of the sequence channels, broadcast by the control system.
/// Re-publishing replaces the registry — the latest wins. This is the configuration the
/// sequence UI sources from the control system.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct SequenceRegistry {
    pub channels: Vec<SequenceChannelSpec>,
}

/// Run state of a channel's loaded sequence, as fsw-2 reports it. Kept only
/// for fsw-2, and deleted with it; fsw-3 reports [`SequenceChannelState`].
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

/// A single transition in a channel's lifecycle, as fsw-2 reports it. Events arrive in
/// order through one log, so the per-channel state machine (`Loaded` → `Started` →
/// `Progress`* → terminal) is totally ordered. Kept only for fsw-2, and deleted with it.
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

/// A granular per-channel state update, broadcast by fsw-2 (control → panel). Kept only
/// for fsw-2, and deleted with it; fsw-3 telemeters the channel's components instead.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct SequenceChannelEvent {
    pub channel: SequenceChannelName,
    pub kind: SequenceEventKind,
}

/// An operator command on a channel, published by the panel (panel → control). The
/// control system executes it, and the channel's components report the result: its
/// `state`, and a `message` saying what happened or why it was refused.
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
    /// (stopped) state. The channel's `accepts` says when it applies.
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

/// One named panel layout a target ships as a recommended default.
///
/// `layout` is a serde_json-encoded [`TileLayout`](crate::TileLayout)
/// document — the tree is recursive, which a postcard schema cannot express,
/// so it crosses the ring as a plain postcard `String`. The document carries
/// its own version for consumers to gate on before hydrating.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct Preset {
    pub name: String,
    pub layout: String,
}

/// The full preset set, published as one latest-wins snapshot so the
/// downlink can retain it for late-joining connections.
#[derive(Serialize, Deserialize, postcard_schema::Schema, Debug, Clone)]
pub struct PresetDefs {
    pub presets: Vec<Preset>,
}

/// Log severity, ordered low → high so consumers can filter by minimum level.
/// Mirrors the `tracing` level set.
#[derive(
    Serialize,
    Deserialize,
    postcard_schema::Schema,
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
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
    pub source: Cow<'static, str>,
    /// The tracing target (module path); empty for health-port lines.
    pub target: Cow<'static, str>,
    pub message: Cow<'static, str>,
    /// Active tracing span scope, root → leaf joined with `:` (e.g.
    /// `"run:load_slot"`). `None` for health-port lines.
    pub span: Option<Cow<'static, str>>,
    /// Event key-values beyond the message, Debug-formatted.
    pub fields: Vec<(Cow<'static, str>, Cow<'static, str>)>,
    /// Source location of the emitting tracing event, when known.
    pub file: Option<Cow<'static, str>>,
    pub line: Option<u32>,
}

/// Version of the sealed-node transfer protocol ([224,45]..[224,57]).
/// Clients MUST handshake with [`GetDbInfo`] before sending any of these:
/// servers that predate the handshake record unknown request messages as
/// telemetry, so probing with anything else pollutes the remote DB.
/// Messages added after version 1 are additionally feature-gated — see
/// [`DbInfoResp::features`].
///
/// Version 7 extended [`GetTimeSeries`] with `from` and `stride`, made its
/// range end exclusive, and added `covered` to the reply
/// ([`FEATURE_RANGE_FETCH`]). That is a hard wire break with no fallback:
/// a version-6 server answers a `GetTimeSeries` in the old layout, which a
/// version-7 client misreads (its `covered` is the first 16 bytes of
/// timestamps), so every client checks the feature bit before asking.
///
/// Version 8 appended `head` to [`NodeManifestResp`]
/// ([`FEATURE_UNSEALED_HEAD`]). Older clients never read it; a version-8
/// client reads a version-7 server's reply as [`NodeManifestRespV7`].
///
/// Version 9 widened [`PacketId`] to 8 bytes and the packet prefix to 16.
/// The length prefix is unchanged, so an older peer's packets still frame
/// but decode to the wrong id and body.
pub const NODE_PROTOCOL_VERSION: u32 = 9;

/// [`DbInfoResp::features`] bit: the server answers [`SyncMsgs`].
pub const FEATURE_MSG_SYNC: u64 = 1 << 1;

/// [`DbInfoResp::features`] bit: the server answers the extended
/// [`GetTimeSeries`] and says what each reply covers, so a mirror can fetch
/// a range or an instant instead of a whole sealed node.
pub const FEATURE_RANGE_FETCH: u64 = 1 << 2;

/// [`DbInfoResp::features`] bit: [`NodeManifestResp`] says where the
/// server's unsealed head lies, so a mirror can range-fetch history the
/// server has not sealed into a node yet.
pub const FEATURE_UNSEALED_HEAD: u64 = 1 << 3;

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct GetDbInfo;

impl Msg for GetDbInfo {
    const ID: PacketId = short_id([224, 45]);
}

impl Request for GetDbInfo {
    type Reply<B: IoBuf + Clone> = DbInfoResp;
}

/// Decoded by [`VersionedFields`], so a reply from an older server still
/// decodes with the fields it predates defaulted.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct DbInfoResp {
    pub protocol_version: u32,
    /// Capability bits; clients gate post-version-1 requests on these.
    /// Bit 0 was the retired envelope protocol ([224,56]/[224,57]) and
    /// must not be reused. Bit 1 is [`FEATURE_MSG_SYNC`], bit 2
    /// [`FEATURE_RANGE_FETCH`], bit 3 [`FEATURE_UNSEALED_HEAD`].
    pub features: u64,
    /// The msg ids a client should forward up this connection: requests
    /// going to the server, never records coming back. Added in version 3.
    pub command_ids: Vec<PacketId>,
    /// The serving target's telemetry namespace, when it has one: the bare
    /// dotted prefix its components carry. Added in version 3.
    pub namespace: Option<String>,
    /// What a client needs to build and send each command: one entry per
    /// command this server takes, messages and frames alike. Added in
    /// version 4.
    pub commands: Vec<CommandInfo>,
    /// The newest sample time the server held when it answered, or `None`
    /// for an empty db. A mirror learns from it where "now" is in the
    /// peer's data before the first live sample arrives, which manifests
    /// alone can't say: they end at the last seal, possibly long ago.
    /// Added in version 5.
    pub last_updated: Option<Timestamp>,
    /// Which database answered, stable across the server's restarts. A
    /// mirror keys its on-disk cache of this peer by it. `None` from a
    /// server that predates it. Added in version 6.
    pub instance_id: Option<InstanceId>,
    /// The server's database lasts only this run: it is deleted when the
    /// server stops, or lives in a scratch directory the next run won't
    /// reuse, so its id is never seen again. A mirror shouldn't keep a
    /// cache it will never reopen, and shouldn't evict history only the
    /// peer could give back once the peer is gone. Added in version 6.
    pub ephemeral: bool,
}

/// A database's identity: random, made when the database is created, and
/// kept with it for its lifetime.
///
/// It says which cached copy of a peer to reopen, not that the copy is
/// still right: a copied database directory keeps its id, so whoever reuses
/// a cache still checks sealed spans against the peer's.
#[derive(
    Serialize, Deserialize, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
)]
pub struct InstanceId(pub [u8; 16]);

impl std::fmt::Display for InstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Why a string isn't an [`InstanceId`]: anything but 32 hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidInstanceId;

impl std::fmt::Display for InvalidInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("an instance id is 32 hex digits")
    }
}

impl std::error::Error for InvalidInstanceId {}

impl std::str::FromStr for InstanceId {
    type Err = InvalidInstanceId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (pairs, []) = s.as_bytes().as_chunks::<2>() else {
            return Err(InvalidInstanceId);
        };
        if pairs.len() != 16 {
            return Err(InvalidInstanceId);
        }
        let mut id = [0u8; 16];
        let nibble = |c: u8| (c as char).to_digit(16).ok_or(InvalidInstanceId);
        for (byte, [high, low]) in id.iter_mut().zip(pairs) {
            *byte = (nibble(*high)? << 4 | nibble(*low)?) as u8;
        }
        Ok(InstanceId(id))
    }
}

impl<'de> Deserialize<'de> for DbInfoResp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = DbInfoResp;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a DbInfoResp")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                seq: A,
            ) -> Result<DbInfoResp, A::Error> {
                let mut fields = VersionedFields::new(seq)?;
                Ok(DbInfoResp {
                    protocol_version: fields.version,
                    features: fields.since(1)?,
                    command_ids: fields.since(3)?,
                    namespace: fields.since(3)?,
                    commands: fields.since(4)?,
                    last_updated: fields.since(5)?,
                    instance_id: fields.since(6)?,
                    ephemeral: fields.since(6)?,
                })
            }
        }
        const FIELDS: &[&str] = &[
            "protocol_version",
            "features",
            "command_ids",
            "namespace",
            "commands",
            "last_updated",
            "instance_id",
            "ephemeral",
        ];
        deserializer.deserialize_struct("DbInfoResp", FIELDS, Visitor)
    }
}

/// The fields of an identity struct ([`DbInfoResp`], [`LinkInfo`]) in wire
/// order, each read only when the sender's protocol version carries it.
///
/// Postcard cannot default a missing trailing field — an older peer's reply
/// simply ends early, which is an unexpected-end error — so the leading
/// `protocol_version` says how far to read. Whatever a newer peer appends
/// past the known fields stays unread, as postcard always allowed.
struct VersionedFields<A> {
    seq: A,
    version: u32,
    index: usize,
}

impl<'de, A: serde::de::SeqAccess<'de>> VersionedFields<A> {
    fn new(mut seq: A) -> Result<Self, A::Error> {
        let version = seq
            .next_element()?
            .ok_or_else(|| serde::de::Error::invalid_length(0, &"a protocol version"))?;
        Ok(VersionedFields {
            seq,
            version,
            index: 1,
        })
    }

    /// The next field, introduced in protocol version `since`.
    fn since<T: Deserialize<'de> + Default>(&mut self, since: u32) -> Result<T, A::Error> {
        if self.version < since {
            return Ok(T::default());
        }
        let index = self.index;
        self.index += 1;
        self.seq.next_element()?.ok_or_else(|| {
            serde::de::Error::invalid_length(index, &"every field the sender's version carries")
        })
    }
}

/// A command a server takes from its clients, described well enough for a
/// ground tool to list it, build a form for it, and encode what the operator
/// filled in.
///
/// `command_ids` only says which logs to forward; this says what goes in
/// them. A client forwards a message command as `Msg` packets on `id`, and a
/// frame command as its table: the vtable announced once, then a `Table`
/// packet per record.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CommandInfo {
    /// The msg id for a message, or `table_id(vtable)` for a frame.
    pub id: PacketId,
    pub kind: CommandKind,
}

/// The two record kinds a command can be, as metor-fsw-3 defines them.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum CommandKind {
    /// A serde value, encoded as its metadata's codec says (postcard when
    /// `metadata["codec"]` is absent).
    Msg(MsgMetadata),
    /// A fixed `#[repr(C)]` record whose bytes are the table the vtable
    /// describes.
    Frame {
        name: String,
        vtable: VTable<Vec<Op>, Vec<u8>, Vec<Field>>,
    },
}

impl Msg for DbInfoResp {
    const ID: PacketId = short_id([224, 46]);
}

/// Asks a db to stream every message log outside its command set over this
/// connection: the log's latest record first, then live records. Gated on
/// [`FEATURE_MSG_SYNC`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct SyncMsgs;

impl Msg for SyncMsgs {
    const ID: PacketId = short_id([224, 63]);
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
    const ID: PacketId = short_id([224, 47]);
}

impl Request for GetNodeManifest {
    type Reply<B: IoBuf + Clone> = NodeManifestResp;
}

/// Every sealed node the server can currently serve for the component,
/// sorted by start timestamp, and where its unsealed head lies.
///
/// The head is not a node anyone can fetch: it is still being written, and
/// it is only ever read with a range fetch ([`GetTimeSeries`]). `nodes` and
/// `head` come from one snapshot, so together they cover everything the
/// server holds: a node sealed after the snapshot is still part of `head`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NodeManifestResp {
    pub component_id: ComponentId,
    pub nodes: Vec<SealRecord>,
    /// The samples past the sealed nodes, or `None` when there are none.
    /// Added in version 8.
    pub head: Option<UnsealedHead>,
}

impl Msg for NodeManifestResp {
    const ID: PacketId = short_id([224, 48]);
}

/// [`NodeManifestResp`] as a server before version 8 sends it: no head.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NodeManifestRespV7 {
    pub component_id: ComponentId,
    pub nodes: Vec<SealRecord>,
}

impl Msg for NodeManifestRespV7 {
    const ID: PacketId = NodeManifestResp::ID;
}

/// The stretch of a component a server holds but has not sealed: its live
/// head, and any node it rolled past but has not sealed yet.
///
/// A summary, not a seal: there is no checksum, since the bytes are still
/// changing, and `end_ts` is only where the head ended when the server
/// answered.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsealedHead {
    /// The oldest unsealed sample.
    pub start_ts: Timestamp,
    /// The newest sample when the server answered.
    pub end_ts: Timestamp,
    /// Samples in `start_ts..=end_ts`, for cost estimates.
    pub count: u64,
    pub element_size: u64,
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
    const ID: PacketId = short_id([224, 49]);
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
    const ID: PacketId = short_id([224, 50]);
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
pub struct FetchNodeDone {
    pub seal: SealRecord,
}

impl Msg for FetchNodeDone {
    const ID: PacketId = short_id([224, 51]);
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
    const ID: PacketId = short_id([224, 52]);
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
    const ID: PacketId = short_id([224, 53]);
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
    const ID: PacketId = short_id([224, 54]);
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
    const ID: PacketId = short_id([224, 55]);
}

/// Version of the fsw link identity protocol ([`LinkInfo`]), independent
/// of [`NODE_PROTOCOL_VERSION`]. Version 4 widened [`PacketId`] to 8 bytes.
pub const LINK_PROTOCOL_VERSION: u32 = 4;

/// Identity push from an fsw link server: the FIRST packet on every
/// accepted connection, ahead of the schema announce replay. A metor-db
/// server never sends it, and an fsw link server never answers
/// [`GetDbInfo`] — the pair is how one address form serves both peers: a
/// client probes with `GetDbInfo` and lets the first packet's id decide
/// the mode.
///
/// Decoded by [`VersionedFields`], so an older link's identity still
/// decodes with the fields it predates defaulted.
#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub struct LinkInfo {
    pub protocol_version: u32,
    /// Capability bits, reserved (0 for now).
    pub features: u64,
    /// The uplink's configured command set: the msg ids a ground client
    /// should forward up the link. The uplink's minted ports are
    /// untelemetered, so forwarding these can never echo back down.
    pub command_ids: Vec<PacketId>,
    /// The serving target's telemetry namespace, when it has one: the bare
    /// dotted prefix its components carry. Added in protocol version 2.
    pub namespace: Option<String>,
    /// The serving `TcpServer` state's declaration name, so a subscriber can
    /// tell one of a target's links from another. Added in version 2.
    pub link: String,
    /// What a ground tool needs to build and send each of `command_ids`. See
    /// [`DbInfoResp::commands`]. Added in version 3.
    pub commands: Vec<CommandInfo>,
}

impl<'de> Deserialize<'de> for LinkInfo {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = LinkInfo;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a LinkInfo")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> Result<LinkInfo, A::Error> {
                let mut fields = VersionedFields::new(seq)?;
                Ok(LinkInfo {
                    protocol_version: fields.version,
                    features: fields.since(1)?,
                    command_ids: fields.since(1)?,
                    namespace: fields.since(2)?,
                    link: fields.since(2)?,
                    commands: fields.since(3)?,
                })
            }
        }
        const FIELDS: &[&str] = &[
            "protocol_version",
            "features",
            "command_ids",
            "namespace",
            "link",
            "commands",
        ];
        deserializer.deserialize_struct("LinkInfo", FIELDS, Visitor)
    }
}

impl Msg for LinkInfo {
    const ID: PacketId = short_id([224, 61]);
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

/// TXT-record key for the serving link's declaration name, the second half
/// of the machine identity a subscriber matches on (`ns` is the first).
pub const TXT_LINK: &str = "link";

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
    short_id([224, 56]),
    short_id([224, 57]),
    LinkInfo::ID,
    SyncMsgs::ID,
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

        let shelved = AlarmShelved {
            def_id: "BATT_OVERTEMP".into(),
            until: Timestamp(1_700_000_000_000_000),
            reason: Some("sensor swap".into()),
            operator: "sphw".into(),
        };
        let shelved2 = roundtrip(&shelved);
        assert_eq!(shelved.until, shelved2.until);
        assert_eq!(shelved.reason, shelved2.reason);

        let unshelved = AlarmUnshelved {
            def_id: "BATT_OVERTEMP".into(),
            operator: "sphw".into(),
        };
        assert_eq!(unshelved.def_id, roundtrip(&unshelved).def_id);
    }

    #[test]
    fn alarm_packet_ids_are_unique() {
        let ids = [
            AlarmDef::ID,
            AlarmRaised::ID,
            AlarmCleared::ID,
            AlarmAck::ID,
            AlarmShelved::ID,
            AlarmUnshelved::ID,
        ];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b, "alarm packet ids must be unique");
            }
        }
        // Distinct from the previously highest assigned id (UpdateComponent, [224,36] —
        // spelled literally: the type is `nox`-gated, its id is reserved regardless).
        for id in ids {
            assert_ne!(id, short_id([224, 36]));
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
                    system: "dl_slot".into(),
                    components: SequenceChannelComponents {
                        state: "fsw.deploy.channel_status.state".into(),
                        elapsed: "fsw.deploy.channel_status.elapsed".into(),
                        ..Default::default()
                    },
                },
                SequenceChannelSpec {
                    name: "attitude".into(),
                    available: vec!["detumble".into()],
                    ..Default::default()
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
                system: "s".into(),
                components: SequenceChannelComponents {
                    state: "t".into(),
                    ..Default::default()
                },
            }],
        };
        assert_eq!(
            postcard::to_allocvec(&registry).unwrap(),
            // 1 channel; name "adcs" (len 4); 1 available; "detumble" (len 8);
            // system "s"; then the eight component names, `state` "t" and the
            // rest empty.
            [
                1, 4, b'a', b'd', b'c', b's', 1, 8, b'd', b'e', b't', b'u', b'm', b'b', b'l', b'e',
                1, b's', 1, b't', 0, 0, 0, 0, 0, 0, 0
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
    fn channel_state_codes_round_trip() {
        for state in SequenceChannelState::ALL {
            assert_eq!(SequenceChannelState::from_code(state.code()), Some(state));
        }
        assert_eq!(SequenceChannelState::Running.code(), 0);
        assert_eq!(SequenceChannelState::Quiescing.code(), 7);
        assert_eq!(SequenceChannelState::from_code(8), None);
    }

    #[test]
    fn accepts_bits_name_each_command() {
        let accepts = SequenceAccepts(SequenceAccepts::START | SequenceAccepts::RESET);
        assert!(accepts.allows(&SequenceCommandKind::Start));
        assert!(accepts.allows(&SequenceCommandKind::Reset));
        assert!(!accepts.allows(&SequenceCommandKind::Stop));
        assert!(!accepts.allows(&SequenceCommandKind::Load { name: "x".into() }));
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
                ![
                    short_id([224, 41]),
                    short_id([224, 42]),
                    short_id([224, 43])
                ]
                .contains(&id),
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
        assert_eq!(SequenceRegistry::ID, short_id([78, 109]));
        assert_eq!(SequenceChannelEvent::ID, short_id([133, 5]));
        assert_eq!(SequenceCommand::ID, short_id([225, 247]));
        assert_eq!(ReloadSequences::ID, short_id([209, 95]));
    }
}

#[cfg(test)]
mod alarm_id_tests {
    use super::*;

    /// The pinned natural schema-name-hash ids; the pin catches a type
    /// rename, which would silently re-key every persisted recording.
    #[test]
    fn alarm_ids_are_pinned() {
        assert_eq!(AlarmDef::ID, short_id([178, 74]));
        assert_eq!(AlarmRaised::ID, short_id([217, 65]));
        assert_eq!(AlarmCleared::ID, short_id([91, 221]));
        assert_eq!(AlarmAck::ID, short_id([253, 188]));
        assert_eq!(AlarmShelved::ID, short_id([83, 33]));
        assert_eq!(AlarmUnshelved::ID, short_id([200, 230]));
        for id in [
            AlarmDef::ID,
            AlarmRaised::ID,
            AlarmCleared::ID,
            AlarmAck::ID,
            AlarmShelved::ID,
            AlarmUnshelved::ID,
        ] {
            assert!(!NODE_PROTOCOL_MESSAGES.contains(&id));
        }
    }
}

#[cfg(test)]
mod preset_id_tests {
    use super::*;

    /// The pinned natural schema-name-hash ids; the pin catches a type
    /// rename, which would silently re-key every persisted recording.
    #[test]
    fn preset_ids_are_pinned() {
        assert_eq!(PresetDefs::ID, short_id([173, 20]));
        assert!(!NODE_PROTOCOL_MESSAGES.contains(&PresetDefs::ID));
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
            target: "metor_fsw3::coordinator".into(),
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
        assert_eq!(LogEvent::ID, short_id([7, 209]));
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
            AlarmShelved::ID,
            AlarmUnshelved::ID,
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
        assert_eq!(LinkInfo::ID, short_id([224, 61]));
        assert!(NODE_PROTOCOL_MESSAGES.contains(&LinkInfo::ID));
        assert_eq!(SyncMsgs::ID, short_id([224, 63]));
        assert!(NODE_PROTOCOL_MESSAGES.contains(&SyncMsgs::ID));
    }

    #[test]
    fn link_info_round_trips() {
        let info = LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids: vec![SequenceCommand::ID, AlarmAck::ID],
            namespace: Some("sat".into()),
            link: "ground".into(),
            commands: Vec::new(),
        };
        let bytes = postcard::to_allocvec(&info).expect("encode");
        let back: LinkInfo = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back, info);
    }

    /// A v1 reader decodes a v2 identity: postcard stops at the fields it
    /// knows and ignores the trailing bytes.
    #[test]
    fn link_info_v2_decodes_under_v1() {
        #[derive(Deserialize, Debug, PartialEq, Eq)]
        struct LinkInfoV1 {
            protocol_version: u32,
            features: u64,
            command_ids: Vec<PacketId>,
        }
        let info = LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids: vec![SequenceCommand::ID],
            namespace: Some("sat".into()),
            link: "ground".into(),
            commands: Vec::new(),
        };
        let bytes = postcard::to_allocvec(&info).expect("encode");
        let v1: LinkInfoV1 = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(
            v1,
            LinkInfoV1 {
                protocol_version: LINK_PROTOCOL_VERSION,
                features: 0,
                command_ids: vec![SequenceCommand::ID],
            }
        );
    }

    /// A described command crosses intact, frames included.
    #[test]
    fn link_info_round_trips_commands() {
        use metor_proto::vtable::builder::{component, raw_field, schema as field_schema, vtable};
        let info = LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids: vec![AlarmAck::ID, short_id([9, 9])],
            namespace: None,
            link: "ground".into(),
            commands: vec![
                CommandInfo {
                    id: AlarmAck::ID,
                    kind: CommandKind::Msg(MsgMetadata {
                        name: "alarm_ack".into(),
                        schema: OwnedNamedType {
                            name: "AlarmAck".into(),
                            ty: postcard_schema::schema::owned::OwnedDataModelType::U8,
                        },
                        metadata: HashMap::from([("codec".into(), "json".into())]),
                    }),
                },
                CommandInfo {
                    id: short_id([9, 9]),
                    kind: CommandKind::Frame {
                        name: "burn".into(),
                        vtable: vtable([raw_field(
                            0,
                            8,
                            field_schema(
                                metor_proto::types::PrimType::F64,
                                &[1],
                                component("burn.dv"),
                            ),
                        )]),
                    },
                },
            ],
        };
        let bytes = postcard::to_allocvec(&info).expect("encode");
        assert_eq!(
            postcard::from_bytes::<LinkInfo>(&bytes).expect("decode"),
            info
        );
    }

    /// A version-2 link's identity — no `commands` — still decodes, so a new
    /// ground tool connects to an older fsw.
    #[test]
    fn link_info_v2_bytes_decode_as_current() {
        #[derive(Serialize)]
        struct LinkInfoV2 {
            protocol_version: u32,
            features: u64,
            command_ids: Vec<PacketId>,
            namespace: Option<String>,
            link: String,
        }
        let bytes = postcard::to_allocvec(&LinkInfoV2 {
            protocol_version: 2,
            features: 0,
            command_ids: vec![SequenceCommand::ID],
            namespace: Some("sat".into()),
            link: "ground".into(),
        })
        .expect("encode");
        assert_eq!(
            postcard::from_bytes::<LinkInfo>(&bytes).expect("decode v2"),
            LinkInfo {
                protocol_version: 2,
                features: 0,
                command_ids: vec![SequenceCommand::ID],
                namespace: Some("sat".into()),
                link: "ground".into(),
                commands: Vec::new(),
            }
        );
    }

    #[test]
    fn link_info_v1_bytes_decode_as_current() {
        let bytes =
            postcard::to_allocvec(&(1u32, 0u64, vec![SequenceCommand::ID])).expect("encode");
        let info = postcard::from_bytes::<LinkInfo>(&bytes).expect("decode v1");
        assert_eq!(info.command_ids, vec![SequenceCommand::ID]);
        assert_eq!((info.namespace, info.link), (None, String::new()));
    }

    /// A version-3 link that misses a field its version promises is
    /// malformed, not an older link.
    #[test]
    fn link_info_truncated_is_an_error() {
        let bytes =
            postcard::to_allocvec(&(3u32, 0u64, vec![SequenceCommand::ID])).expect("encode");
        assert!(postcard::from_bytes::<LinkInfo>(&bytes).is_err());
    }
}

#[cfg(test)]
mod db_info_tests {
    use super::*;

    /// Every byte of a wide id survives the messages that embed one.
    #[test]
    fn full_width_ids_round_trip_in_messages() {
        const WIDE: PacketId = [1, 2, 3, 4, 5, 6, 7, 8];
        let info = LinkInfo {
            protocol_version: LINK_PROTOCOL_VERSION,
            features: 0,
            command_ids: vec![WIDE],
            namespace: None,
            link: String::new(),
            commands: Vec::new(),
        };
        let bytes = postcard::to_allocvec(&info).expect("encode");
        let decoded: LinkInfo = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(decoded.command_ids, [WIDE]);
    }

    /// A version-7 client reads a version-8 manifest reply up to the head
    /// it doesn't know, and a version-7 reply is read as
    /// [`NodeManifestRespV7`], which a version-8 reply's prefix also is.
    #[test]
    fn node_manifest_head_is_a_trailing_field() {
        let seal = SealRecord {
            start_ts: Timestamp(1),
            end_ts: Timestamp(9),
            count: 3,
            index_len: 24,
            data_len: 24,
            checksum: 5,
            element_size: 8,
        };
        let v8 = NodeManifestResp {
            component_id: ComponentId(4),
            nodes: vec![seal],
            head: Some(UnsealedHead {
                start_ts: Timestamp(10),
                end_ts: Timestamp(20),
                count: 2,
                element_size: 8,
            }),
        };
        let bytes = postcard::to_allocvec(&v8).expect("encode");
        let v7: NodeManifestRespV7 = postcard::from_bytes(&bytes).expect("decode v8 as v7");
        assert_eq!((v7.component_id, v7.nodes), (ComponentId(4), vec![seal]));
        let back: NodeManifestResp = postcard::from_bytes(&bytes).expect("round trip");
        assert_eq!(back.head, v8.head);

        let old = postcard::to_allocvec(&NodeManifestRespV7 {
            component_id: ComponentId(4),
            nodes: vec![seal],
        })
        .expect("encode");
        assert!(postcard::from_bytes::<NodeManifestResp>(&old).is_err());
        assert_eq!(
            postcard::from_bytes::<NodeManifestRespV7>(&old)
                .expect("decode v7")
                .nodes,
            vec![seal]
        );
    }

    /// A v2 reader decodes a current reply: postcard stops at the fields it
    /// knows and ignores the trailing bytes.
    #[test]
    fn db_info_v3_decodes_under_v2() {
        #[derive(Deserialize, Debug, PartialEq, Eq)]
        struct DbInfoRespV2 {
            protocol_version: u32,
            features: u64,
        }
        let v3 = DbInfoResp {
            protocol_version: NODE_PROTOCOL_VERSION,
            features: FEATURE_MSG_SYNC,
            command_ids: vec![AlarmAck::ID],
            namespace: Some("sat".into()),
            commands: Vec::new(),
            last_updated: Some(Timestamp(42)),
            instance_id: Some(InstanceId([7; 16])),
            ephemeral: true,
        };
        let bytes = postcard::to_allocvec(&v3).expect("encode");
        let v2: DbInfoRespV2 = postcard::from_bytes(&bytes).expect("decode v3 as v2");
        assert_eq!(
            v2,
            DbInfoRespV2 {
                protocol_version: NODE_PROTOCOL_VERSION,
                features: FEATURE_MSG_SYNC,
            }
        );
        assert_eq!(
            postcard::from_bytes::<DbInfoResp>(&bytes).expect("round trip"),
            v3
        );
    }

    /// A version-3 server's reply — no `commands` — still decodes, so a new
    /// client connects to an older db.
    #[test]
    fn db_info_v3_bytes_decode_as_current() {
        #[derive(Serialize)]
        struct DbInfoRespV3 {
            protocol_version: u32,
            features: u64,
            command_ids: Vec<PacketId>,
            namespace: Option<String>,
        }
        let bytes = postcard::to_allocvec(&DbInfoRespV3 {
            protocol_version: 3,
            features: FEATURE_MSG_SYNC,
            command_ids: vec![AlarmAck::ID],
            namespace: Some("sat".into()),
        })
        .expect("encode");
        assert_eq!(
            postcard::from_bytes::<DbInfoResp>(&bytes).expect("decode v3"),
            DbInfoResp {
                protocol_version: 3,
                features: FEATURE_MSG_SYNC,
                command_ids: vec![AlarmAck::ID],
                namespace: Some("sat".into()),
                commands: Vec::new(),
                last_updated: None,
                instance_id: None,
                ephemeral: false,
            }
        );
    }

    /// A version-5 server's reply ends at `last_updated`; the identity
    /// fields came in version 6.
    #[test]
    fn db_info_v5_bytes_decode_as_current() {
        #[derive(Serialize)]
        struct DbInfoRespV5 {
            protocol_version: u32,
            features: u64,
            command_ids: Vec<PacketId>,
            namespace: Option<String>,
            commands: Vec<CommandInfo>,
            last_updated: Option<Timestamp>,
        }
        let bytes = postcard::to_allocvec(&DbInfoRespV5 {
            protocol_version: 5,
            features: FEATURE_MSG_SYNC,
            command_ids: Vec::new(),
            namespace: None,
            commands: Vec::new(),
            last_updated: Some(Timestamp(42)),
        })
        .expect("encode");
        let info = postcard::from_bytes::<DbInfoResp>(&bytes).expect("decode v5");
        assert_eq!(info.last_updated, Some(Timestamp(42)));
        assert!(info.instance_id.is_none() && !info.ephemeral);
    }

    #[test]
    fn db_info_v2_bytes_decode_as_current() {
        let bytes = postcard::to_allocvec(&(2u32, FEATURE_MSG_SYNC)).expect("encode");
        let info = postcard::from_bytes::<DbInfoResp>(&bytes).expect("decode v2");
        assert_eq!(info.features, FEATURE_MSG_SYNC);
        assert!(info.command_ids.is_empty() && info.namespace.is_none());
        assert!(info.instance_id.is_none() && !info.ephemeral);
    }

    #[test]
    fn instance_id_round_trips_through_hex() {
        let id = InstanceId(std::array::from_fn(|i| (i * 17) as u8));
        let text = id.to_string();
        assert_eq!(text.len(), 32);
        assert_eq!(text.parse::<InstanceId>(), Ok(id));
        assert_eq!(text.to_uppercase().parse::<InstanceId>(), Ok(id));
        assert!("abc".parse::<InstanceId>().is_err());
        assert!("zz".repeat(16).parse::<InstanceId>().is_err());
        assert!("+f".repeat(16).parse::<InstanceId>().is_err());
        assert!("é".repeat(16).parse::<InstanceId>().is_err());
    }
}
