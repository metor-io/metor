//! The `log` output every fn system has, and the `tracing` bridge onto it.
//!
//! [`Log`] writes stamped [`LogEvent`]s directly. [`layer`] queues `tracing`
//! events on the current thread, and [`drain`] moves the queue onto one
//! output, stamped with the cycle time. `FnSystem::execute` clears the queue
//! before the user's `execute` and drains it after, so a line lands on the
//! ring of the system that emitted it.

use core::cell::{Cell, RefCell};

use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;

use crate::port::{Output, SendError};
use crate::record::{DecodeError, EncodeError, Record};

/// Queued lines per cycle; later ones are dropped and counted.
pub const MAX_LINES: usize = 64;

impl Record for LogEvent {
    const NAME: &'static str = "log";
    const MAX_LEN: usize = 4096;
    const DEPTH: usize = 8;
    type Read<'a> = LogEvent;

    fn encode<'a>(&'a self, buf: &'a mut [u8]) -> Result<&'a [u8], EncodeError> {
        crate::record::postcard::encode(self, buf)
    }

    fn decode(bytes: &[u8]) -> Result<LogEvent, DecodeError> {
        crate::record::postcard::decode(bytes)
    }

    fn timestamp(&self) -> Option<Timestamp> {
        Some(self.timestamp)
    }
}

/// A `Log` writes one system's log lines, stamped with the cycle time.
pub struct Log {
    output: Output<LogEvent>,
    now: Timestamp,
}

impl Log {
    pub(crate) fn new(output: Output<LogEvent>) -> Self {
        Self {
            output,
            now: Timestamp(0),
        }
    }

    /// Sets the stamp every line written this cycle carries.
    pub(crate) fn begin(&mut self, now: Timestamp) {
        self.now = now;
    }

    /// Writes an info line.
    pub fn info(&mut self, message: impl Into<String>) {
        let _ = self.write(LogLevel::Info, message.into(), Vec::new());
    }

    /// Writes a warning line.
    pub fn warn(&mut self, message: impl Into<String>) {
        let _ = self.write(LogLevel::Warn, message.into(), Vec::new());
    }

    /// Writes an error line whose first field is `kind`, the fault's identity for the ground.
    pub fn fault(&mut self, kind: &str, message: impl Into<String>) {
        let fields = vec![("kind".to_string(), kind.to_string())];
        let _ = self.write(LogLevel::Error, message.into(), fields);
    }

    /// Writes one line with the given level and fields.
    pub fn write(
        &mut self,
        level: LogLevel,
        message: String,
        fields: Vec<(String, String)>,
    ) -> Result<(), SendError> {
        self.output.write(&LogEvent {
            timestamp: self.now,
            level,
            source: String::new(),
            target: String::new(),
            message,
            span: None,
            fields,
            file: None,
            line: None,
        })
    }

    /// Moves this thread's queued `tracing` events onto the output.
    pub(crate) fn drain_queue(&mut self) {
        drain(self.now, &mut self.output);
    }
}

thread_local! {
    static QUEUE: RefCell<Vec<LogEvent>> = RefCell::new(Vec::with_capacity(MAX_LINES));
    static DROPPED: Cell<u32> = const { Cell::new(0) };
}

/// Empties this thread's queue; events queued before a system ran are not its lines.
pub fn clear() {
    QUEUE.with(|q| q.borrow_mut().clear());
    DROPPED.set(0);
}

/// Writes every queued event onto `output` stamped `now`, then one warning if any were dropped.
pub fn drain(now: Timestamp, output: &mut Output<LogEvent>) {
    QUEUE.with(|q| {
        for mut event in q.borrow_mut().drain(..) {
            event.timestamp = now;
            let _ = output.write(&event);
        }
    });
    let dropped = DROPPED.replace(0);
    if dropped > 0 {
        let _ = output.write(&LogEvent {
            timestamp: now,
            level: LogLevel::Warn,
            source: String::new(),
            target: String::new(),
            message: "log lines dropped".to_string(),
            span: None,
            fields: vec![("dropped".to_string(), dropped.to_string())],
            file: None,
            line: None,
        });
    }
}

fn push(event: LogEvent) {
    QUEUE.with(|q| {
        let mut q = q.borrow_mut();
        if q.len() < MAX_LINES {
            q.push(event);
        } else {
            DROPPED.set(DROPPED.get().saturating_add(1));
        }
    });
}

/// Returns the layer that queues `tracing` events for [`drain`].
pub fn layer() -> LogLayer {
    LogLayer
}

/// A `LogLayer` converts each `tracing` event to a [`LogEvent`] on this thread's queue.
pub struct LogLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = Fields::default();
        event.record(&mut visitor);
        push(LogEvent {
            timestamp: Timestamp(0),
            level: level_of(meta.level()),
            source: meta.target().to_string(),
            target: meta.target().to_string(),
            message: visitor.message,
            span: None,
            fields: visitor.fields,
            file: meta.file().map(str::to_string),
            line: meta.line(),
        });
    }
}

fn level_of(level: &tracing::Level) -> LogLevel {
    match *level {
        tracing::Level::TRACE => LogLevel::Trace,
        tracing::Level::DEBUG => LogLevel::Debug,
        tracing::Level::INFO => LogLevel::Info,
        tracing::Level::WARN => LogLevel::Warn,
        tracing::Level::ERROR => LogLevel::Error,
    }
}

/// Collects an event's `message` and its other fields as strings.
#[derive(Default)]
struct Fields {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer};
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::port::{Input, ring_capacity};

    fn log_pair() -> (RingBuffer, Log, Input<LogEvent>) {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(LogEvent::MAX_LEN, 128).expect("valid"),
            max_readers: 1,
        });
        let log = Log::new(Output::try_new(ring.writer(NoWake).expect("writer")).expect("aligned"));
        let input = Input::try_new(vec![ring.view(NoWake).expect("slot")]).expect("aligned");
        (ring, log, input)
    }

    fn lines(input: &mut Input<LogEvent>) -> Vec<LogEvent> {
        input.drain().map(|r| r.expect("decodes")).collect()
    }

    #[test]
    fn direct_lines_carry_the_stamp_level_and_kind() {
        let (_ring, mut log, mut input) = log_pair();
        log.begin(Timestamp(5));
        log.info("hello");
        log.warn("careful");
        log.fault("sensor_stale", "no gps");
        let seen = lines(&mut input);
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().all(|l| l.timestamp == Timestamp(5)));
        assert_eq!(
            (seen[0].level, seen[0].message.as_str()),
            (LogLevel::Info, "hello")
        );
        assert_eq!(seen[1].level, LogLevel::Warn);
        assert_eq!(seen[2].level, LogLevel::Error);
        assert_eq!(
            seen[2].fields,
            vec![("kind".to_string(), "sensor_stale".to_string())]
        );
    }

    #[test]
    fn traced_events_drain_with_their_fields_and_location() {
        let (_ring, mut log, mut input) = log_pair();
        clear();
        let subscriber = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(slot = "nav", attempts = 3, "occupant failed");
        });
        log.begin(Timestamp(9));
        log.drain_queue();
        let seen = lines(&mut input);
        assert_eq!(seen.len(), 1);
        let ev = &seen[0];
        assert_eq!(ev.timestamp, Timestamp(9));
        assert_eq!(ev.level, LogLevel::Warn);
        assert_eq!(ev.message, "occupant failed");
        assert_eq!(ev.target, module_path!());
        assert!(ev.fields.contains(&("slot".into(), "nav".into())));
        assert!(ev.fields.contains(&("attempts".into(), "3".into())));
        assert_eq!(ev.file.as_deref(), Some(file!()));
        assert!(ev.line.is_some());
    }

    #[test]
    fn the_queue_caps_and_reports_drops_once() {
        let (_ring, mut log, mut input) = log_pair();
        clear();
        let subscriber = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            for i in 0..(MAX_LINES + 2) {
                tracing::info!(i, "line");
            }
        });
        log.begin(Timestamp(1));
        log.drain_queue();
        let seen = lines(&mut input);
        assert_eq!(seen.len(), MAX_LINES + 1);
        let last = seen.last().expect("warning");
        assert_eq!(last.level, LogLevel::Warn);
        assert_eq!(last.fields, vec![("dropped".to_string(), "2".to_string())]);
        log.drain_queue();
        assert!(lines(&mut input).is_empty());
    }

    #[test]
    fn clear_discards_events_from_before_a_system_ran() {
        let (_ring, mut log, mut input) = log_pair();
        let subscriber = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("stale");
        });
        clear();
        log.drain_queue();
        assert!(lines(&mut input).is_empty());
    }
}
