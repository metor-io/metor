//! The graph description a coordinator is built from.

use core::time::Duration;

use serde::{Deserialize, Serialize};

/// A whole graph: the loop's clock, the ring sizing, and the systems in step
/// order.
///
/// List order is step order. A producer may appear after its consumer, which
/// makes the consumer read the previous cycle's record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoordinatorConfig {
    pub clock: Clock,
    /// Records each ring holds.
    pub depth: usize,
    /// Reader slots each ring keeps beyond its wired edges.
    pub reader_slack: usize,
    pub systems: Vec<SystemConfig>,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            clock: Clock::Wall { rate: 100.0 },
            depth: 8,
            reader_slack: 4,
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

/// The producers of one input port. An input left out of the list is
/// unconnected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputConfig {
    pub port: String,
    pub from: Vec<PortRef>,
}

/// An output port of another system, by id and port name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortRef {
    pub system: String,
    pub port: String,
}

/// Which clock stamps each cycle, and how the loop paces itself.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Clock {
    /// Wall time, with the loop sleeping out the remainder of each `1 / rate`
    /// cycle budget.
    Wall { rate: f64 },
    /// A logical clock advancing `dt` per cycle, with the loop running as fast
    /// as the host allows.
    Simulated { dt: Duration },
}

impl Clock {
    /// The wall budget for one cycle; zero under a simulated clock.
    pub(crate) fn budget(&self) -> Duration {
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
    fn default_is_wall_at_100_hz() {
        let config = CoordinatorConfig::default();
        assert_eq!(config.clock, Clock::Wall { rate: 100.0 });
        assert_eq!(config.depth, 8);
        assert_eq!(config.reader_slack, 4);
        assert_eq!(config.clock.budget(), Duration::from_millis(10));
    }

    #[test]
    fn simulated_and_nonpositive_rates_have_no_budget() {
        assert_eq!(
            Clock::Simulated {
                dt: Duration::from_millis(5)
            }
            .budget(),
            Duration::ZERO
        );
        assert_eq!(Clock::Wall { rate: 0.0 }.budget(), Duration::ZERO);
        assert_eq!(Clock::Wall { rate: -1.0 }.budget(), Duration::ZERO);
        assert_eq!(Clock::Wall { rate: f64::NAN }.budget(), Duration::ZERO);
    }

    #[test]
    fn a_vanishing_rate_saturates_the_budget() {
        assert_eq!(Clock::Wall { rate: 1e-300 }.budget(), Duration::MAX);
    }
}
