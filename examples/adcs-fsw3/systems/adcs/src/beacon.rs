//! The beacon: a one-line housekeeping summary in the log at a fixed period.

use adcs_contracts::Housekeeping;
use metor_fsw_3::{Input, JsonSchema, Timestamp, system};
use serde::Deserialize;

use crate::ctrl::magnitude;

/// Configuration for the beacon.
#[derive(Deserialize, JsonSchema, Clone, Debug, PartialEq)]
pub struct BeaconParams {
    /// Seconds between beacons.
    pub period_s: f64,
}

/// The beacon's period and when it last reported.
pub struct Beacon {
    period_s: f64,
    last: Option<Timestamp>,
}

impl Beacon {
    pub fn new(p: BeaconParams) -> Self {
        Self {
            period_s: p.period_s,
            last: None,
        }
    }

    /// Whether a beacon is due at `now`, and if so, marks it sent.
    fn due(&mut self, now: Timestamp) -> bool {
        let due = self
            .last
            .is_none_or(|last| (now.0 - last.0) as f64 * 1e-6 >= self.period_s);
        if due {
            self.last = Some(now);
        }
        due
    }
}

#[system]
/// Logs the newest housekeeping record once per period.
pub fn beacon(#[state] s: &mut Beacon, now: Timestamp, hk: &mut Input<Housekeeping>) {
    let Some(hk) = hk.drain().flatten().last().copied() else {
        return;
    };
    if s.due(now) {
        tracing::info!(
            rate = magnitude(&hk.omega_b),
            bias = magnitude(&hk.b_hat_b),
            torque = magnitude(&hk.torque_b),
            dipole = magnitude(&hk.dipole_b),
            "beacon"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: f64) -> Timestamp {
        Timestamp((s * 1e6) as i64)
    }

    #[test]
    fn first_record_is_due_then_once_per_period() {
        let mut b = Beacon::new(BeaconParams { period_s: 1.0 });
        assert!(b.due(at(0.0)));
        assert!(!b.due(at(0.5)));
        assert!(b.due(at(1.0)));
        assert!(!b.due(at(1.9)));
    }

    #[test]
    fn zero_period_is_due_every_cycle() {
        let mut b = Beacon::new(BeaconParams { period_s: 0.0 });
        assert!(b.due(at(0.0)));
        assert!(b.due(at(0.0)));
    }

    #[test]
    fn clock_stepping_back_is_not_due() {
        let mut b = Beacon::new(BeaconParams { period_s: 1.0 });
        assert!(b.due(at(5.0)));
        assert!(!b.due(at(1.0)));
    }
}
