//! The config a coordinator is built from.
//!
//! It lists the systems to run and wires each system's inputs to other
//! systems' outputs.

use core::marker::PhantomData;
use core::num::NonZeroUsize;
use core::time::Duration;
use std::borrow::Cow;
use std::collections::BTreeMap;

use indexmap::IndexMap;
use metor_proto::types::Timestamp;
use metor_proto_wkt::SEQUENCE_CHANNEL_NAME_CAP;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::io::Backing;
use crate::system::PortDef;

/// The default [`CoordinatorConfig::ring_depth`].
// PANIC Safety: evaluated at compile time.
pub const DEFAULT_RING_DEPTH: NonZeroUsize = NonZeroUsize::new(8).expect("nonzero");

/// The configuration for a coordinator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    /// The clock that paces the cycles.
    pub clock: Clock,
    /// How many records a ring holds per unit of its port's declared depth.
    pub ring_depth: NonZeroUsize,
    /// A prefix added to every name the coordinator announces.
    #[serde(default)]
    pub namespace: Option<String>,
    pub systems: Vec<SystemConfig>,
    /// The resources systems are granted, by id.
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "unique_keys"
    )]
    pub resources: IndexMap<String, Backing>,
    /// The seed every `Rng` parameter draws from.
    #[serde(default)]
    pub seed: u64,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            clock: Clock::Wall {
                rate: WallRate(100.0),
            },
            ring_depth: DEFAULT_RING_DEPTH,
            namespace: None,
            systems: Vec::new(),
            resources: IndexMap::new(),
            seed: 0,
        }
    }
}

/// Configures one system instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemConfig {
    pub id: String,
    pub ty: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default)]
    pub inputs: Vec<InputConfig>,
    #[serde(default)]
    pub outputs: Vec<OutputConfig>,
    /// Gives `Cap<K>` parameters real resources, by parameter name. A sim
    /// resource names its holders itself.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "unique_keys"
    )]
    pub grants: BTreeMap<String, String>,
    /// The adapter the system runs behind, if any. See [`Adapter`].
    #[serde(flatten, with = "adapter_keys")]
    pub adapter: Option<Adapter>,
    /// Where the target file created the system, if it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<Location>,
}

/// A line of a target file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub file: String,
    pub line: u32,
}

impl core::fmt::Display for Location {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}:{}", self.file, self.line)
    }
}

/// Deserializes a map, rejecting a key that appears twice, which serde would
/// otherwise let the last one win.
fn unique_keys<'de, D, M, V>(d: D) -> Result<M, D::Error>
where
    D: Deserializer<'de>,
    M: FromIterator<(String, V)>,
    V: Deserialize<'de>,
{
    struct Unique<V>(PhantomData<V>);

    impl<'de, V: Deserialize<'de>> Visitor<'de> for Unique<V> {
        type Value = IndexMap<String, V>;

        fn expecting(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            f.write_str("a map with unique keys")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = IndexMap::new();
            while let Some((key, value)) = map.next_entry::<String, V>()? {
                if out.contains_key(&key) {
                    return Err(A::Error::custom(format!("`{key}` is named twice")));
                }
                out.insert(key, value);
            }
            Ok(out)
        }
    }

    let unique = d.deserialize_map(Unique(PhantomData))?;
    Ok(unique.into_iter().collect())
}

/// Changes how a system is run.
///
/// [`Adapter::Thread`] runs it on a named background thread. [`Adapter::Channel`]
/// puts it behind a channel, so commands can start, stop, and load it.
///
/// In a config, the adapter is written as a `thread` or a `channel` key next to
/// `id`. A system can have one or the other, not both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Adapter {
    Thread(String),
    Channel(ChannelConfig),
}

/// The adapters a system type runs behind. `bare` is running behind none.
#[cfg(feature = "host")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Adapters {
    pub bare: bool,
    pub thread: bool,
    pub channel: bool,
}

#[cfg(feature = "host")]
impl Default for Adapters {
    fn default() -> Self {
        Self {
            bare: true,
            thread: true,
            channel: true,
        }
    }
}

#[cfg(feature = "host")]
impl Adapters {
    /// Running behind no adapter or a channel, but never on a thread.
    pub const OFF_THREAD: Self = Self {
        bare: true,
        thread: false,
        channel: true,
    };

    /// Whether the type runs behind `adapter`, or behind none for `None`.
    pub fn accepts(self, adapter: Option<&Adapter>) -> bool {
        match adapter {
            None => self.bare,
            Some(Adapter::Thread(_)) => self.thread,
            Some(Adapter::Channel(_)) => self.channel,
        }
    }
}

/// Configures a channel.
///
/// Commands find the channel by `name`. `start` sets whether its system runs at
/// boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub name: ChannelName,
    #[serde(default)]
    pub start: RunState,
}

/// A channel's name, at most [`SEQUENCE_CHANNEL_NAME_CAP`] bytes long.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ChannelName(String);

impl ChannelName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ChannelName {
    type Error = ChannelNameTooLong;

    fn try_from(name: String) -> Result<Self, ChannelNameTooLong> {
        match name.len() <= SEQUENCE_CHANNEL_NAME_CAP {
            true => Ok(Self(name)),
            false => Err(ChannelNameTooLong(name)),
        }
    }
}

impl From<ChannelName> for String {
    fn from(name: ChannelName) -> String {
        name.0
    }
}

impl core::fmt::Display for ChannelName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl JsonSchema for ChannelName {
    fn schema_name() -> Cow<'static, str> {
        String::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        String::json_schema(generator)
    }
}

/// A channel name longer than [`SEQUENCE_CHANNEL_NAME_CAP`] bytes.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("channel `{0}` is longer than {cap} bytes", cap = SEQUENCE_CHANNEL_NAME_CAP)]
pub struct ChannelNameTooLong(pub String);

/// Serializes an [`Adapter`] as either a `thread` key or a `channel` key.
mod adapter_keys {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{Adapter, ChannelConfig};

    #[derive(Default, Deserialize)]
    struct Keys {
        #[serde(default)]
        thread: Option<String>,
        #[serde(default)]
        channel: Option<ChannelConfig>,
    }

    #[derive(Default, Serialize)]
    struct KeysRef<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        thread: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        channel: Option<&'a ChannelConfig>,
    }

    pub fn serialize<S: Serializer>(adapter: &Option<Adapter>, s: S) -> Result<S::Ok, S::Error> {
        let keys = match adapter {
            Some(Adapter::Thread(thread)) => KeysRef {
                thread: Some(thread),
                ..KeysRef::default()
            },
            Some(Adapter::Channel(channel)) => KeysRef {
                channel: Some(channel),
                ..KeysRef::default()
            },
            None => KeysRef::default(),
        };
        keys.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Adapter>, D::Error> {
        match Keys::deserialize(d)? {
            Keys {
                thread: Some(_),
                channel: Some(_),
            } => Err(D::Error::custom(
                "a system takes a `thread` or a `channel`, not both",
            )),
            Keys {
                thread: Some(thread),
                ..
            } => Ok(Some(Adapter::Thread(thread))),
            Keys {
                channel: Some(channel),
                ..
            } => Ok(Some(Adapter::Channel(channel))),
            Keys { .. } => Ok(None),
        }
    }
}

/// Whether a channel's system is executed each cycle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    #[default]
    Running,
    Stopped,
}

impl SystemConfig {
    /// Returns a system with no params and no inputs.
    pub fn new(id: impl Into<String>, ty: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ty: ty.into(),
            params: serde_json::Value::Null,
            inputs: Vec::new(),
            outputs: Vec::new(),
            grants: BTreeMap::new(),
            adapter: None,
            location: None,
        }
    }
}

/// An output port declared by the config instead of the system type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputConfig {
    pub port: String,
    #[serde(flatten)]
    pub source: OutputSource,
}

impl OutputConfig {
    /// Creates an output port that carries the record called `record`.
    pub fn with_record(port: impl Into<String>, record: impl Into<String>) -> Self {
        Self {
            port: port.into(),
            source: OutputSource::Record(record.into()),
        }
    }
}

/// Where a config-defined output gets its definition from.
///
/// `Record` refers to a record registered in the system table. `Def` is a
/// definition given in full.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSource {
    Record(String),
    Def(PortDef),
}

/// An input port and the outputs that feed it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputConfig {
    pub port: String,
    pub from: Vec<PortRef>,
}

/// A reference to an output port of another system.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortRef {
    pub system: String,
    pub port: String,
}

/// The clock that paces the coordinator's cycles.
///
/// `Wall` runs `rate` cycles per second of real time. `Simulated` advances time
/// by `dt` each cycle, as fast as the systems run, starting at `epoch`, or at
/// the time of the build without one.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Clock {
    Wall {
        rate: WallRate,
    },
    Simulated {
        dt: Duration,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epoch: Option<Timestamp>,
    },
}

/// A wall clock's rate in cycles per second: finite and at least
/// [`WallRate::MIN`].
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct WallRate(f64);

impl WallRate {
    pub const MIN: f64 = 0.001;

    pub fn get(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for WallRate {
    type Error = InvalidWallRate;

    fn try_from(rate: f64) -> Result<Self, InvalidWallRate> {
        match rate.is_finite() && rate >= Self::MIN {
            true => Ok(Self(rate)),
            false => Err(InvalidWallRate(rate)),
        }
    }
}

impl From<WallRate> for f64 {
    fn from(rate: WallRate) -> f64 {
        rate.0
    }
}

/// A wall clock rate that is not finite or is below [`WallRate::MIN`].
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("wall clock rate {0} must be finite and at least 0.001 Hz")]
pub struct InvalidWallRate(pub f64);

/// Choices one build makes that the target's config does not.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuildOptions {
    /// Turn every thread group on the coordinator thread, in step order.
    pub deterministic: bool,
}

impl InputConfig {
    /// Creates an input port fed by the output `output` of system `system`.
    pub fn new(
        port: impl Into<String>,
        system: impl Into<String>,
        output: impl Into<String>,
    ) -> Self {
        Self {
            port: port.into(),
            from: vec![PortRef::new(system, output)],
        }
    }
}

impl PortRef {
    pub fn new(system: impl Into<String>, port: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            port: port.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_params_defaults_and_round_trip() {
        let bare: SystemConfig =
            serde_json::from_str(r#"{"id":"nav","ty":"nav"}"#).expect("params optional");
        assert_eq!(bare, SystemConfig::new("nav", "nav"));
        let full = SystemConfig {
            params: serde_json::json!({ "gain": 2.5 }),
            ..SystemConfig::new("nav", "nav")
        };
        let text = serde_json::to_string(&full).expect("serializable");
        assert_eq!(
            serde_json::from_str::<SystemConfig>(&text).expect("round trips"),
            full
        );
    }

    #[test]
    fn test_location_is_optional() {
        let text = r#"{"id":"nav","ty":"nav","location":{"file":"target.py","line":12}}"#;
        let located: SystemConfig = serde_json::from_str(text).expect("a location parses");
        let location = located.location.as_ref().expect("a location");
        assert_eq!(location.to_string(), "target.py:12");
        let encoded = serde_json::to_string(&located).expect("encodes");
        assert_eq!(
            serde_json::from_str::<SystemConfig>(&encoded).expect("round trips"),
            located
        );
        let bare = serde_json::to_string(&SystemConfig::new("nav", "nav")).expect("encodes");
        assert!(!bare.contains("location"), "{bare}");
        let missing_line = r#"{"id":"nav","ty":"nav","location":{"file":"target.py"}}"#;
        assert!(serde_json::from_str::<SystemConfig>(missing_line).is_err());
    }

    #[cfg(feature = "host")]
    #[test]
    fn test_grants_default_and_round_trip() {
        let bare = SystemConfig::new("gps", "gps");
        let text = serde_json::to_string(&bare).expect("serializable");
        assert!(!text.contains("grants"));
        let granted = SystemConfig {
            grants: BTreeMap::from([("uart".into(), "gps1".into())]),
            ..bare
        };
        let text = serde_json::to_string(&granted).expect("serializable");
        assert_eq!(
            serde_json::from_str::<SystemConfig>(&text).expect("round trips"),
            granted
        );
    }

    #[test]
    fn test_grant_needs_a_resource() {
        let text = r#"{"id":"gps","ty":"gps","grants":{"uart":5}}"#;
        assert!(serde_json::from_str::<SystemConfig>(text).is_err());
    }

    #[test]
    fn test_reject_a_parameter_granted_twice() {
        let text = r#"{"id":"gps","ty":"gps","grants":{"uart":"a","uart":"b"}}"#;
        let error = serde_json::from_str::<SystemConfig>(text).expect_err("a duplicate key");
        assert!(
            error.to_string().contains("`uart` is named twice"),
            "{error}"
        );
    }

    #[test]
    fn test_resources_keep_order_and_reject_a_repeated_id() {
        #[derive(Debug, Deserialize)]
        struct Resources(#[serde(deserialize_with = "unique_keys")] IndexMap<String, Backing>);

        let sim = r#"{"sim":{"kind":"uart","host":{"system":"a","param":"u"},"device":{"system":"b","param":"u"}}}"#;
        let parse = |text: String| serde_json::from_str::<Resources>(&text);
        let two = parse(format!(r#"{{"z":{sim},"a":{sim}}}"#)).expect("unique ids");
        let ids: Vec<_> = two.0.keys().map(String::as_str).collect();
        assert_eq!(ids, ["z", "a"]);
        let error = parse(format!(r#"{{"z":{sim},"z":{sim}}}"#)).expect_err("a repeated id");
        assert!(error.to_string().contains("`z` is named twice"), "{error}");
        let config = CoordinatorConfig {
            resources: two.0,
            ..CoordinatorConfig::default()
        };
        let text = serde_json::to_string(&config).expect("serializable");
        assert_eq!(
            serde_json::from_str::<CoordinatorConfig>(&text).unwrap(),
            config
        );
    }

    #[test]
    fn test_adapters_accept() {
        let thread = Adapter::Thread("gps".into());
        let all = Adapters::default();
        assert!(all.accepts(None) && all.accepts(Some(&thread)));
        let bare = Adapters {
            thread: false,
            ..all
        };
        assert!(bare.accepts(None));
        assert!(!bare.accepts(Some(&thread)));
        let none = Adapters {
            bare: false,
            ..bare
        };
        assert!(!none.accepts(None));
    }

    fn adapter_of(text: &str) -> Result<Option<Adapter>, serde_json::Error> {
        serde_json::from_str::<SystemConfig>(text).map(|system| system.adapter)
    }

    #[test]
    fn test_adapter_forms_round_trip() {
        let thread = SystemConfig {
            adapter: Some(Adapter::Thread("gps".into())),
            ..SystemConfig::new("a", "t")
        };
        let channel = SystemConfig {
            adapter: Some(Adapter::Channel(ChannelConfig {
                name: crate::ChannelName::try_from("mode".to_string()).expect("a short name"),
                start: RunState::Stopped,
            })),
            ..SystemConfig::new("a", "t")
        };
        for system in [SystemConfig::new("a", "t"), thread, channel] {
            let text = serde_json::to_string(&system).expect("serializable");
            assert_eq!(
                serde_json::from_str::<SystemConfig>(&text).expect("round trips"),
                system
            );
        }
        let text = serde_json::to_string(&SystemConfig::new("a", "t")).expect("serializable");
        assert!(
            !text.contains("thread") && !text.contains("channel"),
            "{text}"
        );
    }

    #[test]
    fn test_channel_name_cap() {
        let cap = "c".repeat(SEQUENCE_CHANNEL_NAME_CAP);
        let name = ChannelName::try_from(cap.clone()).expect("a name at the cap");
        assert_eq!(name.as_str(), cap);
        assert_eq!(
            ChannelName::try_from(String::new()).map(String::from),
            Ok(String::new())
        );
        let long = "c".repeat(SEQUENCE_CHANNEL_NAME_CAP + 1);
        assert_eq!(
            ChannelName::try_from(long.clone()),
            Err(ChannelNameTooLong(long.clone()))
        );
        let text = format!(r#"{{"id":"a","ty":"t","channel":{{"name":"{long}"}}}}"#);
        assert!(serde_json::from_str::<SystemConfig>(&text).is_err());
    }

    #[test]
    fn test_adapter_keys_parse() {
        assert_eq!(adapter_of(r#"{"id":"a","ty":"t"}"#).unwrap(), None);
        assert_eq!(
            adapter_of(r#"{"id":"a","ty":"t","thread":"gps"}"#).unwrap(),
            Some(Adapter::Thread("gps".into()))
        );
        assert_eq!(
            adapter_of(r#"{"id":"a","ty":"t","channel":{"name":"m"}}"#).unwrap(),
            Some(Adapter::Channel(ChannelConfig {
                name: crate::ChannelName::try_from("m".to_string()).expect("a short name"),
                start: RunState::Running,
            }))
        );
    }

    #[test]
    fn test_reject_bad_adapter() {
        for text in [
            r#"{"id":"a","ty":"t","thread":"gps","channel":{"name":"m"}}"#,
            r#"{"id":"a","ty":"t","thread":5}"#,
            r#"{"id":"a","ty":"t","channel":{"nam":"m"}}"#,
            r#"{"id":"a","ty":"t","channel":{"name":"m","start":"paused"}}"#,
        ] {
            assert!(adapter_of(text).is_err(), "{text}");
        }
    }

    #[test]
    fn test_output_source_forms() {
        let record: OutputConfig =
            serde_json::from_str(r#"{"port":"ping","record":"ping"}"#).expect("record form");
        assert_eq!(record, OutputConfig::with_record("ping", "ping"));
        assert!(serde_json::from_str::<OutputConfig>(r#"{"port":"x"}"#).is_err());
    }
}
