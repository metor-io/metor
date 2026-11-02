//! The `Alarms` parameter of a `#[system]` fn.

use core::fmt;

use metor_proto::types::Timestamp;

use super::{AlarmCell, AlarmPorts, AlarmSpec, Edge, Msg};

/// How many active alarms `Alarms` clears when dropped.
pub const MAX_TRACKED: usize = 32;

/// A system's alarm outputs, the cycle time its checks stamp, and up to
/// [`MAX_TRACKED`] active alarms, which it clears when dropped.
pub struct Alarms {
    ports: AlarmPorts,
    now: Timestamp,
    active: heapless::Vec<ActiveAlarm, MAX_TRACKED>,
}

/// An alarm this system has raised and not cleared.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ActiveAlarm {
    spec: AlarmSpec,
    raised_at: Timestamp,
}

impl Alarms {
    pub(crate) fn new(ports: AlarmPorts) -> Self {
        Self {
            ports,
            now: Timestamp(0),
            active: heapless::Vec::new(),
        }
    }

    pub(crate) fn set_now(&mut self, now: Timestamp) {
        self.now = now;
    }

    /// Steps `cell` with `breached` and publishes any edge, with the spec's
    /// id as the message. `None` holds the cell's counts.
    pub fn check(
        &mut self,
        cell: &mut AlarmCell,
        spec: &AlarmSpec,
        breached: impl Into<Option<bool>>,
    ) {
        self.check_with(cell, spec, breached, |m| fmt::Write::write_str(m, spec.id));
    }

    /// As [`check`](Self::check), with `fmt` writing the message. `fmt` runs
    /// only on a raise.
    pub fn check_with(
        &mut self,
        cell: &mut AlarmCell,
        spec: &AlarmSpec,
        breached: impl Into<Option<bool>>,
        fmt: impl FnOnce(&mut Msg) -> fmt::Result,
    ) {
        let Some(edge) = cell.step(spec, breached.into(), self.now) else {
            return;
        };
        self.ports.publish_alarm(spec, edge, fmt);
        self.track(*spec, edge);
    }

    /// Adds a new raise to `active`, or removes a cleared one. A raise past
    /// `MAX_TRACKED` is not tracked.
    fn track(&mut self, spec: AlarmSpec, edge: Edge) {
        match edge {
            Edge::Raised { raised_at } => {
                let _ = self.active.push(ActiveAlarm { spec, raised_at });
            }
            Edge::Cleared { raised_at } => self
                .active
                .retain(|a| *a != ActiveAlarm { spec, raised_at }),
        }
    }
}

impl Drop for Alarms {
    fn drop(&mut self) {
        for alarm in core::mem::take(&mut self.active) {
            let edge = Edge::Cleared {
                raised_at: alarm.raised_at,
            };
            self.ports.publish_alarm(&alarm.spec, edge, |_| Ok(()));
        }
    }
}
