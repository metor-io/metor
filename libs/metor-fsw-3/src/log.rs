//! Log is a tracing_subscriber that takes tracing logs and puts them into [`LogEvent`] messages

use core::cell::Cell;
use core::ptr::NonNull;
use std::borrow::Cow;

use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;

use crate::port::Output;
use crate::record::{DecodeError, EncodeError, Record};

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

    fn schema() -> crate::record::RecordSchema {
        crate::record::RecordSchema::postcard::<LogEvent>(Self::NAME)
    }
}

/// A `LogPort` writes one system's log lines, stamped with the cycle time.
pub struct LogPort {
    output: Output<LogEvent>,
    now: Timestamp,
    dropped: u32,
}

impl LogPort {
    pub fn new(output: Output<LogEvent>) -> Self {
        Self {
            output,
            now: Timestamp(0),
            dropped: 0,
        }
    }

    /// Writes one line with the given level and fields, stamped with the cycle time.
    fn write(
        &mut self,
        level: LogLevel,
        message: Cow<'static, str>,
        fields: Vec<(Cow<'static, str>, Cow<'static, str>)>,
    ) {
        self.emit(LogEvent {
            timestamp: self.now,
            level,
            source: Cow::Borrowed(""),
            target: Cow::Borrowed(""),
            message,
            span: None,
            fields,
            file: None,
            line: None,
        });
    }

    /// Writes an error line whose first field is `kind`, the fault's identity for the ground.
    pub(crate) fn fault(
        &mut self,
        now: Timestamp,
        kind: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
    ) {
        self.now = now;
        let fields = vec![(Cow::Borrowed("kind"), kind.into())];
        self.write(LogLevel::Error, message.into(), fields);
    }

    /// Stamps and publishes one event; a ring with no room counts as a drop.
    fn emit(&mut self, mut event: LogEvent) {
        event.timestamp = self.now;
        if self.output.write(&event).is_err() {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    /// Writes the count of lines the ring refused, clearing it only if the line lands.
    fn report_dropped(&mut self) {
        if self.dropped == 0 {
            return;
        }
        let event = LogEvent {
            timestamp: self.now,
            level: LogLevel::Warn,
            source: Cow::Borrowed(""),
            target: Cow::Borrowed(""),
            message: Cow::Borrowed("log lines dropped"),
            span: None,
            fields: vec![(Cow::Borrowed("dropped"), self.dropped.to_string().into())],
            file: None,
            line: None,
        };
        if self.output.write(&event).is_ok() {
            self.dropped = 0;
        }
    }
}

/// A `Log` writes the running system's log lines onto the entered [`LogPort`].
pub struct Log;

impl Log {
    /// Writes an info line.
    pub fn info(&mut self, message: impl Into<Cow<'static, str>>) {
        self.write(LogLevel::Info, message, Vec::new());
    }

    /// Writes a warning line.
    pub fn warn(&mut self, message: impl Into<Cow<'static, str>>) {
        self.write(LogLevel::Warn, message, Vec::new());
    }

    /// Writes an error line whose first field is `kind`, the fault's identity for the ground.
    pub fn fault(
        &mut self,
        kind: impl Into<Cow<'static, str>>,
        message: impl Into<Cow<'static, str>>,
    ) {
        let fields = vec![(Cow::Borrowed("kind"), kind.into())];
        self.write(LogLevel::Error, message, fields);
    }

    /// Writes one line with the given level and fields.
    pub fn write(
        &mut self,
        level: LogLevel,
        message: impl Into<Cow<'static, str>>,
        fields: Vec<(Cow<'static, str>, Cow<'static, str>)>,
    ) {
        let message = message.into();
        with_port(|port| port.write(level, message, fields));
    }
}

thread_local! {
    static SLOT: Cell<Option<NonNull<LogPort>>> = const { Cell::new(None) };
}

/// Routes this thread's logs to `port` during `f`, stamping its lines `now`.
///
/// ```compile_fail
/// use metor_fsw_3::log::{LogPort, with_log_port};
/// use metor_proto::types::Timestamp;
///
/// fn cannot_drop_port(mut port: LogPort) {
///     with_log_port(&mut port, Timestamp(0), || drop(port));
/// }
/// ```
pub fn with_log_port<R>(port: &mut LogPort, now: Timestamp, f: impl FnOnce() -> R) -> R {
    port.now = now;
    let _restore = RestoreSlot(SLOT.replace(Some(NonNull::from(port))));
    let result = f();
    with_port(LogPort::report_dropped);
    result
}

struct RestoreSlot(Option<NonNull<LogPort>>);

impl Drop for RestoreSlot {
    fn drop(&mut self) {
        SLOT.set(self.0);
    }
}

fn with_port(f: impl FnOnce(&mut LogPort)) {
    let Some(mut port) = SLOT.take() else {
        return;
    };
    let _restore = RestoreSlot(Some(port));
    // SAFETY: with_log_port borrows the port until its private guard restores
    // the previous slot, including on unwind. Removing this thread-local
    // pointer for the entire callback prevents recursive mutable access.
    f(unsafe { port.as_mut() });
}

/// Returns the layer that writes `tracing` events to the entered [`LogPort`].
pub fn layer() -> LogLayer {
    LogLayer
}

/// A `LogLayer` writes each `tracing` event as a [`LogEvent`] on this thread's port.
pub struct LogLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for LogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        with_port(|port| {
            let meta = event.metadata();
            let mut visitor = Fields::default();
            event.record(&mut visitor);
            port.emit(LogEvent {
                timestamp: Timestamp(0),
                level: level_of(meta.level()),
                source: Cow::Borrowed(meta.target()),
                target: Cow::Borrowed(meta.target()),
                message: visitor.message,
                span: None,
                fields: visitor.fields,
                file: meta.file().map(Cow::Borrowed),
                line: meta.line(),
            });
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
    message: Cow<'static, str>,
    fields: Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn core::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}").into();
        } else {
            self.fields
                .push((Cow::Borrowed(field.name()), format!("{value:?}").into()));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string().into();
        } else {
            self.fields
                .push((Cow::Borrowed(field.name()), value.to_string().into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use metor_fsw_3_ring::{Config, NoWake, RingBuffer, frame_len};
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::port::{Input, ring_capacity};

    /// A port whose ring holds `records` lines of `MAX_LEN`.
    fn log_pair(records: usize) -> (RingBuffer, LogPort, Input<LogEvent>) {
        let ring = RingBuffer::create_in_memory(Config {
            capacity: ring_capacity(LogEvent::MAX_LEN, records).expect("valid"),
            max_readers: 1,
        });
        let port =
            LogPort::new(Output::try_new(ring.writer(NoWake).expect("writer")).expect("aligned"));
        let input = Input::try_new(vec![ring.view(NoWake).expect("slot")]).expect("aligned");
        (ring, port, input)
    }

    fn lines(input: &mut Input<LogEvent>) -> Vec<LogEvent> {
        input.drain().map(|r| r.expect("decodes")).collect()
    }

    #[test]
    fn direct_lines_carry_the_stamp_level_and_kind() {
        let (_ring, mut port, mut input) = log_pair(8);
        let mut log = Log;
        with_log_port(&mut port, Timestamp(5), || {
            log.info("hello");
            log.warn("careful");
            log.fault("sensor_stale", "no gps");
        });
        let seen = lines(&mut input);
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().all(|l| l.timestamp == Timestamp(5)));
        assert_eq!(
            (seen[0].level, &*seen[0].message),
            (LogLevel::Info, "hello")
        );
        assert_eq!(seen[1].level, LogLevel::Warn);
        assert_eq!(seen[2].level, LogLevel::Error);
        assert_eq!(seen[2].fields, vec![("kind".into(), "sensor_stale".into())]);
    }

    #[test]
    fn traced_events_land_with_their_fields_and_location() {
        let (_ring, mut port, mut input) = log_pair(8);
        let subscriber = tracing_subscriber::registry().with(layer());
        with_log_port(&mut port, Timestamp(9), || {
            tracing::subscriber::with_default(subscriber, || {
                tracing::warn!(slot = "nav", attempts = 3, "occupant failed");
            });
        });
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
    fn an_event_with_no_port_entered_reaches_no_ring() {
        let (_ring, mut port, mut input) = log_pair(8);
        let subscriber = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("between systems");
        });
        with_log_port(&mut port, Timestamp(0), || {});
        assert!(lines(&mut input).is_empty());
    }

    #[test]
    fn nested_scopes_restore_the_outer_port() {
        let (_outer_ring, mut outer, mut outer_input) = log_pair(8);
        let (_inner_ring, mut inner, mut inner_input) = log_pair(8);
        let result = with_log_port(&mut outer, Timestamp(1), || {
            Log.info("before");
            with_log_port(&mut inner, Timestamp(2), || Log.info("inner"));
            Log.info("after");
            42
        });
        Log.info("outside");
        assert_eq!(result, 42);
        let outer = lines(&mut outer_input);
        assert_eq!(outer.len(), 2);
        assert_eq!(outer[0].message, "before");
        assert_eq!(outer[1].message, "after");
        assert!(outer.iter().all(|line| line.timestamp == Timestamp(1)));
        let inner = lines(&mut inner_input);
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].message, "inner");
        assert_eq!(inner[0].timestamp, Timestamp(2));
    }

    #[test]
    fn callback_panic_restores_the_previous_slot() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let (_outer_ring, mut outer, mut outer_input) = log_pair(8);
        let (_inner_ring, mut inner, mut inner_input) = log_pair(8);
        with_log_port(&mut outer, Timestamp(1), || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                with_log_port(&mut inner, Timestamp(2), || {
                    Log.info("inner");
                    // PANIC Safety: caught to verify scope cleanup.
                    panic!("callback failed");
                });
            }));
            assert!(result.is_err());
            drop(inner);
            Log.info("outer");
        });
        drop(outer);
        Log.info("outside");
        assert!(SLOT.get().is_none());
        assert_eq!(lines(&mut inner_input).len(), 1);
        let outer = lines(&mut outer_input);
        assert_eq!(outer.len(), 1);
        assert_eq!(outer[0].message, "outer");
    }

    #[test]
    fn scopes_are_independent_between_threads() {
        let (_ring, mut port, mut input) = log_pair(8);
        with_log_port(&mut port, Timestamp(1), || {
            let worker = std::thread::spawn(|| {
                assert!(SLOT.get().is_none());
                let (_ring, mut port, mut input) = log_pair(8);
                with_log_port(&mut port, Timestamp(2), || Log.info("worker"));
                lines(&mut input)
            });
            Log.info("main");
            let worker = worker.join().expect("worker completed");
            assert_eq!(worker.len(), 1);
            assert_eq!(worker[0].timestamp, Timestamp(2));
            assert_eq!(worker[0].message, "worker");
            Log.info("main after worker");
        });
        let main = lines(&mut input);
        assert_eq!(main.len(), 2);
        assert!(main.iter().all(|line| line.timestamp == Timestamp(1)));
    }

    #[test]
    fn formatting_cannot_reenter_the_active_port() {
        struct Recursive;

        impl core::fmt::Debug for Recursive {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                Log.info("recursive direct log");
                tracing::info!("recursive tracing event");
                f.write_str("formatted")
            }
        }

        let (_ring, mut port, mut input) = log_pair(8);
        let subscriber = tracing_subscriber::registry().with(layer());
        with_log_port(&mut port, Timestamp(3), || {
            tracing::subscriber::with_default(subscriber, || {
                tracing::info!(value = ?Recursive, "outer");
            });
            Log.info("after formatting");
        });
        let seen = lines(&mut input);
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].message, "outer");
        assert_eq!(seen[0].fields, vec![("value".into(), "formatted".into())]);
        assert_eq!(seen[1].message, "after formatting");
    }

    #[test]
    fn formatting_panic_restores_the_active_port() {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        struct Panics;

        impl core::fmt::Debug for Panics {
            fn fmt(&self, _: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                // PANIC Safety: caught to verify the temporarily removed slot.
                panic!("formatting failed");
            }
        }

        let (_ring, mut port, mut input) = log_pair(8);
        let subscriber = tracing_subscriber::registry().with(layer());
        with_log_port(&mut port, Timestamp(4), || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                tracing::subscriber::with_default(subscriber, || {
                    tracing::info!(value = ?Panics);
                });
            }));
            assert!(result.is_err());
            Log.info("after panic");
        });
        let seen = lines(&mut input);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].message, "after panic");
        assert_eq!(seen[0].timestamp, Timestamp(4));
    }

    /// The count survives a ring with no room for the warning and lands once
    /// there is room again.
    #[test]
    fn a_full_ring_drops_lines_and_reports_the_count_once() {
        let padded = "p".repeat(LogEvent::MAX_LEN / 2);
        let (_ring, mut port, mut input) = log_pair(4);
        let records = ring_capacity(LogEvent::MAX_LEN, 4).expect("valid")
            / frame_len(encoded_len(&padded, Timestamp(1)));
        let mut log = Log;
        with_log_port(&mut port, Timestamp(1), || {
            for _ in 0..records + 3 {
                log.info(padded.clone());
            }
        });
        let seen = lines(&mut input);
        assert_eq!(seen.len(), records);
        assert!(seen.iter().all(|l| l.level == LogLevel::Info));

        // The reader releases what it drained on its next call, freeing the ring.
        assert!(lines(&mut input).is_empty());
        with_log_port(&mut port, Timestamp(2), || {});
        let seen = lines(&mut input);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].level, LogLevel::Warn);
        assert_eq!(seen[0].fields, vec![("dropped".into(), "3".into())]);

        with_log_port(&mut port, Timestamp(3), || {});
        assert!(lines(&mut input).is_empty());
    }

    /// The encoded length of the line `Log::info(message)` writes.
    fn encoded_len(message: &str, now: Timestamp) -> usize {
        let event = LogEvent {
            timestamp: now,
            level: LogLevel::Info,
            source: Cow::Borrowed(""),
            target: Cow::Borrowed(""),
            message: message.to_string().into(),
            span: None,
            fields: Vec::new(),
            file: None,
            line: None,
        };
        let mut buf = vec![0u8; LogEvent::MAX_LEN];
        event.encode(&mut buf).expect("encodes").len()
    }
}
