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
//! The queue exists because `on_event` can fire from any thread, such as a
//! build worker or another tracing thread, while ring writes belong to the
//! single-threaded cycle loop. It is bounded and drop-newest: a runaway log
//! source costs events, counted and folded into coordinator health as
//! `log_dropped`, never memory or cycle time. Events fired before the mission
//! starts (build, init) simply sit in the queue and flush on the first cycle.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use metor_proto_wkt::{LogEvent, LogLevel};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// Queue depth; events past it are dropped-newest and counted.
const QUEUE_CAP: usize = 1024;

static QUEUE: Mutex<VecDeque<LogEvent>> = Mutex::new(VecDeque::new());
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Set inside a pack dylib by [`init_pack_tracing`]. These statics are
/// per-dylib (each pack links its own copy of this crate), so the flag is
/// only ever true in a shared object — the host keeps the coordinator as the
/// single drain and this flag false.
static PACK_MODE: AtomicBool = AtomicBool::new(false);

/// Whether this linkage unit is a pack dylib draining its own queue.
pub(crate) fn pack_mode() -> bool {
    PACK_MODE.load(Relaxed)
}

/// Install the pack-side tracing pipeline: a per-dylib global subscriber
/// whose only layer is a [`ForwardLayer`] capped at `INFO`, plus the flag
/// that makes every instance's `end_cycle` drain the dylib's queue onto its
/// own log port.
///
/// Called by the `export_pack!` shim from `fsw_pack_open`, once per load. A
/// dylib links its own copy of `tracing`'s dispatcher, so without this the
/// macros silently no-op inside a pack. Attribution is exact for events fired
/// during `execute` (the loop is single-threaded and the dylib runs no other
/// code); events fired during create/bind drain into the first instance that
/// completes a cycle.
pub fn init_pack_tracing() {
    PACK_MODE.store(true, Relaxed);
    use tracing_subscriber::layer::SubscriberExt;
    let subscriber = tracing_subscriber::registry()
        .with(ForwardLayer::new().with_max_level(tracing::Level::INFO));
    let _ = tracing::subscriber::set_global_default(subscriber);
}

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
    /// Maximum verbosity forwarded. `TRACE` allows every level through to an
    /// external `with_filter`.
    max_level: tracing::Level,
}

impl ForwardLayer {
    fn new() -> Self {
        Self {
            max_level: tracing::Level::TRACE,
        }
    }

    /// Cap the forwarded verbosity, for stacks with no external filter.
    pub fn with_max_level(mut self, max_level: tracing::Level) -> Self {
        self.max_level = max_level;
        self
    }
}

/// A [`ForwardLayer`] for a host's subscriber stack. Filter it externally
/// (`with_filter`) to choose what reaches the downlink; the CLI forwards
/// `INFO` and up by default.
pub fn forward_layer() -> ForwardLayer {
    ForwardLayer::new()
}

impl<S> Layer<S> for ForwardLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn enabled(
        &self,
        metadata: &tracing::Metadata<'_>,
        _ctx: Context<'_, S>,
    ) -> bool {
        *metadata.level() <= self.max_level
    }

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
            // Mission time at the instant the event fired; wall only before
            // the first cycle (build/init), which coincides with the epoch.
            timestamp: crate::clock::mission_now_or_wall(),
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
