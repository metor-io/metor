//! Per-system log telemetry.
//!
//! Systems in this crate never return errors from `execute`. Instead, every
//! system implicitly owns a `log` output carrying [`LogEvent`] messages and
//! reports trouble as ordinary telemetry over it. A [`LogPort`] is the handle
//! behind that port: it queues lines during a cycle and flushes them after.
//!
//! A fault a system wants counted — a dropped publish, a corrupt input, a
//! missing sensor — is a log line like any other, tagged with a `kind` field
//! through [`LogPort::fault`]. There is no counter frame; the ground counts
//! lines by kind. A fault that recurs every cycle costs one line per cycle.
//!
//! The framework wraps each `execute` and drives [`LogPort::flush`]
//! afterwards. A free-running `AsyncSystem` flushes when it chooses. Lines
//! travel as self-describing message records on the log ring, so the
//! downlink forwards them like any other message and a line is never
//! truncated to fit a frame slot. A line the ring rejects, or one queued past
//! [`MAX_LINES`], is counted and reported as one `log_dropped` line by the
//! first flush that can land it, since a dropped line cannot announce itself.
//!
//! Pack authors can use either [`LogPort::log`] or `tracing` macros: the
//! `export_pack!` shim installs a per-dylib forwarding subscriber
//! ([`crate::logfwd::init_pack_tracing`], `INFO` and up), and `flush` drains
//! the dylib's queue onto the instance's own log port, so both paths land on
//! the same downlinked stream attributed to the instance.
//!
//! The run record every system also publishes (`system_status`) is not this
//! port's business: the host authors it. See [`crate::status`].

use core::fmt::Display;
use std::sync::Arc;

use metor_fsw_ring::{NoWake, WakeSource};
use metor_proto::types::Timestamp;

use crate::message::MsgOut;

pub use metor_proto_wkt::{LogEvent, LogLevel};

/// Max log lines queued in one cycle; further lines are dropped and counted.
pub const MAX_LINES: usize = 64;

/// The handle a system logs through, surfaced as `output.log()`.
pub struct LogPort<WD = NoWake>
where
    WD: WakeSource,
{
    log: MsgOut<LogEvent, WD>,
    /// The owning system's instance name, stamped into every emitted
    /// [`LogEvent`] as its `source`. Empty until the binder threads it in.
    instance: Arc<str>,
    /// Lines queued this cycle; stamped with the cycle timestamp at flush.
    pending: Vec<LogEvent>,
    /// Lines lost since the last successful report of them.
    dropped: u64,
}

impl<WD> LogPort<WD>
where
    WD: WakeSource,
{
    /// Builds the handle from the framework-allocated log port.
    pub fn new(log: MsgOut<LogEvent, WD>) -> Self {
        Self {
            log,
            instance: Arc::from(""),
            pending: Vec::new(),
            dropped: 0,
        }
    }

    /// Sets the instance name stamped into emitted [`LogEvent`]s as `source`.
    pub fn set_instance(&mut self, name: &str) {
        self.instance = Arc::from(name);
    }

    // ---- system-facing API ----

    /// Queues a log line for the next [`flush`](Self::flush).
    pub fn log(&mut self, level: LogLevel, msg: &str) {
        self.queue(level, msg, &[]);
    }

    /// Queues a fault line: a log line whose first field is `kind=<kind>`,
    /// so the ground can count faults by kind. A kind is a single word; use
    /// `_` to separate words.
    pub fn fault(
        &mut self,
        level: LogLevel,
        kind: &str,
        msg: &str,
        fields: &[(&str, &dyn Display)],
    ) {
        let mut all = Vec::with_capacity(fields.len() + 1);
        all.push(("kind", &kind as &dyn Display));
        all.extend_from_slice(fields);
        self.queue(level, msg, &all);
    }

    /// Emits one pre-built [`LogEvent`] directly, bypassing the queue so the
    /// event keeps its own timestamp and fields. The tracing drain path.
    pub fn emit_event(&mut self, ev: &LogEvent) {
        if self.log.emit(ev).is_err() {
            self.dropped += 1;
        }
    }

    /// Adds `n` to the lines lost outside this port (a forward queue that
    /// overflowed, say), reported by the next successful flush.
    pub fn note_dropped(&mut self, n: u64) {
        self.dropped += n;
    }

    /// Closes a cycle: stamps and emits the queued lines, inside a pack dylib
    /// drains the tracing forward queue onto this port, then reports the
    /// lines lost since the last report.
    pub fn flush(&mut self, timestamp: Timestamp) {
        for mut ev in core::mem::take(&mut self.pending) {
            ev.timestamp = timestamp;
            self.emit_event(&ev);
        }
        // Inside a pack dylib the tracing forward queue is per-dylib and the
        // loop is single-threaded, so everything queued since the last drain
        // was fired by this instance's own execute — drain it here, restamped
        // with this instance as the source. False everywhere else (the host
        // coordinator owns the host queue).
        if crate::logfwd::pack_mode() {
            let instance = self.instance.clone();
            self.dropped += crate::logfwd::drain(|mut ev| {
                ev.source = instance.to_string();
                self.emit_event(&ev);
            });
        }
        self.report_dropped(timestamp);
    }

    fn queue(&mut self, level: LogLevel, msg: &str, fields: &[(&str, &dyn Display)]) {
        if self.pending.len() >= MAX_LINES {
            self.dropped += 1;
            return;
        }
        self.pending.push(LogEvent {
            timestamp: Timestamp(0),
            level,
            source: self.instance.to_string(),
            target: String::new(),
            message: msg.to_string(),
            span: None,
            fields: fields
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            file: None,
            line: None,
        });
    }

    /// One line for every drop since the last report. The count clears only
    /// when the line lands, so a ring that is still full keeps accumulating.
    fn report_dropped(&mut self, timestamp: Timestamp) {
        if self.dropped == 0 {
            return;
        }
        let dropped = self.dropped;
        let ev = LogEvent {
            timestamp,
            level: LogLevel::Warn,
            source: self.instance.to_string(),
            target: String::new(),
            message: "log lines dropped".to_string(),
            span: None,
            fields: vec![
                ("kind".to_string(), "log_dropped".to_string()),
                ("dropped".to_string(), dropped.to_string()),
            ],
            file: None,
            line: None,
        };
        if self.log.emit(&ev).is_ok() {
            self.dropped = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{LOG_DEPTH, MAX_MSG_BYTES, MsgIn};
    use crate::port::capacity_for;
    use metor_fsw_ring::{Config, RingBuffer};

    fn port_with_reader(depth: usize) -> (LogPort, MsgIn<LogEvent>) {
        let log_ring = RingBuffer::create_in_memory(Config {
            capacity: capacity_for(MAX_MSG_BYTES, depth),
            max_readers: 1,
        });
        let log_in = MsgIn::new(log_ring.view(NoWake).unwrap());
        let mut port = LogPort::new(MsgOut::new(log_ring.writer(NoWake).unwrap()));
        port.set_instance("nav");
        (port, log_in)
    }

    fn drain(log_in: &mut MsgIn<LogEvent>) -> Vec<LogEvent> {
        let mut events = Vec::new();
        log_in.drain(|ev| events.push(ev)).unwrap();
        events
    }

    fn field<'a>(ev: &'a LogEvent, key: &str) -> Option<&'a str> {
        ev.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn log_lines_flush_as_stamped_events() {
        let (mut port, mut log_in) = port_with_reader(LOG_DEPTH);
        let long = "x".repeat(500);
        port.log(LogLevel::Error, &long);
        port.log(LogLevel::Info, "line 1");
        port.flush(Timestamp(3));

        let events = drain(&mut log_in);
        assert_eq!(events.len(), 2);
        // No fixed-size slot, so a long line survives whole.
        assert_eq!(events[0].level, LogLevel::Error);
        assert_eq!(events[0].message, long);
        assert_eq!(events[0].source, "nav");
        assert_eq!(events[0].timestamp, Timestamp(3));
        assert_eq!(events[0].span, None);
        assert!(events[0].fields.is_empty());
        assert_eq!(events[1].level, LogLevel::Info);
        assert_eq!(events[1].message, "line 1");
    }

    #[test]
    fn faults_carry_their_kind_first() {
        let (mut port, mut log_in) = port_with_reader(LOG_DEPTH);
        port.fault(
            LogLevel::Warn,
            "imu_missing",
            "no imu sample",
            &[("age_us", &1500)],
        );
        port.flush(Timestamp(1));

        let events = drain(&mut log_in);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, LogLevel::Warn);
        assert_eq!(
            events[0].fields[0],
            ("kind".to_string(), "imu_missing".to_string())
        );
        assert_eq!(field(&events[0], "age_us"), Some("1500"));
    }

    #[test]
    fn lines_past_the_cap_are_reported_as_one_line() {
        let (mut port, mut log_in) = port_with_reader(LOG_DEPTH);
        for i in 0..MAX_LINES + 3 {
            port.log(LogLevel::Info, &format!("line {i}"));
        }
        port.flush(Timestamp(1));

        // The kept lines, then one line for the three lost ones.
        let events = drain(&mut log_in);
        assert_eq!(events.len(), MAX_LINES + 1);
        let report = events.last().unwrap();
        assert_eq!(field(report, "kind"), Some("log_dropped"));
        assert_eq!(field(report, "dropped"), Some("3"));
        assert_eq!(report.timestamp, Timestamp(1));

        // Reported once; a quiet flush emits nothing.
        port.flush(Timestamp(2));
        assert!(drain(&mut log_in).is_empty());
    }

    #[test]
    fn a_line_the_ring_rejects_is_reported() {
        // A line larger than the ring can ever hold is rejected at emit;
        // the loss is reported by the same flush, after the lines that fit.
        let (mut port, mut log_in) = port_with_reader(2);
        let huge = "x".repeat(4 * MAX_MSG_BYTES);
        port.log(LogLevel::Info, &huge);
        port.log(LogLevel::Info, "fits");
        port.flush(Timestamp(1));

        let events = drain(&mut log_in);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].message, "fits");
        assert_eq!(field(&events[1], "kind"), Some("log_dropped"));
        assert_eq!(field(&events[1], "dropped"), Some("1"));
    }

    #[test]
    fn quiet_flush_emits_nothing() {
        let (mut port, mut log_in) = port_with_reader(LOG_DEPTH);
        port.flush(Timestamp(1));
        assert!(drain(&mut log_in).is_empty());
    }
}
