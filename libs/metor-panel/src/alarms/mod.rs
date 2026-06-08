//! Ingests the control system's alarm broadcast (declaration, raised, cleared, and
//! operator-ack messages) into a single app-global store that views observe.
//!
//! The panel never decides whether an alarm fires — the control system does that and
//! reports it via [`AlarmRaised`]/[`AlarmCleared`]. The limits carried on an
//! [`AlarmDef`] are display hints only. The one message the panel *publishes* is
//! [`AlarmAck`], so an operator's acknowledgment is seen by every connected client.
//!
//! State folding lives in [`AlarmState`] (pure, unit-tested). [`AlarmStore`] is the gpui
//! entity that owns it plus the in-process reader tasks; [`GlobalAlarmStore`] hands the
//! entity to any view via [`try_global`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use gpui::{App, Context, Entity, Global, Task, prelude::*};
use metor_db::DB;
use metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_proto_wkt::{
    AlarmAck, AlarmCleared, AlarmDef, AlarmId, AlarmLimit, AlarmRaised, OccurrenceId, Severity,
};

use crate::msg_ingest::ingest_loop;

#[cfg(test)]
mod tests;

/// How many past events the in-memory history ring keeps for the alarm panel. Deeper
/// history can be queried from the persisted message log on demand.
const MAX_HISTORY: usize = 1000;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlarmEventKind {
    Raised,
    Cleared,
    Acked,
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
    history: VecDeque<AlarmEvent>,
}

impl AlarmState {
    pub fn apply_def(&mut self, def: AlarmDef) {
        self.defs.insert(def.id.clone(), def);
    }

    pub fn apply_raised(&mut self, timestamp: Timestamp, raised: AlarmRaised) {
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

    pub fn apply_cleared(&mut self, timestamp: Timestamp, cleared: AlarmCleared) {
        let was_active = self.active.remove(&cleared.occurrence).is_some();
        self.acked.remove(&cleared.occurrence);
        // Ignore clears for occurrences we never saw raised — they carry no state.
        if was_active {
            self.push_event(AlarmEvent {
                kind: AlarmEventKind::Cleared,
                def_id: cleared.def_id,
                occurrence: cleared.occurrence,
                timestamp,
                severity: None,
                detail: String::new(),
            });
        }
    }

    pub fn apply_ack(&mut self, timestamp: Timestamp, ack: AlarmAck) {
        if !self.active.contains_key(&ack.occurrence) {
            return;
        }
        self.acked.insert(ack.occurrence);
        self.push_event(AlarmEvent {
            kind: AlarmEventKind::Acked,
            def_id: ack.def_id,
            occurrence: ack.occurrence,
            timestamp,
            severity: None,
            detail: ack.operator,
        });
    }

    fn push_event(&mut self, event: AlarmEvent) {
        self.history.push_back(event);
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    /// Active alarms, highest severity first then most recently raised.
    pub fn active_sorted(&self) -> Vec<ActiveAlarm> {
        let mut alarms: Vec<ActiveAlarm> = self.active.values().cloned().collect();
        alarms.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(b.raised_at.0.cmp(&a.raised_at.0))
        });
        alarms
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
                && target.element_index.map(|i| i == element).unwrap_or(true)
        })
    }

    pub fn highest_active_severity(&self) -> Option<Severity> {
        self.active.values().map(|alarm| alarm.severity).max()
    }

    /// `[info, warning, critical]` counts over active alarms.
    pub fn counts_by_severity(&self) -> [usize; 3] {
        let mut counts = [0usize; 3];
        for alarm in self.active.values() {
            counts[severity_index(alarm.severity)] += 1;
        }
        counts
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn unacked_count(&self) -> usize {
        self.active
            .keys()
            .filter(|occ| !self.acked.contains(occ))
            .count()
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
    _tasks: Vec<Task<()>>,
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

        let tasks = vec![
            cx.spawn({
                let db = db.clone();
                async move |this, cx| {
                    ingest_loop(db, AlarmDef::ID, this, cx, |store, _ts, def| {
                        store.state.apply_def(def)
                    })
                    .await
                }
            }),
            cx.spawn({
                let db = db.clone();
                async move |this, cx| {
                    ingest_loop(db, AlarmRaised::ID, this, cx, |store, ts, raised| {
                        store.state.apply_raised(ts, raised)
                    })
                    .await
                }
            }),
            cx.spawn({
                let db = db.clone();
                async move |this, cx| {
                    ingest_loop(db, AlarmCleared::ID, this, cx, |store, ts, cleared| {
                        store.state.apply_cleared(ts, cleared)
                    })
                    .await
                }
            }),
            cx.spawn({
                let db = db.clone();
                async move |this, cx| {
                    ingest_loop(db, AlarmAck::ID, this, cx, |store, ts, ack| {
                        store.state.apply_ack(ts, ack)
                    })
                    .await
                }
            }),
        ];

        Self {
            state: AlarmState::default(),
            db,
            operator,
            _tasks: tasks,
        }
    }

    pub fn state(&self) -> &AlarmState {
        &self.state
    }

    /// Publish an operator acknowledgment for `occurrence`. The resulting [`AlarmAck`]
    /// is broadcast back through the DB and folded into state by the ack reader, so this
    /// is fire-and-forget.
    pub fn acknowledge(&self, occurrence: OccurrenceId) {
        let Some(active) = self.state.active.get(&occurrence) else {
            return;
        };
        let ack = AlarmAck {
            def_id: active.def_id.clone(),
            occurrence,
            operator: self.operator.clone(),
            note: None,
        };
        if let Ok(bytes) = postcard::to_allocvec(&ack) {
            let _ = self.db.push_msg(Timestamp::now(), AlarmAck::ID, &bytes);
        }
    }

    /// Acknowledge every active alarm that hasn't been acked yet.
    pub fn acknowledge_all(&self) {
        let pending: Vec<OccurrenceId> = self
            .state
            .active
            .keys()
            .copied()
            .filter(|occ| !self.state.acked.contains(occ))
            .collect();
        for occurrence in pending {
            self.acknowledge(occurrence);
        }
    }
}

