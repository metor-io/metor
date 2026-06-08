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

#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct UpdateComponent {
    pub id: ComponentId,
    pub value: crate::ComponentValue,
}

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
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

/// Which side of a value a display limit sits on. A band is expressed as an `Upper`
/// plus a `Lower` entry.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    Upper,
    Lower,
}

/// A single limit line to *display* on a plot. Informational only — limits describe
/// where to draw guide lines, they are **not** the boundary that decides firing. The
/// control system evaluates firing (hysteresis, debounce, rate, …) and reports it via
/// [`AlarmRaised`].
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AlarmLimit {
    pub kind: LimitKind,
    pub value: f64,
    pub severity: Severity,
    pub label: Option<String>,
}

/// The component (and optional element index) an alarm pertains to, enabling plots to
/// auto-associate limit lines and tinting with the traces that show that data.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AlarmTarget {
    pub component_id: ComponentId,
    pub element_index: Option<usize>,
}

/// Declaration / description of an alarm, broadcast by the control system. Carries the
/// human-readable identity and the informational display limits. The event time is the
/// message-log timestamp.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AlarmDef {
    pub id: AlarmId,
    pub name: String,
    pub description: String,
    pub target: Option<AlarmTarget>,
    pub limits: Vec<AlarmLimit>,
    pub default_severity: Severity,
}

impl Msg for AlarmDef {
    const ID: PacketId = [224, 37];
}

/// The source of truth that an alarm is *firing*, broadcast by the control system. The
/// event time is the message-log timestamp.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AlarmRaised {
    pub def_id: AlarmId,
    pub occurrence: OccurrenceId,
    pub severity: Severity,
    pub value: Option<f64>,
    pub message: String,
}

impl Msg for AlarmRaised {
    const ID: PacketId = [224, 38];
}

/// Marks a previously raised [`occurrence`](AlarmRaised::occurrence) as resolved,
/// broadcast by the control system.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AlarmCleared {
    pub def_id: AlarmId,
    pub occurrence: OccurrenceId,
}

impl Msg for AlarmCleared {
    const ID: PacketId = [224, 39];
}

/// Operator acknowledgment of an alarm occurrence, published by the panel so every
/// connected client sees the ack.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AlarmAck {
    pub def_id: AlarmId,
    pub occurrence: OccurrenceId,
    pub operator: String,
    pub note: Option<String>,
}

impl Msg for AlarmAck {
    const ID: PacketId = [224, 40];
}

/// Stable identity for a sequence channel, assigned by the control system. A channel is
/// a slot that holds at most one loaded sequence at a time.
pub type ChannelId = u64;

/// Human-readable name of a sequence, e.g. `"deploy_solar_array"`. Sequences are
/// referenced by name when loaded into a channel.
pub type SequenceName = String;

/// One channel slot declared by the control system: its identity, display name, and the
/// set of sequence names that may be loaded into it.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SequenceChannelSpec {
    pub id: ChannelId,
    pub name: String,
    pub available: Vec<SequenceName>,
}

/// Whole-registry declaration of the sequence channels, broadcast by the control system.
/// Re-publishing replaces the registry — the latest wins. This is the configuration the
/// sequence UI sources from the control system.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SequenceRegistry {
    pub channels: Vec<SequenceChannelSpec>,
}

impl Msg for SequenceRegistry {
    const ID: PacketId = [224, 41];
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
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum SequenceEventKind {
    /// A sequence was loaded into the channel.
    Loaded { name: SequenceName },
    /// The channel's loaded sequence was cleared.
    Unloaded,
    Started,
    /// The sequence made progress — sent a message, advanced a step, etc. The `detail`
    /// becomes the channel's latest status line.
    Progress { detail: String },
    /// Hard-stopped (dropped). May have left the system in an unsafe state.
    Stopped,
    /// Commanded safe-termination ran to completion.
    Aborted,
    Completed,
    Failed { reason: String },
}

/// A granular per-channel state update, broadcast by the control system (control → panel).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SequenceChannelEvent {
    pub channel_id: ChannelId,
    pub kind: SequenceEventKind,
}

impl Msg for SequenceChannelEvent {
    const ID: PacketId = [224, 42];
}

/// An operator command on a channel, published by the panel (panel → control). The
/// control system executes it and reports the result via [`SequenceChannelEvent`].
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum SequenceCommandKind {
    /// Load the named sequence into the channel.
    Load { name: SequenceName },
    Start,
    /// Commanded safe-termination.
    Abort,
    /// Hard-stop (drop) — may leave the system unsafe.
    Stop,
}

/// A command targeting one channel, published by the panel.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SequenceCommand {
    pub channel_id: ChannelId,
    pub command: SequenceCommandKind,
}

impl Msg for SequenceCommand {
    const ID: PacketId = [224, 43];
}

/// Asks the control system to re-read its sequence source(s) (disk, etc.) and re-publish
/// an updated [`SequenceRegistry`]. Global scope. Published by the panel.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ReloadSequences {}

impl Msg for ReloadSequences {
    const ID: PacketId = [224, 44];
}

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
        // Distinct from the previously highest assigned id (UpdateComponent).
        for id in ids {
            assert_ne!(id, UpdateComponent::ID);
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
                    id: 1,
                    name: "Deploy".into(),
                    available: vec!["deploy_solar_array".into(), "deploy_antenna".into()],
                },
                SequenceChannelSpec {
                    id: 2,
                    name: "Attitude".into(),
                    available: vec!["detumble".into()],
                },
            ],
        };
        let registry2 = roundtrip(&registry);
        assert_eq!(registry.channels.len(), registry2.channels.len());
        assert_eq!(
            registry.channels[0].available,
            registry2.channels[0].available
        );

        let event = SequenceChannelEvent {
            channel_id: 1,
            kind: SequenceEventKind::Loaded {
                name: "deploy_solar_array".into(),
            },
        };
        assert_eq!(event.channel_id, roundtrip(&event).channel_id);

        let progress = SequenceChannelEvent {
            channel_id: 1,
            kind: SequenceEventKind::Progress {
                detail: "panel 1 latched".into(),
            },
        };
        assert!(matches!(
            roundtrip(&progress).kind,
            SequenceEventKind::Progress { .. }
        ));

        let command = SequenceCommand {
            channel_id: 2,
            command: SequenceCommandKind::Load {
                name: "detumble".into(),
            },
        };
        assert_eq!(command.channel_id, roundtrip(&command).channel_id);

        let _ = roundtrip(&ReloadSequences {});
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
    }
}
