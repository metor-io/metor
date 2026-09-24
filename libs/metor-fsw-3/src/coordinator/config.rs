//! The graph description a coordinator is built from.

use core::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::error::BuildError;
use crate::system::PortDef;

const MIN_WALL_RATE: f64 = 0.001;

/// Default number of records a ring can hold
pub const DEFAULT_RING_DEPTH: usize = 8;

/// The configuration for a coordinator
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    pub clock: Clock,
    pub ring_depth: usize,
    /// The prefix every announced name hangs under.
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

/// A `SystemConfig` names one system, its registered type, its params, and the edges into it.
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
    /// The adapter the system runs behind, if any.
    #[serde(flatten, with = "adapter_keys")]
    pub adapter: Option<Adapter>,
}

/// What a system runs behind: a named background thread, or a channel.
///
/// In a config it is a `thread` or a `channel` key beside `id`, never both.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Adapter {
    Thread(String),
    Channel(ChannelConfig),
}

/// A channel's name, which commands address, and its state at boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(default)]
    pub start: RunState,
}

/// An adapter as a config spells it: at most one of two keys.
mod adapter_keys {
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{Adapter, ChannelConfig};

    #[derive(Default, Serialize, Deserialize)]
    struct Keys {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<ChannelConfig>,
    }

    pub fn serialize<S: Serializer>(adapter: &Option<Adapter>, s: S) -> Result<S::Ok, S::Error> {
        let keys = match adapter.clone() {
            Some(Adapter::Thread(thread)) => Keys {
                thread: Some(thread),
                ..Keys::default()
            },
            Some(Adapter::Channel(channel)) => Keys {
                channel: Some(channel),
                ..Keys::default()
            },
            None => Keys::default(),
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
            adapter: None,
        }
    }
}

/// One port of a system with dynamic outputs, and where its definition comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputConfig {
    pub port: String,
    #[serde(flatten)]
    pub source: OutputSource,
}

impl OutputConfig {
    /// A port carrying the record `record` names.
    pub fn record(port: impl Into<String>, record: impl Into<String>) -> Self {
        Self {
            port: port.into(),
            source: OutputSource::Record(record.into()),
        }
    }
}

/// What defines a dynamic output: a record name looked up in the table, a
/// peer's port `metor build` has yet to resolve, or the definition it resolved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSource {
    Record(String),
    Peer(PeerPort),
    Def(PortDef),
}

/// A port of a link on some target of the deployment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PeerPort {
    pub target: String,
    pub link: String,
    pub port: String,
}

/// A link on some target of the deployment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PeerLink {
    pub target: String,
    pub link: String,
}

/// The producers of one input port
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputConfig {
    pub port: String,
    pub from: Vec<PortRef>,
}

/// An reference to an output port of another system
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortRef {
    pub system: String,
    pub port: String,
}

/// The clock mode for the coordinator either real wall time or a simulated clock.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Clock {
    Wall { rate: f64 },
    Simulated { dt: Duration },
}

impl Clock {
    pub(crate) fn validate(&self) -> Result<(), BuildError> {
        if let Self::Wall { rate } = self
            && (!rate.is_finite() || *rate < MIN_WALL_RATE)
        {
            return Err(BuildError::InvalidClockRate);
        }
        Ok(())
    }

    /// The wall budget for one cycle; zero under a simulated clock.
    pub(crate) fn cycle_budget(&self) -> Duration {
        match self {
            Clock::Wall { rate } if *rate > 0.0 => {
                Duration::try_from_secs_f64(1.0 / rate).unwrap_or(Duration::MAX)
            }
            _ => Duration::ZERO,
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

    #[test]
    fn test_zero_cycle_budget() {
        assert_eq!(
            Clock::Simulated {
                dt: Duration::from_millis(5)
            }
            .cycle_budget(),
            Duration::ZERO
        );
        let clock = Clock::Wall { rate: f64::MAX };
        assert_eq!(clock.validate(), Ok(()));
        assert_eq!(clock.cycle_budget(), Duration::ZERO);
    }

    #[test]
    fn test_minimum_wall_rate_budget() {
        let clock = Clock::Wall {
            rate: MIN_WALL_RATE,
        };
        assert_eq!(clock.validate(), Ok(()));
        assert_eq!(clock.cycle_budget(), Duration::from_secs(1_000));
    }
}
