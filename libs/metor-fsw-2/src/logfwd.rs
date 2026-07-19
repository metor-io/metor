//! Forwards host `tracing` events onto the downlink as [`LogEvent`]s.
//!
//! [`ForwardLayer`] is a `tracing-subscriber` layer that converts each event
//! into a [`LogEvent`] — level, target, message, non-message fields, the
//! active span path, and source location — and pushes it onto a bounded
//! global queue. The coordinator drains the queue once per cycle onto its own
//! log port ([`HealthPort::emit_event`](crate::health::HealthPort)), so
//! tracing events ride the same message stream as every system's
//! [`HealthPort::log`](crate::health::HealthPort::log) lines and reach the
//! ground with no extra downlink plumbing.
//!
//! The queue seam exists because `on_event` can fire from any thread (the
//! build pipeline, the async sender tasks) while ring writes belong to the
//! single-threaded cycle loop. It is bounded and drop-newest: a runaway log
//! source costs events, counted and folded into coordinator health as
//! `log_dropped`, never memory or cycle time. Events fired before the mission
//! starts (build, init) simply sit in the queue and flush on the first cycle.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use metor_proto::types::Timestamp;
use metor_proto_wkt::{LogEvent, LogLevel};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Queue depth; events past it are dropped-newest and counted.
const QUEUE_CAP: usize = 1024;

static QUEUE: Mutex<VecDeque<LogEvent>> = Mutex::new(VecDeque::new());
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Convert a `tracing::Level` to the wire enum.
fn level_of(level: &tracing::Level) -> LogLevel {
    match *level {
        tracing::Level::TRACE => LogLevel::Trace,
        tracing::Level::DEBUG => LogLevel::Debug,
        tracing::Level::INFO => LogLevel::Info,
        tracing::Level::WARN => LogLevel::Warn,
        tracing::Level::ERROR => LogLevel::Error,
    }
}

/// Collects an event's `message` and remaining fields as display strings.
#[derive(Default)]
struct FieldVisitor {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for FieldVisitor {
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

/// The forwarding layer. Compose it onto a `tracing_subscriber::registry()`
/// stack; [`forward_layer`] builds one for embedders.
pub struct ForwardLayer {
    _priv: (),
}

/// A [`ForwardLayer`] for a host's subscriber stack. Filter it externally
/// (`with_filter`) to choose what reaches the downlink; the CLI forwards
/// `INFO` and up by default.
pub fn forward_layer() -> ForwardLayer {
    ForwardLayer { _priv: () }
}

impl<S> Layer<S> for ForwardLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        // Root → leaf, matching the textual convention `run:load_slot`.
        let span = ctx.event_scope(event).map(|scope| {
            let names: Vec<&str> = scope.from_root().map(|s| s.name()).collect();
            names.join(":")
        });
        let ev = LogEvent {
            timestamp: Timestamp::now(),
            level: level_of(meta.level()),
            source: meta.target().to_string(),
            target: meta.target().to_string(),
            message: visitor.message,
            span,
            fields: visitor.fields,
            file: meta.file().map(str::to_string),
            line: meta.line(),
        };
        let mut q = QUEUE.lock().expect("log queue lock is never poisoned");
        if q.len() < QUEUE_CAP {
            q.push_back(ev);
        } else {
            DROPPED.fetch_add(1, Relaxed);
        }
    }
}

/// Drain every queued event into `f` and return the drop count accumulated
/// since the last call. The coordinator's per-cycle (and shutdown) hook.
pub(crate) fn drain(mut f: impl FnMut(LogEvent)) -> u64 {
    let mut q = QUEUE.lock().expect("log queue lock is never poisoned");
    while let Some(ev) = q.pop_front() {
        f(ev);
    }
    DROPPED.swap(0, Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    /// One event inside two nested spans lands in the queue with its level,
    /// target, message, joined span path, non-message fields, and location.
    /// Serial with any other queue test via the global-queue drain.
    #[test]
    fn event_converts_and_drains() {
        let subscriber = tracing_subscriber::registry().with(forward_layer());
        let _ = drain(|_| {});
        tracing::subscriber::with_default(subscriber, || {
            let outer = tracing::info_span!("run");
            let _outer = outer.enter();
            let inner = tracing::info_span!("load_slot");
            let _inner = inner.enter();
            tracing::warn!(slot = "nav", attempts = 3, "occupant failed");
        });
        let mut events = Vec::new();
        let dropped = drain(|ev| events.push(ev));
        assert_eq!(dropped, 0);
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.level, LogLevel::Warn);
        assert_eq!(ev.message, "occupant failed");
        assert_eq!(ev.span.as_deref(), Some("run:load_slot"));
        assert_eq!(ev.source, module_path!());
        assert!(ev.fields.contains(&("slot".into(), "nav".into())));
        assert!(ev.fields.contains(&("attempts".into(), "3".into())));
        assert_eq!(ev.file.as_deref(), Some(file!()));
        assert!(ev.line.is_some());
    }
}
