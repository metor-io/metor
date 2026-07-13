//! Per-system health and log telemetry.
//!
//! Systems in this crate never return errors from `execute`. Instead, every
//! system implicitly owns a pair of output ports, one carrying a
//! [`SystemHealth`] frame and one carrying a [`SystemLog`] frame, and reports
//! trouble as ordinary telemetry over them. A [`HealthPort`] bundles the two
//! ports with the counter state behind them.
//!
//! The split of responsibilities is fixed. The system itself only calls
//! [`HealthPort::error`] to bump a named error counter and
//! [`HealthPort::log`] to queue a log line. The framework wraps each call to
//! `execute` and drives [`HealthPort::end_cycle`] afterwards, which stamps the
//! standard counters (cycle count, total errors, execute duration) and
//! publishes one health record plus any queued log lines.
//!
//! Named error counters ride the dynamic [`FrameMap`] tail of the health
//! frame, so each kind surfaces as its own `health.error_counts.<kind>`
//! component. Log messages are fixed-size byte arrays because frames have no
//! string type; lines longer than [`LOG_MSG_CAP`] are truncated.

use core::mem::offset_of;

use metor_fsw_ring::{NoWake, WakeSource};
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
// `FromBytes` lets these output-only frames also be read back through a typed
// `Input` port.

use crate::Frame;
use crate::dynamic::{FrameList, FrameMap, pack_str};
use crate::port::Output;

/// Max distinct named error counters carried in one health record.
pub const MAX_ERR_KINDS: usize = 16;
/// Max log lines flushed in one log record.
pub const MAX_LINES: usize = 16;
/// Byte capacity of one log line's message; longer lines are truncated.
pub const LOG_MSG_CAP: usize = 64;

/// A telemetry frame that snapshots one system's run counters at the end of
/// each cycle.
///
/// The scalar counters are maintained by the framework around `execute`;
/// `error_counts` holds the named counters bumped through
/// [`HealthPort::error`] and lands as one `health.error_counts.<kind>`
/// component per kind.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "health")]
pub struct SystemHealth {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub cycles: u64,
    pub errors: u64,
    pub last_execute_micros: u64,
    pub error_counts: FrameMap<u64, MAX_ERR_KINDS>,
}

/// A fixed-size log entry carried on the [`SystemLog`] frame, holding a
/// severity level, the used byte length, and the message bytes.
#[derive(metor_fsw::AsVTable, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy)]
#[repr(C)]
pub struct LogLine {
    pub level: u8,
    pub len: u8,
    pub _pad: [u8; 6],
    pub msg: [u8; LOG_MSG_CAP],
}

impl LogLine {
    fn new(level: Level, msg: &str) -> Self {
        let (msg, len) = pack_str::<LOG_MSG_CAP>(msg);
        Self {
            level: level as u8,
            len,
            _pad: [0; 6],
            msg,
        }
    }
}

/// A telemetry frame carrying the lines a system queued through
/// [`HealthPort::log`] during one cycle.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "log")]
pub struct SystemLog {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub lines: FrameList<LogLine, MAX_LINES>,
}

/// Log severity, stored as the [`LogLine::level`] byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Level {
    Info = 0,
    Warn = 1,
    Error = 2,
}

/// The handle a system uses to report errors and log lines as telemetry.
///
/// It bundles the [`SystemHealth`] and [`SystemLog`] output ports with the
/// counter state behind them, and is surfaced to a system as
/// `output.health()`. See the [module docs](self) for who calls what.
pub struct HealthPort<WD = NoWake>
where
    WD: WakeSource,
{
    health: Output<SystemHealth, WD>,
    log: Output<SystemLog, WD>,
    cycles: u64,
    errors: u64,
    last_execute_micros: u64,
    error_counts: Vec<(String, u64)>,
    pending: Vec<LogLine>,
}

impl<WD> HealthPort<WD>
where
    WD: WakeSource,
{
    /// Builds the handle from the two framework-allocated output ports.
    pub fn new(health: Output<SystemHealth, WD>, log: Output<SystemLog, WD>) -> Self {
        Self {
            health,
            log,
            cycles: 0,
            errors: 0,
            last_execute_micros: 0,
            error_counts: Vec::new(),
            pending: Vec::new(),
        }
    }

    // ---- system-facing API ----

    /// Bumps the named error counter along with the total `errors` count.
    ///
    /// Kinds beyond [`MAX_ERR_KINDS`] still count toward the total but get no
    /// counter of their own. The kind becomes a path segment of the counter's
    /// component (`health.error_counts.<kind>`), so it must be non-empty and
    /// must not contain `.` — a key the map rejects would drop every counter
    /// from the health record. Use `_` to separate words.
    pub fn error(&mut self, kind: &str) {
        debug_assert!(
            !kind.is_empty() && !kind.contains('.'),
            "error kind {kind:?} is not a valid path segment; use `_`, not `.`"
        );
        self.errors += 1;
        if let Some((_, n)) = self.error_counts.iter_mut().find(|(k, _)| k == kind) {
            *n += 1;
        } else if self.error_counts.len() < MAX_ERR_KINDS {
            self.error_counts.push((kind.to_string(), 1));
        }
    }

    /// Queues a log line for the next [`end_cycle`](Self::end_cycle); lines
    /// past [`MAX_LINES`] are dropped.
    pub fn log(&mut self, level: Level, msg: &str) {
        if self.pending.len() < MAX_LINES {
            self.pending.push(LogLine::new(level, msg));
        }
    }

    /// Closes a cycle by bumping `cycles`, stamping the execute duration, and
    /// publishing one health record plus any queued log lines.
    pub fn end_cycle(&mut self, timestamp: Timestamp, execute_micros: u64) {
        self.cycles += 1;
        self.last_execute_micros = execute_micros;
        self.publish_health(timestamp);
        self.flush_logs(timestamp);
    }

    fn publish_health(&mut self, timestamp: Timestamp) {
        let frame = SystemHealth {
            timestamp,
            cycles: self.cycles,
            errors: self.errors,
            last_execute_micros: self.last_execute_micros,
            error_counts: FrameMap::EMPTY,
        };
        let counts = &self.error_counts;
        let _ = self.health.write_with(&frame, |fw| {
            let res = fw.map(offset_of!(SystemHealth, error_counts), |m| {
                for (kind, n) in counts {
                    m.insert(kind, *n);
                }
            });
            // `error` asserts every kind is a valid path segment, so the map
            // cannot reject a key here.
            debug_assert!(res.is_ok(), "error kind rejected as a map key: {res:?}");
        });
    }

    fn flush_logs(&mut self, timestamp: Timestamp) {
        if self.pending.is_empty() {
            return;
        }
        let frame = SystemLog {
            timestamp,
            lines: FrameList::EMPTY,
        };
        let pending = core::mem::take(&mut self.pending);
        let _ = self.log.write_with(&frame, |fw| {
            fw.list(offset_of!(SystemLog, lines), |l| {
                for line in &pending {
                    l.push(*line);
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::port::{Input, buffer_capacity, frame_list_iter};
    use metor_fsw::Decomponentize;
    use metor_proto::types::{ComponentId, ComponentView};
    use metor_fsw_ring::{Config, RingBuffer};

    /// Collects the `error_counts.<kind>` map members published on a health
    /// record, keyed by their component id, off the vtable `apply` path.
    #[derive(Default)]
    struct CountSink(HashMap<ComponentId, u64>);

    impl Decomponentize for CountSink {
        type Error = core::convert::Infallible;
        fn apply_value(
            &mut self,
            component_id: ComponentId,
            value: ComponentView<'_>,
            _timestamp: Option<metor_proto::types::Timestamp>,
        ) -> Result<(), Self::Error> {
            self.0.insert(component_id, value.to_f64() as u64);
            Ok(())
        }
    }

    impl CountSink {
        fn count(&self, kind: &str) -> Option<u64> {
            self.0
                .get(&ComponentId::new(&format!("health.error_counts.{kind}")))
                .copied()
        }
    }

    fn port_with_readers() -> (HealthPort, Input<SystemHealth>, Input<SystemLog>) {
        let health_ring = RingBuffer::create_in_memory(Config {
            capacity: buffer_capacity::<SystemHealth>(8),
            max_readers: 1,
        });
        let log_ring = RingBuffer::create_in_memory(Config {
            capacity: buffer_capacity::<SystemLog>(8),
            max_readers: 1,
        });
        let port = HealthPort::new(
            Output::new(health_ring.writer(NoWake).unwrap()),
            Output::new(log_ring.writer(NoWake).unwrap()),
        );
        let health_in = Input::new(health_ring.view(NoWake).unwrap());
        let log_in = Input::new(log_ring.view(NoWake).unwrap());
        (port, health_in, log_in)
    }

    #[test]
    fn error_counts_land_in_the_published_frame() {
        let (mut port, mut health_in, _log_in) = port_with_readers();
        port.error("imu_missing");
        port.error("imu_missing");
        port.error("i2c_timeout");
        port.end_cycle(Timestamp(7), 12);

        // Field access derefs through the grant; no `.get()` needed.
        let grant = health_in.latest().expect("health published");
        assert_eq!(grant.timestamp, Timestamp(7));
        assert_eq!(grant.cycles, 1);
        assert_eq!(grant.errors, 3);
        assert_eq!(grant.last_execute_micros, 12);
        let mut counts = CountSink::default();
        grant.apply(&mut counts).unwrap().unwrap();
        assert_eq!(counts.count("imu_missing"), Some(2));
        assert_eq!(counts.count("i2c_timeout"), Some(1));
        assert_eq!(counts.count("absent"), None);
    }

    #[test]
    fn kinds_past_the_cap_count_toward_the_total_only() {
        let (mut port, mut health_in, _log_in) = port_with_readers();
        for i in 0..MAX_ERR_KINDS + 2 {
            port.error(&format!("kind_{i}"));
        }
        // A capped-out kind still folds into an existing counter.
        port.error("kind_0");
        port.end_cycle(Timestamp(1), 0);

        let grant = health_in.latest().expect("health published");
        assert_eq!(grant.get().errors, (MAX_ERR_KINDS + 3) as u64);
        let mut counts = CountSink::default();
        grant.apply(&mut counts).unwrap().unwrap();
        let present = (0..MAX_ERR_KINDS + 2)
            .filter(|i| counts.count(&format!("kind_{i}")).is_some())
            .count();
        assert_eq!(present, MAX_ERR_KINDS);
        assert_eq!(counts.count("kind_0"), Some(2));
        // The kinds past the cap got no counter of their own.
        assert_eq!(counts.count(&format!("kind_{MAX_ERR_KINDS}")), None);
    }

    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "not a valid path segment"))]
    fn dotted_kind_is_rejected_in_debug() {
        let (mut port, _health_in, _log_in) = port_with_readers();
        port.error("telemetry.dropped");
        // In release builds the assert compiles out; nothing to check here
        // beyond "does not corrupt the counter list", which the publish-path
        // debug_assert covers in debug builds.
    }

    #[test]
    fn log_lines_are_capped_and_truncated() {
        let (mut port, _health_in, mut log_in) = port_with_readers();
        let long = "x".repeat(LOG_MSG_CAP + 20);
        port.log(Level::Error, &long);
        for i in 0..MAX_LINES + 3 {
            port.log(Level::Info, &format!("line {i}"));
        }
        port.end_cycle(Timestamp(3), 0);

        let grant = log_in.latest().expect("log published");
        let lines: Vec<LogLine> =
            frame_list_iter(grant.table(), offset_of!(SystemLog, lines)).collect();
        assert_eq!(lines.len(), MAX_LINES);
        let first = lines[0];
        assert_eq!(first.level, Level::Error as u8);
        assert_eq!(first.len as usize, LOG_MSG_CAP);
        assert_eq!(&first.msg[..], &long.as_bytes()[..LOG_MSG_CAP]);
    }

    #[test]
    fn quiet_cycle_publishes_health_but_no_log() {
        let (mut port, mut health_in, mut log_in) = port_with_readers();
        port.end_cycle(Timestamp(1), 0);
        assert!(health_in.latest().is_some());
        assert!(log_in.latest().is_none());
    }
}
