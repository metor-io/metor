//! The graph description a coordinator is built from.

use core::time::Duration;

use serde::{Deserialize, Serialize};

use super::error::BuildError;

const MIN_WALL_RATE: f64 = 0.001;

/// The configuration for a coordinator
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    pub clock: Clock,
    /// The maximum number of frames a ring can hold.
    pub ring_depth: usize,
    pub systems: Vec<SystemConfig>,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            clock: Clock::Wall { rate: 100.0 },
            ring_depth: 8,
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
    /// Ports a system with dynamic outputs publishes, named by their record.
    #[serde(default)]
    pub outputs: Vec<OutputConfig>,
    /// The background thread an async system runs on; `None` is the shared one.
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

/// One port of a system with dynamic outputs, and the record it carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputConfig {
    pub port: String,
    pub record: String,
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
    fn params_default_to_null_and_round_trip() {
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
    fn simulated_and_subnanosecond_periods_have_no_budget() {
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
    fn minimum_wall_rate_has_a_bounded_period() {
        let clock = Clock::Wall {
            rate: MIN_WALL_RATE,
        };
        assert_eq!(clock.validate(), Ok(()));
        assert_eq!(clock.cycle_budget(), Duration::from_secs(1_000));
    }
}
