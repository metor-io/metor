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

/// One system: the id it is addressed by, the registered type it is built
/// from, and the edges into its inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SystemConfig {
    pub id: String,
    pub ty: String,
    #[serde(default)]
    pub inputs: Vec<InputConfig>,
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
