//! Per-system health & log telemetry (system.md §4).
//!
//! Systems never return errors; they report trouble as ordinary frames over a
//! framework-provided health output port that every system implicitly gets. The
//! three standard counters (cycles / errors / last execute micros) are
//! maintained by the framework around `execute`; domain-specific error kinds
//! are bumped by name and ride a dynamic `FrameMap`. String logs ride a parallel
//! [`SystemLog`] frame as fixed-size byte components (metor-proto has no string
//! type).

use core::mem::offset_of;

use metor_fsw_ring::{Backing, BoxBacking, NoWake, WakeSink, WakeSource};
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
// `FromBytes` lets the health/log frames be read back through a typed `Input` port.

use crate::Frame;
use crate::dynamic::{FrameList, FrameMap, pack_str};
use crate::port::Output;

/// Max distinct domain-error kinds carried in one health record.
pub const MAX_ERR_KINDS: usize = 16;
/// Max log lines flushed in one log record.
pub const MAX_LINES: usize = 16;
/// Fixed capacity of one log line's message (longer lines are truncated, §Q8).
pub const LOG_MSG_CAP: usize = 64;

/// The standard per-system health frame. The scalar counters are framework-
/// maintained; `error_counts` holds the system's named counters (lands as
/// `health.error_counts.<kind>` via the dynamic-frame path).
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

/// One log line: a level, a used-length, and a fixed-size message buffer. The
/// `msg` array is a `U8` component (`health... .log.lines.N.msg`).
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

/// The standard per-system log frame.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[repr(C)]
#[metor_fsw(name = "log")]
pub struct SystemLog {
    #[metor_fsw(timestamp)]
    pub timestamp: Timestamp,
    pub lines: FrameList<LogLine, MAX_LINES>,
}

/// Log severity. Stored as the `LogLine::level` byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Level {
    Info = 0,
    Warn = 1,
    Error = 2,
}

/// The health/log port pair plus the framework-maintained counter state. Surfaced
/// to a system as `output.health()`; the framework drives [`HealthPort::end_cycle`]
/// around each `execute` to publish a record (system.md §4.2).
pub struct HealthPort<B = BoxBacking, WD = NoWake, WS = NoWake>
where
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    health: Output<SystemHealth, B, WD, WS>,
    log: Output<SystemLog, B, WD, WS>,
    cycles: u64,
    errors: u64,
    last_execute_micros: u64,
    error_counts: Vec<(String, u64)>,
    pending: Vec<LogLine>,
}

impl<B, WD, WS> HealthPort<B, WD, WS>
where
    B: Backing,
    WD: WakeSource,
    WS: WakeSink,
{
    /// Build the handle from the two framework-allocated output ports.
    pub fn new(health: Output<SystemHealth, B, WD, WS>, log: Output<SystemLog, B, WD, WS>) -> Self {
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

    // ---- system-facing API (the only error mechanism) ----

    /// Bump a named domain error counter (and the standard `errors` total).
    pub fn error(&mut self, kind: &str) {
        self.errors += 1;
        if let Some((_, n)) = self.error_counts.iter_mut().find(|(k, _)| k == kind) {
            *n += 1;
        } else if self.error_counts.len() < MAX_ERR_KINDS {
            self.error_counts.push((kind.to_string(), 1));
        }
    }

    /// Append a log line (flushed as part of the next [`end_cycle`](Self::end_cycle)).
    pub fn log(&mut self, level: Level, msg: &str) {
        if self.pending.len() < MAX_LINES {
            self.pending.push(LogLine::new(level, msg));
        }
    }

    // ---- framework-side counter maintenance ----

    /// Close a cycle: bump `cycles`, stamp the execute duration, and publish one
    /// health record (with the named counters) plus any pending log lines.
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
