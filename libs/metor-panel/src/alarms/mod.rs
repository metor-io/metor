//! Ingests the control system's alarm broadcast (declaration, raised, cleared, and
//! operator-ack messages) into a single app-global store that views observe.
//!
//! The panel never decides whether an alarm fires — the control system does that and
//! reports it via [`AlarmRaised`]/[`AlarmCleared`]. The limits carried on an
//! [`AlarmDef`] are display hints only. What the panel *publishes* is the operator's
//! side of the conversation: [`AlarmAck`], and the shelving pair
//! [`AlarmShelved`]/[`AlarmUnshelved`]. All three travel the same message path, so every
//! connected client converges on one answer; the control system consumes only the ack.
//!
//! Two semantics live here rather than in any view. A **latch** keeps an occurrence that
//! cleared before anyone acknowledged it, so a transient alarm is still there to be
//! dismissed. A **shelf** is ISA-18.2's time-limited suppression of a known-noisy point:
//! the alarm keeps firing and keeps its history, it just leaves the pending list until
//! the shelf expires.
//!
//! State folding lives in [`AlarmState`] (pure, unit-tested). [`AlarmStore`] is the gpui
//! entity that owns it plus the in-process reader tasks; [`GlobalAlarmStore`] hands the
//! entity to any view via [`try_global`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Context, Entity, Global, Task, prelude::*};
use metor_db::DB;
use metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_proto_wkt::{
    AlarmAck, AlarmCleared, AlarmDef, AlarmDefs, AlarmId, AlarmLimit, AlarmRaised, AlarmShelved,
    AlarmUnshelved, OccurrenceId, Severity,
};

use crate::msg_ingest::{IngestSource, ingest_all};
use latch::TileState;

pub mod latch;

#[cfg(test)]
mod tests;

/// How many past events the in-memory history ring keeps for the alarm panel. Deeper
/// history can be queried from the persisted message log on demand.
const MAX_HISTORY: usize = 1000;

/// Longest a shelf may run. ISA-18.2 has no "shelve forever": a point that needs
/// permanent suppression needs a configuration change, not an operator gesture.
pub const MAX_SHELF_DURATION: Duration = Duration::from_secs(8 * 60 * 60);

/// A currently-raised alarm occurrence (raised and not yet cleared).
#[derive(Clone, Debug)]
pub struct ActiveAlarm {
    pub def_id: AlarmId,
    pub occurrence: OccurrenceId,
    pub severity: Severity,
    pub value: Option<f64>,
    pub message: String,
    pub raised_at: Timestamp,
}

/// An occurrence that cleared while still unacknowledged.
///
/// Kept keyed by [`AlarmId`] rather than occurrence, so a chattering point replaces its
/// own entry instead of growing the map: there is one thing to dismiss per alarm point.
#[derive(Clone, Debug)]
pub struct LatchedAlarm {
    pub alarm: ActiveAlarm,
    pub cleared_at: Timestamp,
}

/// An operator's time-limited suppression of one alarm point.
///
/// `shelved_at` and `until` are **wall-clock** stamps ([`Timestamp::now`]): a shelf is a
/// console gesture that expires on the operator's clock, and is never compared against
/// telemetry or simulation time.
#[derive(Clone, Debug)]
pub struct Shelf {
    pub until: Timestamp,
    pub reason: Option<String>,
    pub operator: String,
    pub shelved_at: Timestamp,
    /// Severity the point was at when shelved. A raise above it defeats the shelf, so
    /// suppressing a noisy warning can never hide the critical it turns into.
    pub severity_at_shelve: Option<Severity>,
}

/// One row of the pending list: a live occurrence or a latched one, tagged with the
/// [`TileState`] that decides how it renders.
#[derive(Clone, Debug)]
pub struct PendingAlarm {
    pub alarm: ActiveAlarm,
    pub state: TileState,
    pub cleared_at: Option<Timestamp>,
}

/// The rollup of one alarm definition, as an annunciator tile sees it. A shelved or
/// never-fired def reads [`TileState::Normal`] with everything else absent.
#[derive(Clone, Debug, Default)]
pub struct AlarmPoint {
    pub state: TileState,
    pub since: Option<Timestamp>,
    pub severity: Option<Severity>,
    pub value: Option<f64>,
    pub occurrence: Option<OccurrenceId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlarmEventKind {
    Raised,
    Cleared,
    Acked,
    Shelved,
    Unshelved,
}

/// One entry in the alarm history log shown by the panel.
#[derive(Clone, Debug)]
pub struct AlarmEvent {
    pub kind: AlarmEventKind,
    pub def_id: AlarmId,
    pub occurrence: OccurrenceId,
    pub timestamp: Timestamp,
    pub severity: Option<Severity>,
    pub detail: String,
}

/// The folded alarm state. Kept free of gpui/DB so the reconciliation rules can be
/// tested directly.
#[derive(Default)]
pub struct AlarmState {
    defs: HashMap<AlarmId, AlarmDef>,
    active: HashMap<OccurrenceId, ActiveAlarm>,
    acked: HashSet<OccurrenceId>,
    latched: HashMap<AlarmId, LatchedAlarm>,
    shelves: HashMap<AlarmId, Shelf>,
    history: VecDeque<AlarmEvent>,
    /// Total events ever pushed; a monotonic staleness stamp for observers
    /// (the plot's event overlay) that stays valid as the ring evicts.
    history_pushed: u64,
}

impl AlarmState {
    pub fn apply_def(&mut self, def: AlarmDef) {
        self.defs.insert(def.id.clone(), def);
    }

    /// Fold a raise. Two rules beyond the obvious insert: an escalation reuses the
    /// occurrence id, so an acknowledgment of the milder band no longer stands; and a
    /// raise above the severity a shelf was taken at defeats the shelf outright.
    pub fn apply_raised(&mut self, timestamp: Timestamp, raised: AlarmRaised) {
        if self
            .active
            .get(&raised.occurrence)
            .is_some_and(|prev| raised.severity > prev.severity)
        {
            self.acked.remove(&raised.occurrence);
        }
        if self.shelves.get(&raised.def_id).is_some_and(|shelf| {
            shelf
                .severity_at_shelve
                .is_none_or(|at| raised.severity > at)
        }) {
            self.shelves.remove(&raised.def_id);
            self.push_event(AlarmEvent {
                kind: AlarmEventKind::Unshelved,
                def_id: raised.def_id.clone(),
                occurrence: raised.occurrence,
                timestamp,
                severity: Some(raised.severity),
                detail: "escalated".into(),
            });
        }
        self.latched.remove(&raised.def_id);
        self.active.insert(
            raised.occurrence,
            ActiveAlarm {
                def_id: raised.def_id.clone(),
                occurrence: raised.occurrence,
                severity: raised.severity,
                value: raised.value,
                message: raised.message.clone(),
                raised_at: timestamp,
            },
        );
        self.push_event(AlarmEvent {
            kind: AlarmEventKind::Raised,
            def_id: raised.def_id,
            occurrence: raised.occurrence,
            timestamp,
            severity: Some(raised.severity),
            detail: raised.message,
        });
    }

    /// Fold a clear. An occurrence the operator already dismissed is dropped; one that
    /// cleared unacknowledged latches instead, so it survives to be dismissed.
    pub fn apply_cleared(&mut self, timestamp: Timestamp, cleared: AlarmCleared) {
        // Ignore clears for occurrences we never saw raised — they carry no state.
        let Some(alarm) = self.active.remove(&cleared.occurrence) else {
            return;
        };
        if !self.acked.remove(&cleared.occurrence) {
            self.latched.insert(
                cleared.def_id.clone(),
                LatchedAlarm {
                    alarm,
                    cleared_at: timestamp,
                },
            );
        }
        self.push_event(AlarmEvent {
            kind: AlarmEventKind::Cleared,
            def_id: cleared.def_id,
            occurrence: cleared.occurrence,
            timestamp,
            severity: None,
            detail: String::new(),
        });
    }

    /// Fold an acknowledgment. Acking a latched occurrence retires the latch — nothing
    /// else does.
    pub fn apply_ack(&mut self, timestamp: Timestamp, ack: AlarmAck) {
        let latched = self
            .latched
            .get(&ack.def_id)
            .is_some_and(|entry| entry.alarm.occurrence == ack.occurrence);
        if latched {
            self.latched.remove(&ack.def_id);
        } else if self.active.contains_key(&ack.occurrence) {
            self.acked.insert(ack.occurrence);
        } else {
            return;
        }
        self.push_event(AlarmEvent {
            kind: AlarmEventKind::Acked,
            def_id: ack.def_id,
            occurrence: ack.occurrence,
            timestamp,
            severity: None,
            detail: ack.operator,
        });
    }

    pub fn apply_shelved(&mut self, timestamp: Timestamp, shelved: AlarmShelved) {
        let point = self.point(&shelved.def_id);
        let detail = match &shelved.reason {
            Some(reason) => format!("{} — {reason}", shelved.operator),
            None => shelved.operator.clone(),
        };
        self.shelves.insert(
            shelved.def_id.clone(),
            Shelf {
                until: shelved.until,
                reason: shelved.reason,
                operator: shelved.operator,
                shelved_at: timestamp,
                severity_at_shelve: point.severity,
            },
        );
        self.push_event(AlarmEvent {
            kind: AlarmEventKind::Shelved,
            def_id: shelved.def_id,
            occurrence: point.occurrence.unwrap_or_default(),
            timestamp,
            severity: point.severity,
            detail,
        });
    }

    pub fn apply_unshelved(&mut self, timestamp: Timestamp, unshelved: AlarmUnshelved) {
        if self.shelves.remove(&unshelved.def_id).is_none() {
            return;
        }
        let point = self.point(&unshelved.def_id);
        self.push_event(AlarmEvent {
            kind: AlarmEventKind::Unshelved,
            def_id: unshelved.def_id,
            occurrence: point.occurrence.unwrap_or_default(),
            timestamp,
            severity: None,
            detail: unshelved.operator,
        });
    }

    fn push_event(&mut self, event: AlarmEvent) {
        self.history.push_back(event);
        self.history_pushed += 1;
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    /// Total events ever pushed; a cheap change stamp for the plot's event overlay.
    pub fn history_pushed(&self) -> u64 {
        self.history_pushed
    }

    /// Active alarms, highest severity first then most recently raised. Live-only:
    /// callers that mean "firing now" (plot tinting) read this, not [`Self::pending_sorted`].
    pub fn active_sorted(&self) -> Vec<ActiveAlarm> {
        let mut alarms: Vec<ActiveAlarm> = self.active.values().cloned().collect();
        alarms.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(b.raised_at.0.cmp(&a.raised_at.0))
        });
        alarms
    }

    /// Everything awaiting an operator — live occurrences plus latched ones — with
    /// shelved points left out. Highest severity first; at equal severity live rows
    /// precede latched ones, then most recently raised.
    pub fn pending_sorted(&self) -> Vec<PendingAlarm> {
        let now = Timestamp::now();
        let live = self
            .active
            .values()
            .filter(|alarm| !self.is_shelved_at(&alarm.def_id, now))
            .map(|alarm| PendingAlarm {
                state: match self.acked.contains(&alarm.occurrence) {
                    true => TileState::AlarmAcked,
                    false => TileState::AlarmUnacked,
                },
                alarm: alarm.clone(),
                cleared_at: None,
            });
        let latched = self
            .latched
            .values()
            .filter(|entry| !self.is_shelved_at(&entry.alarm.def_id, now))
            .map(|entry| PendingAlarm {
                alarm: entry.alarm.clone(),
                state: TileState::ClearedUnacked,
                cleared_at: Some(entry.cleared_at),
            });

        let mut rows: Vec<PendingAlarm> = live.chain(latched).collect();
        rows.sort_by(|a, b| {
            b.alarm
                .severity
                .cmp(&a.alarm.severity)
                .then(a.cleared_at.is_some().cmp(&b.cleared_at.is_some()))
                .then(b.alarm.raised_at.0.cmp(&a.alarm.raised_at.0))
        });
        rows
    }

    /// The rollup one alarm definition presents to an annunciator tile: its worst live
    /// occurrence, else its latch, else nothing. A shelved def reads
    /// [`TileState::Normal`] — that is what shelving means everywhere but the shelf list.
    pub fn point(&self, def_id: &AlarmId) -> AlarmPoint {
        if self.is_shelved_at(def_id, Timestamp::now()) {
            return AlarmPoint::default();
        }
        if let Some(alarm) = self
            .active
            .values()
            .filter(|alarm| &alarm.def_id == def_id)
            .max_by_key(|alarm| (alarm.severity, alarm.raised_at.0))
        {
            return AlarmPoint {
                state: match self.acked.contains(&alarm.occurrence) {
                    true => TileState::AlarmAcked,
                    false => TileState::AlarmUnacked,
                },
                since: Some(alarm.raised_at),
                severity: Some(alarm.severity),
                value: alarm.value,
                occurrence: Some(alarm.occurrence),
            };
        }
        match self.latched.get(def_id) {
            Some(entry) => AlarmPoint {
                state: TileState::ClearedUnacked,
                since: Some(entry.alarm.raised_at),
                severity: Some(entry.alarm.severity),
                value: entry.alarm.value,
                occurrence: Some(entry.alarm.occurrence),
            },
            None => AlarmPoint::default(),
        }
    }

    /// Every declared alarm, for surfaces that match a glob against [`AlarmDef::name`].
    pub fn defs_iter(&self) -> impl Iterator<Item = &AlarmDef> {
        self.defs.values()
    }

    /// The definition an occurrence belongs to, live or latched.
    pub fn def_of(&self, occurrence: OccurrenceId) -> Option<&AlarmId> {
        if let Some(alarm) = self.active.get(&occurrence) {
            return Some(&alarm.def_id);
        }
        self.latched
            .values()
            .find(|entry| entry.alarm.occurrence == occurrence)
            .map(|entry| &entry.alarm.def_id)
    }

    /// Live shelves, soonest expiry first.
    ///
    /// Expiry is evaluated here rather than by a removal task: a query that lies for up
    /// to a tick is worse than a map holding an entry nobody reads.
    pub fn shelves_sorted(&self) -> Vec<(&AlarmId, &Shelf)> {
        let now = Timestamp::now();
        let mut shelves: Vec<(&AlarmId, &Shelf)> = self
            .shelves
            .iter()
            .filter(|(_, shelf)| shelf.until > now)
            .collect();
        shelves.sort_by_key(|(_, shelf)| shelf.until.0);
        shelves
    }

    fn is_shelved_at(&self, def_id: &AlarmId, now: Timestamp) -> bool {
        self.shelves
            .get(def_id)
            .is_some_and(|shelf| shelf.until > now)
    }

    pub fn is_acked(&self, occurrence: OccurrenceId) -> bool {
        self.acked.contains(&occurrence)
    }

    pub fn def(&self, id: &AlarmId) -> Option<&AlarmDef> {
        self.defs.get(id)
    }

    /// Display limit lines declared for the trace `(component_id, element)`. A def with
    /// no element index applies to every element of its component.
    pub fn limits_for(&self, component_id: ComponentId, element: usize) -> Vec<AlarmLimit> {
        self.defs
            .values()
            .filter(|def| Self::targets(def, component_id, element))
            .flat_map(|def| def.limits.clone())
            .collect()
    }

    /// Highest severity among *active* alarms whose def targets the trace
    /// `(component_id, element)`, if any. Drives plot out-of-bounds tinting.
    pub fn active_severity_for(
        &self,
        component_id: ComponentId,
        element: usize,
    ) -> Option<Severity> {
        self.active
            .values()
            .filter(|alarm| {
                self.defs
                    .get(&alarm.def_id)
                    .is_some_and(|def| Self::targets(def, component_id, element))
            })
            .map(|alarm| alarm.severity)
            .max()
    }

    fn targets(def: &AlarmDef, component_id: ComponentId, element: usize) -> bool {
        def.target.as_ref().is_some_and(|target| {
            target.component_id == component_id
                && target
                    .element_index
                    .map(|i| i as usize == element)
                    .unwrap_or(true)
        })
    }

    /// Worst severity among everything pending, for the titlebar's status dot.
    pub fn highest_pending_severity(&self) -> Option<Severity> {
        self.pending_states().map(|(severity, _)| severity).max()
    }

    /// `[info, warning, critical]` counts over pending alarms.
    pub fn counts_by_severity(&self) -> [usize; 3] {
        let mut counts = [0usize; 3];
        for (severity, _) in self.pending_states() {
            counts[severity_index(severity)] += 1;
        }
        counts
    }

    /// How many alarms are awaiting an operator, latched ones included.
    pub fn pending_count(&self) -> usize {
        self.pending_states().count()
    }

    pub fn unacked_count(&self) -> usize {
        self.pending_states()
            .filter(|(_, state)| *state != TileState::AlarmAcked)
            .count()
    }

    /// The pending set reduced to what the counts need, skipping the clones
    /// [`Self::pending_sorted`] hands the renderer.
    fn pending_states(&self) -> impl Iterator<Item = (Severity, TileState)> + '_ {
        let now = Timestamp::now();
        let live = self
            .active
            .values()
            .filter(move |alarm| !self.is_shelved_at(&alarm.def_id, now))
            .map(move |alarm| {
                let state = match self.acked.contains(&alarm.occurrence) {
                    true => TileState::AlarmAcked,
                    false => TileState::AlarmUnacked,
                };
                (alarm.severity, state)
            });
        let latched = self
            .latched
            .values()
            .filter(move |entry| !self.is_shelved_at(&entry.alarm.def_id, now))
            .map(|entry| (entry.alarm.severity, TileState::ClearedUnacked));
        live.chain(latched)
    }

    pub fn history(&self) -> &VecDeque<AlarmEvent> {
        &self.history
    }
}

pub fn severity_index(severity: Severity) -> usize {
    match severity {
        Severity::Info => 0,
        Severity::Warning => 1,
        Severity::Critical => 2,
    }
}

/// The gpui entity wrapping [`AlarmState`]; owns the in-process ingestion tasks and the
/// DB handle used to publish acks.
pub struct AlarmStore {
    state: AlarmState,
    db: Arc<DB>,
    operator: String,
    _task: Task<()>,
    _ticker: Task<()>,
}

/// Hands the shared [`AlarmStore`] entity to any part of the app.
pub struct GlobalAlarmStore(pub Entity<AlarmStore>);

impl Global for GlobalAlarmStore {}

/// The shared alarm store, or `None` if it was never initialized (e.g. in tests).
pub fn try_global(cx: &App) -> Option<Entity<AlarmStore>> {
    cx.try_global::<GlobalAlarmStore>().map(|g| g.0.clone())
}

impl AlarmStore {
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let entity = cx.new(|cx| AlarmStore::new(db, cx));
        cx.set_global(GlobalAlarmStore(entity));
    }

    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let operator = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_else(|_| "operator".to_string());

        // Sources list cause before effect: defs and raises must fold before the clears
        // and acks that reference them, so equal-timestamp records merge in this order.
        let task = cx.spawn({
            let db = db.clone();
            async move |this, cx| {
                let sources = vec![
                    // The live snapshot channel: the whole def set in one
                    // record (retained by the link for late joiners).
                    IngestSource::new(AlarmDefs::ID, |store: &mut Self, _ts, defs: AlarmDefs| {
                        for def in defs.defs {
                            store.state.apply_def(def);
                        }
                    }),
                    // Per-def records, still folded for recordings that
                    // predate the set-shaped channel.
                    IngestSource::new(AlarmDef::ID, |store: &mut Self, _ts, def| {
                        store.state.apply_def(def)
                    }),
                    IngestSource::new(AlarmRaised::ID, |store: &mut Self, ts, raised| {
                        store.state.apply_raised(ts, raised)
                    }),
                    IngestSource::new(AlarmShelved::ID, |store: &mut Self, ts, shelved| {
                        store.state.apply_shelved(ts, shelved)
                    }),
                    IngestSource::new(AlarmUnshelved::ID, |store: &mut Self, ts, unshelved| {
                        store.state.apply_unshelved(ts, unshelved)
                    }),
                    IngestSource::new(AlarmCleared::ID, |store: &mut Self, ts, cleared| {
                        store.state.apply_cleared(ts, cleared)
                    }),
                    IngestSource::new(AlarmAck::ID, |store: &mut Self, ts, ack| {
                        store.state.apply_ack(ts, ack)
                    }),
                ];
                ingest_all(db, sources, this, cx).await
            }
        });

        Self {
            state: AlarmState::default(),
            db,
            operator,
            _task: task,
            _ticker: spawn_shelf_ticker(cx),
        }
    }

    pub fn state(&self) -> &AlarmState {
        &self.state
    }

    /// Publish an operator acknowledgment for `occurrence`, live or latched. The
    /// resulting [`AlarmAck`] is broadcast back through the DB and folded into state by
    /// the ack reader, so this is fire-and-forget.
    pub fn acknowledge(&self, occurrence: OccurrenceId) {
        let Some(def_id) = self.state.def_of(occurrence) else {
            return;
        };
        let ack = AlarmAck {
            def_id: def_id.clone(),
            occurrence,
            operator: self.operator.clone(),
            note: None,
        };
        self.publish(AlarmAck::ID, &ack);
    }

    /// Acknowledge everything pending that hasn't been acked yet.
    pub fn acknowledge_all(&self) {
        let unacked: Vec<OccurrenceId> = self
            .state
            .pending_sorted()
            .iter()
            .filter(|pending| pending.state != TileState::AlarmAcked)
            .map(|pending| pending.alarm.occurrence)
            .collect();
        for occurrence in unacked {
            self.acknowledge(occurrence);
        }
    }

    /// Shelve `def_id` for `duration`, capped at [`MAX_SHELF_DURATION`]. Published like
    /// an ack so every console shelves the point, but never uplinked — the control
    /// system keeps evaluating the alarm.
    pub fn shelve(&self, def_id: AlarmId, duration: Duration, reason: Option<String>) {
        let shelved = AlarmShelved {
            def_id,
            until: Timestamp::now() + duration.min(MAX_SHELF_DURATION),
            reason,
            operator: self.operator.clone(),
        };
        self.publish(AlarmShelved::ID, &shelved);
    }

    /// End a shelf early.
    pub fn unshelve(&self, def_id: AlarmId) {
        let unshelved = AlarmUnshelved {
            def_id,
            operator: self.operator.clone(),
        };
        self.publish(AlarmUnshelved::ID, &unshelved);
    }

    fn publish<T: serde::Serialize>(&self, id: metor_proto::types::PacketId, msg: &T) {
        if let Ok(bytes) = postcard::to_allocvec(msg) {
            let _ = self.db.push_msg(Timestamp::now(), id, &bytes);
        }
    }
}

/// Repaint every observer once a second while any shelf is live, so the countdowns tick
/// and a point reappears the moment its shelf expires (expiry is a query-time
/// comparison, not an event). The timer costs one wakeup a second and notifies nobody
/// when there is nothing shelved.
fn spawn_shelf_ticker(cx: &mut Context<AlarmStore>) -> Task<()> {
    cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(SHELF_TICK).await;
            let alive = this.update(cx, |store, cx| {
                if !store.state.shelves.is_empty() {
                    cx.notify();
                }
            });
            if alive.is_err() {
                break;
            }
        }
    })
}

const SHELF_TICK: Duration = Duration::from_secs(1);
