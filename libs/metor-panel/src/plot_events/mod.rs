//! Event sources for the time-series plot's flag annotations.
//!
//! A plot draws vertical flags for events that fall in its visible time range —
//! log lines, alarm transitions, sequence lifecycle steps, and any other message
//! id an operator opts into. Rather than teach the plot about each event kind,
//! every kind is an [`EventSource`]: the plot asks a source for the events in a
//! range and gets back uniform [`PlotEvent`]s (time, color, one-line label, and a
//! typed detail for the popover).
//!
//! The three built-in sources read the existing app-global stores
//! ([`crate::logs`], [`crate::alarms`], [`crate::sequences`]) — no data is
//! duplicated, and they reuse those crates' domain types directly in
//! [`EventDetail`]. Any other message id resolves to a lazily-created
//! [`MsgEventStore`] that tails the raw record bytes and decodes them against the
//! schema the control system announces, yielding JSON (or a raw byte length when
//! no schema is known yet). The [`EventSourceRegistry`] global holds the built-ins
//! and hands out (or creates) a source for a requested [`EventKindKey`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{AnyEntity, App, Context, Entity, Global, Hsla, SharedString, Task, prelude::*};
use metor_db::DB;
use metor_proto::types::{Msg, PacketId, Timestamp};
use metor_proto_wkt::{
    AlarmAck, AlarmCleared, AlarmDef, AlarmRaised, LogEvent, LogLevel, MsgMetadata,
    SequenceChannelEvent, SequenceRegistry,
};

use crate::alarms::{self, AlarmEvent, AlarmEventKind, severity_index};
use crate::logs;
use crate::msg_ingest::{IngestSource, ingest_all};
use crate::sequences::{self, SequenceLogEntry, run_state_index};
use crate::theme::{Theme, theme};

#[cfg(test)]
mod tests;

/// Most flags a single source returns for one range. Dense stretches would swamp
/// the plot and the query; on overflow the newest events win.
const MAX_EVENTS: usize = 500;

/// Ring depth for an unknown message's decoded history. Matches the log store —
/// message channels can be chatty and deeper history stays queryable from the db.
const MSG_MAX_HISTORY: usize = 20_000;

/// Longest label message before it's elided with `…`.
const LABEL_MAX: usize = 80;

/// Identifies an event source. The three built-ins are singletons; every other
/// message id gets its own [`Msg`](EventKindKey::Msg) source.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum EventKindKey {
    Logs,
    Alarms,
    Sequences,
    Msg(PacketId),
}

/// Serialize an [`EventKindKey`] to the stable string used in saved layouts:
/// `"logs"`, `"alarms"`, `"sequences"`, or `"msg:e03c"` (lowercase hex id).
pub fn kind_key_to_string(key: EventKindKey) -> String {
    match key {
        EventKindKey::Logs => "logs".to_string(),
        EventKindKey::Alarms => "alarms".to_string(),
        EventKindKey::Sequences => "sequences".to_string(),
        EventKindKey::Msg(id) => format!("msg:{:02x}{:02x}", id[0], id[1]),
    }
}

/// Parse a [`kind_key_to_string`] string back to a key. `None` for an unknown
/// tag or a malformed message id, so stale layouts drop the overlay silently.
pub fn kind_key_from_string(s: &str) -> Option<EventKindKey> {
    match s {
        "logs" => Some(EventKindKey::Logs),
        "alarms" => Some(EventKindKey::Alarms),
        "sequences" => Some(EventKindKey::Sequences),
        _ => {
            let hex = s.strip_prefix("msg:")?;
            if hex.len() != 4 {
                return None;
            }
            let hi = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let lo = u8::from_str_radix(&hex[2..4], 16).ok()?;
            Some(EventKindKey::Msg([hi, lo]))
        }
    }
}

/// One annotation the plot can draw: when it happened, how to tint it, a one-line
/// header for the flag/popover, and the typed payload behind it.
#[derive(Clone, Debug)]
pub struct PlotEvent {
    /// Best-available event time: logs use the payload timestamp (the FSW clock
    /// the plot's traces are stamped with), alarms/sequences/unknown msgs use the
    /// db record timestamp. Under a simulated FSW clock the record stamp diverges
    /// (link arrival time); no correction is made here.
    pub ts: Timestamp,
    pub color: Hsla,
    /// One-line popover header, e.g. `"WARN nav: sun lost"`.
    pub label: SharedString,
    /// Compact name for the flag chip on the plot gutter, e.g. "WARN nav" or
    /// an alarm's def id; `label` stays the popover header.
    pub short: SharedString,
    pub detail: EventDetail,
}

/// The typed payload behind a [`PlotEvent`], reusing each store's own record type
/// so the popover can render the full event without a parallel mirror.
#[derive(Clone, Debug)]
pub enum EventDetail {
    Log(LogEvent),
    Alarm(AlarmEvent),
    Sequence(SequenceLogEntry),
    /// Schema-decoded unknown message.
    Json(Arc<serde_json::Value>),
    /// Payload byte length; the schema was absent or the decode failed.
    Raw(usize),
}

/// A queryable source of plot annotations. Sources are cheap handles read through
/// the [`App`]; the data lives in the app-global stores they front.
pub trait EventSource {
    fn key(&self) -> EventKindKey;
    /// Display name for pickers and the legend.
    fn name(&self, cx: &App) -> SharedString;
    /// Representative swatch color for pickers/legend, from the active theme.
    fn default_color(&self, cx: &App) -> Hsla;
    /// Monotonic change counter, for caching and repaint decisions.
    fn generation(&self, cx: &App) -> u64;
    /// Events with `ts` in `range`, ascending, capped at [`MAX_EVENTS`] (newest
    /// kept on overflow).
    fn events_in(&self, range: Range<Timestamp>, cx: &App) -> Vec<PlotEvent>;
    /// The entity to `cx.observe` for repaints, if any.
    fn observe_target(&self, cx: &App) -> Option<AnyEntity>;
}

/// Filter `(ts, payload)` items to those in `range`, sorted ascending, keeping the
/// newest [`MAX_EVENTS`] on overflow. Pure over the payload so it's testable
/// without a store, and used by the built-in sources whose histories aren't
/// strictly ts-ordered (log history is per-source, and merges are close but not
/// exact).
fn in_range<T>(
    items: impl Iterator<Item = (Timestamp, T)>,
    range: &Range<Timestamp>,
) -> Vec<(Timestamp, T)> {
    let mut kept: Vec<(Timestamp, T)> = items.filter(|(ts, _)| range.contains(ts)).collect();
    kept.sort_by_key(|(ts, _)| *ts);
    if kept.len() > MAX_EVENTS {
        kept.drain(..kept.len() - MAX_EVENTS);
    }
    kept
}

/// Elide `s` to `max` characters with a trailing `…`.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    } else {
        s.to_string()
    }
}

// -- Logs --------------------------------------------------------------------

/// Log lines from the global [`LogStore`](crate::logs::LogStore).
struct LogEventSource;

fn log_level_abbrev(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Trace => "TRACE",
        LogLevel::Debug => "DEBUG",
        LogLevel::Info => "INFO",
        LogLevel::Warn => "WARN",
        LogLevel::Error => "ERROR",
    }
}

/// Level color ramp, matching the log panel: error/warn/info track the alarm
/// severities, debug/trace fall to tertiary text.
fn log_level_color(level: LogLevel, theme: &Theme) -> Hsla {
    match level {
        LogLevel::Error => theme.alarm_color(2),
        LogLevel::Warn => theme.alarm_color(1),
        LogLevel::Info => theme.alarm_color(0),
        LogLevel::Debug | LogLevel::Trace => theme.text_tertiary,
    }
}

fn log_short(ev: &LogEvent) -> SharedString {
    let level = log_level_abbrev(ev.level);
    if ev.source.is_empty() {
        level.into()
    } else {
        format!("{level} {}", ev.source).into()
    }
}

fn log_label(ev: &LogEvent) -> SharedString {
    let level = log_level_abbrev(ev.level);
    let message = truncate(&ev.message, LABEL_MAX);
    if ev.source.is_empty() {
        format!("{level} {message}").into()
    } else {
        format!("{level} {}: {message}", ev.source).into()
    }
}

impl EventSource for LogEventSource {
    fn key(&self) -> EventKindKey {
        EventKindKey::Logs
    }

    fn name(&self, _cx: &App) -> SharedString {
        "Logs".into()
    }

    fn default_color(&self, cx: &App) -> Hsla {
        theme(cx).text_secondary
    }

    fn generation(&self, cx: &App) -> u64 {
        logs::try_global(cx).map_or(0, |s| s.read(cx).state().pushed())
    }

    fn events_in(&self, range: Range<Timestamp>, cx: &App) -> Vec<PlotEvent> {
        let Some(store) = logs::try_global(cx) else {
            return Vec::new();
        };
        let theme = theme(cx);
        let store = store.read(cx);
        let items = store
            .state()
            .history()
            .iter()
            .map(|rec| (rec.event.timestamp, &rec.event));
        in_range(items, &range)
            .into_iter()
            .map(|(ts, ev)| PlotEvent {
                ts,
                color: log_level_color(ev.level, &theme),
                label: log_label(ev),
                short: log_short(ev),
                detail: EventDetail::Log(ev.clone()),
            })
            .collect()
    }

    fn observe_target(&self, cx: &App) -> Option<AnyEntity> {
        logs::try_global(cx).map(Entity::into_any)
    }
}

// -- Alarms ------------------------------------------------------------------

/// Alarm transitions from the global [`AlarmStore`](crate::alarms::AlarmStore).
struct AlarmEventSource;

/// A raised alarm takes its severity color; cleared/acked transitions are calmer
/// secondary text.
fn alarm_color(ev: &AlarmEvent, theme: &Theme) -> Hsla {
    match ev.kind {
        AlarmEventKind::Raised => theme.alarm_color(ev.severity.map_or(0, severity_index)),
        AlarmEventKind::Cleared | AlarmEventKind::Acked => theme.text_secondary,
    }
}

fn alarm_short(ev: &AlarmEvent) -> SharedString {
    match ev.kind {
        AlarmEventKind::Raised => ev.def_id.clone().into(),
        AlarmEventKind::Cleared => format!("{} clr", ev.def_id).into(),
        AlarmEventKind::Acked => format!("{} ack", ev.def_id).into(),
    }
}

fn alarm_label(ev: &AlarmEvent) -> SharedString {
    let kind = match ev.kind {
        AlarmEventKind::Raised => "Raised",
        AlarmEventKind::Cleared => "Cleared",
        AlarmEventKind::Acked => "Acked",
    };
    if ev.detail.is_empty() {
        format!("{kind} {}", ev.def_id).into()
    } else {
        format!("{kind} {}: {}", ev.def_id, truncate(&ev.detail, LABEL_MAX)).into()
    }
}

impl EventSource for AlarmEventSource {
    fn key(&self) -> EventKindKey {
        EventKindKey::Alarms
    }

    fn name(&self, _cx: &App) -> SharedString {
        "Alarms".into()
    }

    fn default_color(&self, cx: &App) -> Hsla {
        theme(cx).alarm_color(2)
    }

    fn generation(&self, cx: &App) -> u64 {
        alarms::try_global(cx).map_or(0, |s| s.read(cx).state().history_pushed())
    }

    fn events_in(&self, range: Range<Timestamp>, cx: &App) -> Vec<PlotEvent> {
        let Some(store) = alarms::try_global(cx) else {
            return Vec::new();
        };
        let theme = theme(cx);
        let store = store.read(cx);
        let items = store.state().history().iter().map(|ev| (ev.timestamp, ev));
        in_range(items, &range)
            .into_iter()
            .map(|(ts, ev)| PlotEvent {
                ts,
                color: alarm_color(ev, &theme),
                label: alarm_label(ev),
                short: alarm_short(ev),
                detail: EventDetail::Alarm(ev.clone()),
            })
            .collect()
    }

    fn observe_target(&self, cx: &App) -> Option<AnyEntity> {
        alarms::try_global(cx).map(Entity::into_any)
    }
}

// -- Sequences ---------------------------------------------------------------

/// Sequence lifecycle steps from the global
/// [`SequenceStore`](crate::sequences::SequenceStore).
struct SequenceEventSource;

fn sequence_label(entry: &SequenceLogEntry) -> SharedString {
    format!("{}: {}", entry.channel_name, entry.label).into()
}

impl EventSource for SequenceEventSource {
    fn key(&self) -> EventKindKey {
        EventKindKey::Sequences
    }

    fn name(&self, _cx: &App) -> SharedString {
        "Sequences".into()
    }

    fn default_color(&self, cx: &App) -> Hsla {
        theme(cx).line_colors[4]
    }

    fn generation(&self, cx: &App) -> u64 {
        sequences::try_global(cx).map_or(0, |s| s.read(cx).state().history_pushed())
    }

    fn events_in(&self, range: Range<Timestamp>, cx: &App) -> Vec<PlotEvent> {
        let Some(store) = sequences::try_global(cx) else {
            return Vec::new();
        };
        let theme = theme(cx);
        let store = store.read(cx);
        let items = store.state().history().iter().map(|e| (e.timestamp, e));
        in_range(items, &range)
            .into_iter()
            .map(|(ts, entry)| PlotEvent {
                ts,
                // Each step takes the color of the run state it moved the channel to.
                color: theme.run_state_color(run_state_index(entry.run_state)),
                label: sequence_label(entry),
                // The step label alone ("Started", "Loaded x") chips well.
                short: entry.label.clone().into(),
                detail: EventDetail::Sequence(entry.clone()),
            })
            .collect()
    }

    fn observe_target(&self, cx: &App) -> Option<AnyEntity> {
        sequences::try_global(cx).map(Entity::into_any)
    }
}

// -- Unknown messages --------------------------------------------------------

/// One decoded record in a [`MsgEventStore`]'s ring.
struct MsgRecord {
    ts: Timestamp,
    detail: EventDetail,
}

/// Tails the raw records of one message id and decodes each against the schema
/// the control system announces, so any channel the built-ins don't claim can
/// still be flagged on the plot. The schema announce can trail the first record,
/// so the lookup is retried until it resolves; records ingested before then stay
/// [`Raw`](EventDetail::Raw).
pub struct MsgEventStore {
    id: PacketId,
    db: Arc<DB>,
    /// Announced schema + name, resolved lazily from the db.
    meta: Option<MsgMetadata>,
    history: VecDeque<MsgRecord>,
    /// Total records ever pushed; the generation stamp for observers.
    pushed: u64,
    _task: Task<()>,
}

/// Decode one record's bytes against its schema. Pure, so the JSON/raw split is
/// testable without a store or db.
fn decode_payload(meta: Option<&MsgMetadata>, bytes: &[u8]) -> EventDetail {
    match meta {
        Some(meta) => match postcard_dyn::from_slice_dyn(&meta.schema, bytes) {
            Ok(value) => EventDetail::Json(Arc::new(value)),
            Err(_) => EventDetail::Raw(bytes.len()),
        },
        None => EventDetail::Raw(bytes.len()),
    }
}

impl MsgEventStore {
    fn new(id: PacketId, db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let meta = lookup_meta(&db, id);
        let task = cx.spawn({
            let db = db.clone();
            async move |this, cx| {
                let sources = vec![IngestSource::new_raw(id, |store: &mut Self, ts, bytes| {
                    store.ingest(ts, bytes)
                })];
                ingest_all(db, sources, this, cx).await
            }
        });
        Self {
            id,
            db,
            meta,
            history: VecDeque::new(),
            pushed: 0,
            _task: task,
        }
    }

    fn ingest(&mut self, ts: Timestamp, bytes: &[u8]) {
        if self.meta.is_none() {
            self.meta = lookup_meta(&self.db, self.id);
        }
        let detail = decode_payload(self.meta.as_ref(), bytes);
        self.history.push_back(MsgRecord { ts, detail });
        self.pushed += 1;
        while self.history.len() > MSG_MAX_HISTORY {
            self.history.pop_front();
        }
    }

    fn name(&self) -> SharedString {
        match &self.meta {
            Some(meta) if !meta.name.is_empty() => meta.name.clone().into(),
            _ => format!("msg {:02x}:{:02x}", self.id[0], self.id[1]).into(),
        }
    }

    /// Records in `range`, ascending, capped at [`MAX_EVENTS`]. The ring is
    /// ingest-ordered (backfill sorts by ts, the live tail appends WAL order), so
    /// the bounds come from a binary search rather than a scan.
    fn records_in(&self, range: Range<Timestamp>) -> Vec<(Timestamp, EventDetail)> {
        let lo = self.history.partition_point(|r| r.ts < range.start);
        let hi = self.history.partition_point(|r| r.ts < range.end);
        let start = if hi.saturating_sub(lo) > MAX_EVENTS {
            hi - MAX_EVENTS
        } else {
            lo
        };
        (start..hi)
            .map(|i| {
                let r = &self.history[i];
                (r.ts, r.detail.clone())
            })
            .collect()
    }
}

/// The announced metadata for `id`, if the db has seen its schema announce.
fn lookup_meta(db: &DB, id: PacketId) -> Option<MsgMetadata> {
    db.with_state(|s| {
        s.msg_log_iter()
            .find(|(mid, _)| *mid == id)
            .and_then(|(_, meta)| meta.cloned())
    })
}

/// A source fronting one [`MsgEventStore`]. All of an id's events share the id's
/// stable palette color.
struct MsgEventSource {
    id: PacketId,
    entity: Entity<MsgEventStore>,
}

impl EventSource for MsgEventSource {
    fn key(&self) -> EventKindKey {
        EventKindKey::Msg(self.id)
    }

    fn name(&self, cx: &App) -> SharedString {
        self.entity.read(cx).name()
    }

    fn default_color(&self, cx: &App) -> Hsla {
        let colors = theme(cx).line_colors;
        colors[u16::from_le_bytes(self.id) as usize % colors.len()]
    }

    fn generation(&self, cx: &App) -> u64 {
        self.entity.read(cx).pushed
    }

    fn events_in(&self, range: Range<Timestamp>, cx: &App) -> Vec<PlotEvent> {
        let color = self.default_color(cx);
        let store = self.entity.read(cx);
        let name = store.name();
        store
            .records_in(range)
            .into_iter()
            .map(|(ts, detail)| {
                let label = match &detail {
                    EventDetail::Raw(len) => format!("{name} · {len} B").into(),
                    _ => name.clone(),
                };
                PlotEvent {
                    ts,
                    color,
                    label,
                    short: name.clone(),
                    detail,
                }
            })
            .collect()
    }

    fn observe_target(&self, _cx: &App) -> Option<AnyEntity> {
        Some(self.entity.clone().into_any())
    }
}

// -- Registry ----------------------------------------------------------------

/// App-global registry of event sources. Holds the built-in singletons and the
/// set of message ids they claim, and creates (then caches) a [`MsgEventStore`]
/// the first time an unclaimed id is requested.
pub struct EventSourceRegistry {
    builtins: Vec<Rc<dyn EventSource>>,
    claimed: HashSet<PacketId>,
    msg_stores: HashMap<PacketId, Entity<MsgEventStore>>,
}

impl Global for EventSourceRegistry {}

/// The registry, or `None` if it was never initialized (e.g. in tests).
pub fn try_global(cx: &App) -> Option<&EventSourceRegistry> {
    cx.try_global::<EventSourceRegistry>()
}

impl EventSourceRegistry {
    pub fn init(cx: &mut App) {
        let builtins: Vec<Rc<dyn EventSource>> = vec![
            Rc::new(LogEventSource),
            Rc::new(AlarmEventSource),
            Rc::new(SequenceEventSource),
        ];
        // The message ids the built-ins already cover, so a msg picker can hide
        // the ids that would only duplicate a built-in source.
        let claimed = HashSet::from([
            LogEvent::ID,
            AlarmDef::ID,
            AlarmRaised::ID,
            AlarmCleared::ID,
            AlarmAck::ID,
            SequenceRegistry::ID,
            SequenceChannelEvent::ID,
        ]);
        cx.set_global(EventSourceRegistry {
            builtins,
            claimed,
            msg_stores: HashMap::new(),
        });
    }

    /// The built-in sources, in display order.
    pub fn builtins(&self) -> &[Rc<dyn EventSource>] {
        &self.builtins
    }

    /// Message ids already covered by a built-in source.
    pub fn claimed(&self) -> &HashSet<PacketId> {
        &self.claimed
    }

    /// The source for `key`, creating and caching a [`MsgEventStore`] on first
    /// request of an unknown message id. `None` only if the registry is absent.
    pub fn source_for(
        key: EventKindKey,
        db: &Arc<DB>,
        cx: &mut App,
    ) -> Option<Rc<dyn EventSource>> {
        match key {
            EventKindKey::Logs | EventKindKey::Alarms | EventKindKey::Sequences => cx
                .try_global::<EventSourceRegistry>()?
                .builtins
                .iter()
                .find(|s| s.key() == key)
                .cloned(),
            EventKindKey::Msg(id) => {
                let existing = cx
                    .try_global::<EventSourceRegistry>()?
                    .msg_stores
                    .get(&id)
                    .cloned();
                let entity = match existing {
                    Some(entity) => entity,
                    None => {
                        let db = db.clone();
                        let entity = cx.new(|cx| MsgEventStore::new(id, db, cx));
                        cx.update_global::<EventSourceRegistry, ()>(|reg, _| {
                            reg.msg_stores.insert(id, entity.clone());
                        });
                        entity
                    }
                };
                Some(Rc::new(MsgEventSource { id, entity }))
            }
        }
    }
}
