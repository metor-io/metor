//! Alarms: conditions over a system's or sequence's ports, published as the
//! wkt `AlarmRaised` and `AlarmCleared` messages.

mod cell;
mod ports;
mod records;
mod system;
mod text;

use core::num::NonZeroU16;

pub use cell::{AlarmCell, Edge};
pub use metor_proto_wkt::Severity;
pub use ports::{ALARM_CLEARED_PORT, ALARM_RAISED_PORT, AlarmPorts};
pub use records::{ClearedMsg, RaisedMsg};
pub use system::Alarms;
#[cfg(test)]
pub(crate) use text::occurrence;
pub use text::{Id, Msg};

/// One alarm's identity and debounce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlarmSpec {
    /// Unique among the alarms of one system or sequence.
    pub id: &'static str,
    pub severity: Severity,
    pub debounce: NonZeroU16,
}

impl AlarmSpec {
    pub const fn info(id: &'static str) -> Self {
        Self::new(id, Severity::Info)
    }

    pub const fn warning(id: &'static str) -> Self {
        Self::new(id, Severity::Warning)
    }

    pub const fn critical(id: &'static str) -> Self {
        Self::new(id, Severity::Critical)
    }

    /// Cycles a breach must hold to raise, and a recovery to clear.
    pub const fn debounce(self, cycles: u16) -> Self {
        let Some(debounce) = NonZeroU16::new(cycles) else {
            // PANIC Safety: specs are `const` items, so this fails the build.
            panic!("an alarm's debounce must be at least 1");
        };
        Self { debounce, ..self }
    }

    const fn new(id: &'static str, severity: Severity) -> Self {
        // PANIC Safety: specs are `const` items, so this fails the build.
        assert!(!id.is_empty(), "an alarm id must not be empty");
        Self {
            id,
            severity,
            debounce: NonZeroU16::MIN,
        }
    }
}
