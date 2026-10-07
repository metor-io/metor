//! The config a coordinator is built from.
//!
//! It lists the systems to run and wires each system's inputs to other
//! systems' outputs.

use core::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::io::Grant;
use crate::system::PortDef;

/// The default [`CoordinatorConfig::ring_depth`].
pub const DEFAULT_RING_DEPTH: usize = 8;

/// The configuration for a coordinator.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    pub clock: Clock,
    /// Scales each port's ring size.
    ///
    /// A port's ring holds its declared depth times this many records.
    pub ring_depth: usize,
    /// A prefix added to every name the coordinator announces.
    #[serde(default)]
    pub namespace: Option<String>,
    pub systems: Vec<SystemConfig>,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            clock: Clock::Wall { rate: 100.0 },
            ring_depth: DEFAULT_RING_DEPTH,
            namespace: None,
            systems: Vec::new(),
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
    /// Gives each `Cap<K>` parameter a resource.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Grant>,
    /// The adapter the system runs behind, if any. See [`Adapter`].
    #[serde(flatten, with = "adapter_keys")]
    pub adapter: Option<Adapter>,
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

/// Configures a channel.
///
/// Commands find the channel by `name`. `start` sets whether its system runs at
/// boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(default)]
    pub start: RunState,
}

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
            grants: Vec::new(),
            adapter: None,
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
    pub fn record(port: impl Into<String>, record: impl Into<String>) -> Self {
        Self {
            port: port.into(),
            source: OutputSource::Record(record.into()),
        }
    }
}

/// Where a config-defined output gets its definition from.
///
/// `Record` refers to a record registered in the system table. `Peer` points at a
/// port on another target, which `metor build` replaces with `Def`, the
/// resolved definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSource {
    Record(String),
    Peer(PeerPort),
    Def(PortDef),
}

/// A port on a link of another target in the same deployment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PeerPort {
    pub target: String,
    pub link: String,
    pub port: String,
}

/// A link on another target in the same deployment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PeerLink {
    pub target: String,
    pub link: String,
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
/// by `dt` each cycle, as fast as the systems run.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Clock {
    Wall { rate: f64 },
    Simulated { dt: Duration },
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
    fn test_grants_default_and_round_trip() {
        let bare = SystemConfig::new("gps", "gps");
        let text = serde_json::to_string(&bare).expect("serializable");
        assert!(!text.contains("grants"));
        let granted = SystemConfig {
            grants: vec![Grant {
                param: "uart".into(),
                resource: "gps1".into(),
            }],
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
        let text = r#"{"id":"gps","ty":"gps","grants":[{"param":"uart"}]}"#;
        assert!(serde_json::from_str::<SystemConfig>(text).is_err());
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
                name: "mode".into(),
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
    fn test_adapter_keys_parse() {
        assert_eq!(adapter_of(r#"{"id":"a","ty":"t"}"#).unwrap(), None);
        assert_eq!(
            adapter_of(r#"{"id":"a","ty":"t","thread":"gps"}"#).unwrap(),
            Some(Adapter::Thread("gps".into()))
        );
        assert_eq!(
            adapter_of(r#"{"id":"a","ty":"t","channel":{"name":"m"}}"#).unwrap(),
            Some(Adapter::Channel(ChannelConfig {
                name: "m".into(),
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
        assert_eq!(record, OutputConfig::record("ping", "ping"));
        let peer: OutputConfig = serde_json::from_str(
            r#"{"port":"est.est","peer":{"target":"a","link":"pub","port":"est.est"}}"#,
        )
        .expect("peer form");
        assert!(matches!(peer.source, OutputSource::Peer(ref p) if p.link == "pub"));
        let text = serde_json::to_string(&peer).expect("encodes");
        assert!(text.starts_with(r#"{"port":"est.est","peer":{"#), "{text}");
        assert!(serde_json::from_str::<OutputConfig>(r#"{"port":"x"}"#).is_err());
    }
}
