//! Ingests the control system's sequence broadcast (the channel registry plus per-channel
//! lifecycle events) into a single app-global store that views observe, and publishes the
//! operator's commands back to the control system.
//!
//! A *channel* is a slot that holds at most one loaded *sequence* (a small script that
//! drives the system). The control system is the source of truth: it declares the channels
//! and their loadable sequences via [`SequenceRegistry`] and reports every transition via
//! [`SequenceChannelEvent`]. The panel only renders that state and publishes
//! [`SequenceCommand`]/[`ReloadSequences`] — it never decides a channel's run state itself.
//! Commands are fire-and-forget: the control system executes one and reports the result as
//! an event, which flows back through the ingest loop so every connected client converges.
//!
//! State folding lives in [`SequenceState`] (pure, unit-tested). [`SequenceStore`] is the
//! gpui entity that owns it plus the reader tasks; [`GlobalSequenceStore`] hands the entity
//! to any view via [`try_global`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use gpui::{App, Context, Entity, Global, SharedString, Task, prelude::*};
use metor_db::DB;
use metor_proto::types::{Msg, Timestamp};
use metor_proto_wkt::{
    ChannelId, ReloadSequences, SequenceChannelEvent, SequenceCommand, SequenceCommandKind,
    SequenceEventKind, SequenceRegistry, SequenceRunState,
};

use crate::msg_ingest::ingest_loop;

#[cfg(test)]
mod tests;

/// How many past events the in-memory history ring keeps for the sequence panel. Deeper
/// history can be queried from the persisted message log on demand.
const MAX_HISTORY: usize = 1000;

/// The live state of one channel, folded from the registry and its events.
#[derive(Clone, Debug)]
pub struct ChannelState {
    pub id: ChannelId,
    pub name: SharedString,
    /// Sequence names that may be loaded into this channel (from the registry).
    pub available: Vec<SharedString>,
    /// The currently loaded sequence, if any.
    pub loaded: Option<SharedString>,
    pub run_state: SequenceRunState,
    /// The latest status line reported by a `Progress`/`Failed` event.
    pub last_message: Option<SharedString>,
    pub updated_at: Timestamp,
}

/// One entry in the sequence history log shown by the panel.
#[derive(Clone, Debug)]
pub struct SequenceLogEntry {
    pub channel_id: ChannelId,
    pub channel_name: SharedString,
    pub timestamp: Timestamp,
    pub run_state: SequenceRunState,
    pub label: SharedString,
}

/// The folded sequence state. Kept free of gpui/DB so the reconciliation rules can be
/// tested directly.
#[derive(Default)]
pub struct SequenceState {
    channels: HashMap<ChannelId, ChannelState>,
    /// Declaration order from the latest registry, for stable UI ordering.
    order: Vec<ChannelId>,
    history: VecDeque<SequenceLogEntry>,
}

impl SequenceState {
    /// Apply a whole-registry declaration. Channels that persist keep their runtime state
    /// (`loaded`/`run_state`/`last_message`); channels no longer declared are dropped.
    pub fn apply_registry(&mut self, timestamp: Timestamp, registry: SequenceRegistry) {
        let mut order = Vec::with_capacity(registry.channels.len());
        let incoming: HashSet<ChannelId> = registry.channels.iter().map(|c| c.id).collect();
        for spec in registry.channels {
            order.push(spec.id);
            let available = spec
                .available
                .into_iter()
                .map(SharedString::from)
                .collect();
            match self.channels.get_mut(&spec.id) {
                Some(existing) => {
                    existing.name = spec.name.into();
                    existing.available = available;
                }
                None => {
                    self.channels.insert(
                        spec.id,
                        ChannelState {
                            id: spec.id,
                            name: spec.name.into(),
                            available,
                            loaded: None,
                            run_state: SequenceRunState::Idle,
                            last_message: None,
                            updated_at: timestamp,
                        },
                    );
                }
            }
        }
        self.channels.retain(|id, _| incoming.contains(id));
        self.order = order;
    }

    /// Apply one per-channel lifecycle event. Events for undeclared channels are ignored —
    /// the registry is the source of which channels exist.
    pub fn apply_event(&mut self, timestamp: Timestamp, event: SequenceChannelEvent) {
        let Some(ch) = self.channels.get_mut(&event.channel_id) else {
            return;
        };
        ch.updated_at = timestamp;
        let channel_name = ch.name.clone();
        let label: SharedString = match &event.kind {
            SequenceEventKind::Loaded { name } => {
                ch.loaded = Some(name.clone().into());
                ch.run_state = SequenceRunState::Idle;
                ch.last_message = None;
                format!("Loaded {name}").into()
            }
            SequenceEventKind::Unloaded => {
                ch.loaded = None;
                ch.run_state = SequenceRunState::Idle;
                ch.last_message = None;
                "Unloaded".into()
            }
            SequenceEventKind::Started => {
                ch.run_state = SequenceRunState::Running;
                "Started".into()
            }
            SequenceEventKind::Progress { detail } => {
                ch.run_state = SequenceRunState::Running;
                ch.last_message = Some(detail.clone().into());
                format!("Progress: {detail}").into()
            }
            SequenceEventKind::Stopped => {
                ch.run_state = SequenceRunState::Stopped;
                "Stopped".into()
            }
            SequenceEventKind::Aborted => {
                ch.run_state = SequenceRunState::Aborted;
                "Aborted".into()
            }
            SequenceEventKind::Completed => {
                ch.run_state = SequenceRunState::Completed;
                "Completed".into()
            }
            SequenceEventKind::Failed { reason } => {
                ch.run_state = SequenceRunState::Failed;
                ch.last_message = Some(reason.clone().into());
                format!("Failed: {reason}").into()
            }
        };
        let run_state = self.channels[&event.channel_id].run_state;
        self.push_event(SequenceLogEntry {
            channel_id: event.channel_id,
            channel_name,
            timestamp,
            run_state,
            label,
        });
    }

    fn push_event(&mut self, event: SequenceLogEntry) {
        self.history.push_back(event);
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }

    /// Channels in registry declaration order.
    pub fn channels_ordered(&self) -> Vec<&ChannelState> {
        self.order
            .iter()
            .filter_map(|id| self.channels.get(id))
            .collect()
    }

    pub fn channel(&self, id: ChannelId) -> Option<&ChannelState> {
        self.channels.get(&id)
    }

    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    pub fn count_in_state(&self, run_state: SequenceRunState) -> usize {
        self.channels
            .values()
            .filter(|c| c.run_state == run_state)
            .count()
    }

    pub fn history(&self) -> &VecDeque<SequenceLogEntry> {
        &self.history
    }
}

/// Stable index for a run state, used to pick a theme color (mirrors `severity_index`).
pub fn run_state_index(run_state: SequenceRunState) -> usize {
    match run_state {
        SequenceRunState::Idle => 0,
        SequenceRunState::Running => 1,
        SequenceRunState::Completed => 2,
        SequenceRunState::Aborted => 3,
        SequenceRunState::Stopped => 4,
        SequenceRunState::Failed => 5,
    }
}

/// Whether a channel in this run state may be reset to the beginning. Reset is only offered
/// from a terminal `Completed`/`Aborted` state (the control system enforces the same guard).
pub fn is_resettable(run_state: SequenceRunState) -> bool {
    matches!(
        run_state,
        SequenceRunState::Completed | SequenceRunState::Aborted
    )
}

pub fn run_state_label(run_state: SequenceRunState) -> &'static str {
    match run_state {
        SequenceRunState::Idle => "Idle",
        SequenceRunState::Running => "Running",
        SequenceRunState::Completed => "Completed",
        SequenceRunState::Aborted => "Aborted",
        SequenceRunState::Stopped => "Stopped",
        SequenceRunState::Failed => "Failed",
    }
}

/// The gpui entity wrapping [`SequenceState`]; owns the in-process ingestion tasks and the
/// DB handle used to publish commands.
pub struct SequenceStore {
    state: SequenceState,
    db: Arc<DB>,
    _tasks: Vec<Task<()>>,
}

/// Hands the shared [`SequenceStore`] entity to any part of the app.
pub struct GlobalSequenceStore(pub Entity<SequenceStore>);

impl Global for GlobalSequenceStore {}

/// The shared sequence store, or `None` if it was never initialized (e.g. in tests).
pub fn try_global(cx: &App) -> Option<Entity<SequenceStore>> {
    cx.try_global::<GlobalSequenceStore>().map(|g| g.0.clone())
}

impl SequenceStore {
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let entity = cx.new(|cx| SequenceStore::new(db, cx));
        cx.set_global(GlobalSequenceStore(entity));
    }

    fn new(db: Arc<DB>, cx: &mut Context<Self>) -> Self {
        let tasks = vec![
            cx.spawn({
                let db = db.clone();
                async move |this, cx| {
                    ingest_loop(db, SequenceRegistry::ID, this, cx, |store, ts, reg| {
                        store.state.apply_registry(ts, reg)
                    })
                    .await
                }
            }),
            cx.spawn({
                let db = db.clone();
                async move |this, cx| {
                    ingest_loop(db, SequenceChannelEvent::ID, this, cx, |store, ts, ev| {
                        store.state.apply_event(ts, ev)
                    })
                    .await
                }
            }),
        ];

        Self {
            state: SequenceState::default(),
            db,
            _tasks: tasks,
        }
    }

    pub fn state(&self) -> &SequenceState {
        &self.state
    }

    fn publish(&self, channel_id: ChannelId, command: SequenceCommandKind) {
        let cmd = SequenceCommand {
            channel_id,
            command,
        };
        if let Ok(bytes) = postcard::to_allocvec(&cmd) {
            let _ = self.db.push_msg(Timestamp::now(), SequenceCommand::ID, &bytes);
        }
    }

    /// Load the named sequence into `channel_id`.
    pub fn load(&self, channel_id: ChannelId, name: impl Into<String>) {
        self.publish(channel_id, SequenceCommandKind::Load { name: name.into() });
    }

    pub fn start(&self, channel_id: ChannelId) {
        self.publish(channel_id, SequenceCommandKind::Start);
    }

    /// Commanded safe-termination.
    pub fn abort(&self, channel_id: ChannelId) {
        self.publish(channel_id, SequenceCommandKind::Abort);
    }

    /// Hard-stop (drop) — may leave the system in an unsafe state. The UI guards this with
    /// a confirmation gesture before calling it.
    pub fn stop(&self, channel_id: ChannelId) {
        self.publish(channel_id, SequenceCommandKind::Stop);
    }

    /// Rebuild the loaded sequence from the beginning. Only meaningful from a terminal state
    /// (see [`is_resettable`]); the control system ignores it otherwise.
    pub fn reset(&self, channel_id: ChannelId) {
        self.publish(channel_id, SequenceCommandKind::Reset);
    }

    /// Ask the control system to re-read its sequence source(s) and re-publish the registry.
    pub fn reload(&self) {
        if let Ok(bytes) = postcard::to_allocvec(&ReloadSequences {}) {
            let _ = self
                .db
                .push_msg(Timestamp::now(), ReloadSequences::ID, &bytes);
        }
    }
}
