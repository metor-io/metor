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

use metor_fsw_ring::{NoWake, WakeSink, WakeSource};
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
pub struct HealthPort<WD = NoWake, WS = NoWake>
where
    WD: WakeSource,
    WS: WakeSink,
{
    health: Output<SystemHealth, WD, WS>,
    log: Output<SystemLog, WD, WS>,
    cycles: u64,
    errors: u64,
    last_execute_micros: u64,
    error_counts: Vec<(String, u64)>,
    pending: Vec<LogLine>,
}

impl<WD, WS> HealthPort<WD, WS>
where
    WD: WakeSource,
    WS: WakeSink,
{
    /// Builds the handle from the two framework-allocated output ports.
    pub fn new(health: Output<SystemHealth, WD, WS>, log: Output<SystemLog, WD, WS>) -> Self {
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
    /// counter of their own.
    pub fn error(&mut self, kind: &str) {
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

    // ---- framework-side counter maintenance ----

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
            let _ = fw.map(offset_of!(SystemHealth, error_counts), |m| {
                for (kind, n) in counts {
                    m.insert(kind, *n);
                }
            });
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
