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
    /// If set chooses a particular background thread for the system to run on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
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
            thread: None,
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
