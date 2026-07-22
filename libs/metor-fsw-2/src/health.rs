//! Per-system health and log telemetry.
//!
//! Systems in this crate never return errors from `execute`. Instead, every
//! system implicitly owns a pair of output ports, one carrying a
//! [`SystemHealth`] frame and one carrying [`LogEvent`] messages, and reports
//! trouble as ordinary telemetry over them. A [`HealthPort`] bundles the two
//! ports with the counter state behind them.
//!
//! A cyclic system calls
//! [`HealthPort::error`] to bump a named error counter and
//! [`HealthPort::log`] to queue a log line. The framework wraps each call to
//! `execute` and drives [`HealthPort::end_cycle`] afterwards, which stamps the
//! standard counters (cycle count, total errors, execute duration) and
//! publishes one health record plus one [`LogEvent`] per queued line.
//! A free-running [`AsyncSystem`](crate::AsyncSystem) owns its health timing
//! and must choose when to send its queued state.
//!
//! Named error counters ride the dynamic [`FrameMap`] tail of the health
//! frame, so each kind surfaces as its own `health.error_counts.<kind>`
//! component. Log lines travel as self-describing message records on a log
//! ring, so the downlink forwards them like any other message and a line is
//! never truncated to fit a frame slot.
//!
//! Pack authors can use either [`HealthPort::log`] or `tracing` macros: the
//! `export_pack!` shim installs a per-dylib forwarding subscriber
//! ([`crate::logfwd::init_pack_tracing`], `INFO` and up), and `end_cycle`
//! drains the dylib's queue onto the instance's own log port, so both paths
//! land on the same downlinked stream attributed to the instance.

use core::mem::offset_of;
use std::sync::Arc;

use metor_fsw_ring::{NoWake, WakeSource};
use metor_proto::types::Timestamp;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
// `FromBytes` lets this output-only frame also be read back through a typed
// `Input` port.

use crate::Frame;
use crate::dynamic::FrameMap;
use crate::message::MsgOut;
use crate::port::Output;

pub use metor_proto_wkt::{LogEvent, LogLevel};

/// Max distinct named error counters carried in one health record.
pub const MAX_ERR_KINDS: usize = 16;
/// Max log lines queued in one cycle; further lines are dropped and counted.
pub const MAX_LINES: usize = 64;

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

/// The handle a system uses to report errors and log lines as telemetry.
///
/// It bundles the [`SystemHealth`] and [`LogEvent`] output ports with the
/// counter state behind them, and is surfaced to a system as
/// `output.health()`. See the [module docs](self) for who calls what.
pub struct HealthPort<WD = NoWake>
where
    WD: WakeSource,
{
    health: Output<SystemHealth, WD>,
    log: MsgOut<LogEvent, WD>,
    /// The owning system's instance name, stamped into every emitted
    /// [`LogEvent`] as its `source`. Empty until the binder threads it in.
    instance: Arc<str>,
    cycles: u64,
    errors: u64,
    last_execute_micros: u64,
    error_counts: Vec<(String, u64)>,
    pending: Vec<(LogLevel, String)>,
}

impl<WD> HealthPort<WD>
where
    WD: WakeSource,
{
    /// Builds the handle from the two framework-allocated output ports.
    pub fn new(health: Output<SystemHealth, WD>, log: MsgOut<LogEvent, WD>) -> Self {
        Self {
            health,
            log,
            instance: Arc::from(""),
            cycles: 0,
            errors: 0,
            last_execute_micros: 0,
            error_counts: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Sets the instance name stamped into emitted [`LogEvent`]s as `source`.
    pub(crate) fn set_instance(&mut self, name: &str) {
        self.instance = Arc::from(name);
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
    /// past [`MAX_LINES`] are dropped and counted as `log_dropped` errors.
    pub fn log(&mut self, level: LogLevel, msg: &str) {
        if self.pending.len() < MAX_LINES {
            self.pending.push((level, msg.to_string()));
        } else {
            self.error("log_dropped");
        }
    }

    /// Emits one pre-built [`LogEvent`] directly, bypassing the pending queue
    /// so the event keeps its own timestamp and fields. The tracing drain path.
    pub(crate) fn emit_event(&mut self, ev: &LogEvent) {
        if self.log.emit(ev).is_err() {
            self.error("log_dropped");
        }
    }

    /// Closes a cycle by bumping `cycles`, stamping the execute duration, and
    /// publishing one health record plus one [`LogEvent`] per queued line.
    pub fn end_cycle(&mut self, timestamp: Timestamp, execute_micros: u64) {
        self.cycles += 1;
        self.last_execute_micros = execute_micros;
        self.publish_health(timestamp);
        self.flush_logs(timestamp);
        // Inside a pack dylib the tracing forward queue is per-dylib and the
        // loop is single-threaded, so everything queued since the last drain
        // was fired by this instance's own execute — drain it here, restamped
        // with this instance as the source. False everywhere else (the host
        // coordinator owns the host queue).
        if crate::logfwd::pack_mode() {
            let instance = self.instance.clone();
            let dropped = crate::logfwd::drain(|mut ev| {
                ev.source = instance.to_string();
                self.emit_event(&ev);
            });
            for _ in 0..dropped {
                self.error("log_dropped");
            }
        }
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
        let result = self.health.write_with(&frame, |fw| {
            let res = fw.map(
                &frame.error_counts,
                offset_of!(SystemHealth, error_counts),
                |m| {
                    for (kind, n) in counts {
                        m.insert(kind, *n);
                    }
                },
            );
            // `error` asserts every kind is a valid path segment, so the map
            // cannot reject a key here.
            debug_assert!(res.is_ok(), "error kind rejected as a map key: {res:?}");
        });
        if result.is_err() {
            self.error("health_publish_failed");
        }
    }

    fn flush_logs(&mut self, timestamp: Timestamp) {
        for (level, message) in core::mem::take(&mut self.pending) {
            let ev = LogEvent {
                timestamp,
                level,
                source: self.instance.to_string(),
                target: String::new(),
                message,
                span: None,
                fields: Vec::new(),
                file: None,
                line: None,
            };
            self.emit_event(&ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::message::{LOG_DEPTH, MAX_MSG_BYTES, MsgIn};
    use crate::port::{Input, buffer_capacity, capacity_for};
    use metor_fsw::Decomponentize;
    use metor_fsw_ring::{Config, RingBuffer};
    use metor_proto::types::{ComponentId, ComponentView};

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

    fn port_with_readers() -> (HealthPort, Input<SystemHealth>, MsgIn<LogEvent>) {
        let health_ring = RingBuffer::create_in_memory(Config {
            capacity: buffer_capacity::<SystemHealth>(8),
            max_readers: 1,
        });
        let log_ring = RingBuffer::create_in_memory(Config {
            capacity: capacity_for(MAX_MSG_BYTES, LOG_DEPTH),
            max_readers: 1,
        });
        let log_in = MsgIn::new(log_ring.view(NoWake).unwrap());
        let port = HealthPort::new(
            Output::new(health_ring.writer(NoWake).unwrap()),
            MsgOut::new(log_ring.writer(NoWake).unwrap()),
        );
        let health_in = Input::new(health_ring.view(NoWake).unwrap());
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
        let grant = health_in
            .latest()
            .expect("ring readable")
            .expect("health published");
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

        let grant = health_in
            .latest()
            .expect("ring readable")
            .expect("health published");
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
    fn log_lines_flush_as_stamped_events() {
        let (mut port, _health_in, mut log_in) = port_with_readers();
        port.set_instance("nav");
        let long = "x".repeat(500);
        port.log(LogLevel::Error, &long);
        port.log(LogLevel::Info, "line 1");
        port.end_cycle(Timestamp(3), 0);

        let mut events: Vec<LogEvent> = Vec::new();
        log_in.drain(|ev| events.push(ev)).unwrap();
        assert_eq!(events.len(), 2);
        // No fixed-size slot, so a long line survives whole.
        assert_eq!(events[0].level, LogLevel::Error);
        assert_eq!(events[0].message, long);
        assert_eq!(events[0].source, "nav");
        assert_eq!(events[0].timestamp, Timestamp(3));
        assert_eq!(events[0].span, None);
        assert_eq!(events[1].level, LogLevel::Info);
        assert_eq!(events[1].message, "line 1");
    }

    #[test]
    fn log_lines_past_the_cap_drop_and_count() {
        let (mut port, mut health_in, mut log_in) = port_with_readers();
        for i in 0..MAX_LINES + 3 {
            port.log(LogLevel::Info, &format!("line {i}"));
        }
        port.end_cycle(Timestamp(1), 0);

        let mut n = 0;
        log_in.drain(|_| n += 1).unwrap();
        assert_eq!(n, MAX_LINES);
        let grant = health_in
            .latest()
            .expect("ring readable")
            .expect("health published");
        assert_eq!(grant.get().errors, 3);
    }

    #[test]
    fn quiet_cycle_publishes_health_but_no_log() {
        let (mut port, mut health_in, mut log_in) = port_with_readers();
        port.end_cycle(Timestamp(1), 0);
        assert!(health_in.latest().unwrap().is_some());
        let mut n = 0;
        log_in.drain(|_| n += 1).unwrap();
        assert_eq!(n, 0);
    }
}
