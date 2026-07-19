//! Ingests the flight software's [`LogEvent`] broadcast into an app-global
//! store the log viewer observes.
//!
//! One message type, one source: every log line — a system's health-port log
//! call or a host tracing event routed through the FSW's forwarding layer —
//! arrives as a [`LogEvent`] record on the persisted message log, so the
//! store is the alarm/sequence ingest shape with a single source and a much
//! deeper history ring.

use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;

use gpui::{App, Context, Entity, Global, Task, prelude::*};
use metor_db::DB;
use metor_proto::types::Msg;
use metor_proto_wkt::{LogEvent, LogLevel};

use crate::msg_ingest::{IngestSource, ingest_all};

/// How many lines the in-memory ring keeps for the viewer. Logs are much
/// chattier than alarms; deeper history stays queryable from the persisted
/// message log.
const MAX_HISTORY: usize = 20_000;

/// One received log line: the event plus a monotonic sequence number that
/// stays valid as the ring evicts, so a filtered view can index stably.
pub struct LogRecord {
    pub seq: u64,
    pub event: LogEvent,
}

/// The folded log state: the bounded history ring and the set of source names
/// seen, which feeds the viewer's source filter.
#[derive(Default)]
pub struct LogState {
    history: VecDeque<LogRecord>,
    sources: BTreeSet<String>,
    /// Total lines ever pushed; `seq` of the next record. The ring's front
    /// record has `seq == pushed - history.len()`.
    pushed: u64,
}

impl LogState {
    fn push(&mut self, event: LogEvent) {
        if !event.source.is_empty() && !self.sources.contains(&event.source) {
            self.sources.insert(event.source.clone());
        }
        self.history.push_back(LogRecord {
            seq: self.pushed,
            event,
        });
        self.pushed += 1;
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    pub fn history(&self) -> &VecDeque<LogRecord> {
        &self.history
    }

    /// Total lines ever pushed; a cheap staleness stamp for filtered indexes.
    pub fn pushed(&self) -> u64 {
        self.pushed
    }

    /// The record with sequence number `seq`, if the ring still holds it.
    pub fn by_seq(&self, seq: u64) -> Option<&LogRecord> {
        let front = self.pushed - self.history.len() as u64;
        self.history.get(usize::try_from(seq.checked_sub(front)?).ok()?)
    }

    /// Every source name seen, sorted; feeds the viewer's source filter.
    pub fn sources(&self) -> impl Iterator<Item = &str> {
        self.sources.iter().map(String::as_str)
    }

    /// `[trace, debug, info, warn, error]` counts over the retained history.
    pub fn counts_by_level(&self) -> [usize; 5] {
        let mut counts = [0usize; 5];
        for rec in &self.history {
            counts[rec.event.level as usize] += 1;
        }
        counts
    }
}

/// Index of a [`LogLevel`] into level-ordered arrays and the severity color
/// ramp: trace/debug map below info.
pub fn level_index(level: LogLevel) -> usize {
    level as usize
}

/// The gpui entity wrapping [`LogState`]; owns the in-process ingestion task.
pub struct LogStore {
    state: LogState,
    _task: Task<()>,
}

/// Hands the shared [`LogStore`] entity to any part of the app.
pub struct GlobalLogStore(pub Entity<LogStore>);

impl Global for GlobalLogStore {}

/// The shared log store, or `None` if it was never initialized (e.g. in tests).
pub fn try_global(cx: &App) -> Option<Entity<LogStore>> {
    cx.try_global::<GlobalLogStore>().map(|g| g.0.clone())
}

impl LogStore {
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let entity = cx.new(|cx| LogStore::new(db, cx));
        cx.set_global(GlobalLogStore(entity));
    }

    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let task = cx.spawn(async move |this, cx| {
            // Display uses the event's payload timestamp (the FSW-side clock);
            // the merge timestamp only orders the backfill.
            let sources = vec![IngestSource::new(
                LogEvent::ID,
                |store: &mut Self, _ts, event: LogEvent| store.state.push(event),
            )];
            ingest_all(db, sources, this, cx).await
        });

        Self {
            state: LogState::default(),
            _task: task,
        }
    }

    pub fn state(&self) -> &LogState {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_proto::types::Timestamp;

    fn event(source: &str, level: LogLevel, message: &str) -> LogEvent {
        LogEvent {
            timestamp: Timestamp(1),
            level,
            source: source.to_string(),
            target: String::new(),
            message: message.to_string(),
            span: None,
            fields: Vec::new(),
            file: None,
            line: None,
        }
    }

    /// Eviction keeps seq addressing stable and the source set grows once per
    /// distinct name.
    #[test]
    fn ring_evicts_and_seq_stays_stable() {
        let mut state = LogState::default();
        for i in 0..(MAX_HISTORY + 10) {
            let source = if i % 2 == 0 { "nav" } else { "ctrl" };
            state.push(event(source, LogLevel::Info, &format!("line {i}")));
        }
        assert_eq!(state.history().len(), MAX_HISTORY);
        assert_eq!(state.pushed(), (MAX_HISTORY + 10) as u64);
        // The first surviving record is the 10th pushed; older seqs are gone.
        assert!(state.by_seq(9).is_none());
        assert_eq!(state.by_seq(10).unwrap().event.message, "line 10");
        let last = state.pushed() - 1;
        assert_eq!(
            state.by_seq(last).unwrap().event.message,
            format!("line {}", MAX_HISTORY + 9)
        );
        assert_eq!(state.sources().collect::<Vec<_>>(), vec!["ctrl", "nav"]);
        let counts = state.counts_by_level();
        assert_eq!(counts[LogLevel::Info as usize], MAX_HISTORY);
    }
}
