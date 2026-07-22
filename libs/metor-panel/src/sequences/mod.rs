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
    ReloadSequences, SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
    SequenceRegistry, SequenceRunState,
};

use crate::msg_ingest::{IngestSource, ingest_all};

#[cfg(test)]
mod tests;

/// How many past events the in-memory history ring keeps for the sequence panel. Deeper
/// history can be queried from the persisted message log on demand.
const MAX_HISTORY: usize = 1000;

/// The live state of one channel, folded from the registry and its events. The channel's
/// **name** (the slot's instance name on the control system) is its identity — the same
/// key `SequenceCommand::channel` addresses.
#[derive(Clone, Debug)]
pub struct ChannelState {
    pub name: SharedString,
    /// Sequence names that may be loaded into this channel (from the registry).
    pub available: Vec<SharedString>,
    /// The currently loaded sequence — or, while a `Loading` event is pending its
    /// `Loaded`, the one being loaded.
    pub loaded: Option<SharedString>,
    pub run_state: SequenceRunState,
    /// The latest status line reported by a `Loading`/`Progress`/`Failed` event.
    pub last_message: Option<SharedString>,
    pub updated_at: Timestamp,
}

/// One entry in the sequence history log shown by the panel.
#[derive(Clone, Debug)]
pub struct SequenceLogEntry {
    pub channel_name: SharedString,
    pub timestamp: Timestamp,
    pub run_state: SequenceRunState,
    pub label: SharedString,
}

/// The folded sequence state. Kept free of gpui/DB so the reconciliation rules can be
/// tested directly.
#[derive(Default)]
pub struct SequenceState {
    channels: HashMap<SharedString, ChannelState>,
    /// Declaration order from the latest registry, for stable UI ordering.
    order: Vec<SharedString>,
    history: VecDeque<SequenceLogEntry>,
    /// Total events ever pushed; a monotonic staleness stamp for observers
    /// (the plot's event overlay) that stays valid as the ring evicts.
    history_pushed: u64,
}

impl SequenceState {
    /// Apply a whole-registry declaration. Channels that persist keep their runtime state
    /// (`loaded`/`run_state`/`last_message`); channels no longer declared are dropped.
    pub fn apply_registry(&mut self, timestamp: Timestamp, registry: SequenceRegistry) {
        let mut order = Vec::with_capacity(registry.channels.len());
        for spec in registry.channels {
            let name = SharedString::from(spec.name);
            order.push(name.clone());
            let available = spec
                .available
                .into_iter()
                .map(SharedString::from)
                .collect();
            match self.channels.get_mut(&name) {
                Some(existing) => {
                    existing.available = available;
                }
                None => {
                    self.channels.insert(
                        name.clone(),
                        ChannelState {
                            name,
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
        let incoming: HashSet<&SharedString> = order.iter().collect();
        self.channels.retain(|name, _| incoming.contains(name));
        self.order = order;
    }

    /// Apply one per-channel lifecycle event. Events for undeclared channels are ignored —
    /// the registry is the source of which channels exist.
    pub fn apply_event(&mut self, timestamp: Timestamp, event: SequenceChannelEvent) {
        let Some(ch) = self.channels.get_mut(event.channel.as_str()) else {
            return;
        };
        ch.updated_at = timestamp;
        let channel_name = ch.name.clone();
        let label: SharedString = match &event.kind {
            SequenceEventKind::Loading { name } => {
                ch.loaded = Some(name.clone().into());
                ch.run_state = SequenceRunState::Idle;
                ch.last_message = Some("Loading…".into());
                format!("Loading {name}").into()
            }
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
            SequenceEventKind::Refused { reason } => {
                // A rejected command changed nothing; report why without
                // disturbing the channel's actual run state.
                ch.last_message = Some(reason.clone().into());
                format!("Refused: {reason}").into()
            }
        };
        let run_state = ch.run_state;
        self.push_event(SequenceLogEntry {
            channel_name,
            timestamp,
            run_state,
            label,
        });
    }

    fn push_event(&mut self, event: SequenceLogEntry) {
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

    /// Channels in registry declaration order.
    pub fn channels_ordered(&self) -> Vec<&ChannelState> {
        self.order
            .iter()
            .filter_map(|name| self.channels.get(name))
            .collect()
    }

    pub fn channel(&self, name: &str) -> Option<&ChannelState> {
        self.channels.get(name)
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

/// Whether a channel in this run state may be reset to the beginning.
pub fn is_resettable(run_state: SequenceRunState) -> bool {
    matches!(
        run_state,
        SequenceRunState::Completed
            | SequenceRunState::Aborted
            | SequenceRunState::Stopped
            | SequenceRunState::Failed
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
    _task: Task<()>,
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
        // Registry declares channels before their events can fold, so it leads the
        // sources list and wins equal-timestamp ties in the merged backfill.
        let task = cx.spawn({
            let db = db.clone();
            async move |this, cx| {
                let sources = vec![
                    IngestSource::new(SequenceRegistry::ID, |store: &mut Self, ts, reg| {
                        store.state.apply_registry(ts, reg)
                    }),
                    IngestSource::new(SequenceChannelEvent::ID, |store: &mut Self, ts, ev| {
                        store.state.apply_event(ts, ev)
                    }),
                ];
                ingest_all(db, sources, this, cx).await
            }
        });

        Self {
            state: SequenceState::default(),
            db,
            _task: task,
        }
    }

    pub fn state(&self) -> &SequenceState {
        &self.state
    }

    fn publish(&self, channel: &str, command: SequenceCommandKind) {
        let cmd = SequenceCommand {
            channel: channel.to_string(),
            command,
        };
        if let Ok(bytes) = postcard::to_allocvec(&cmd) {
            let _ = self
                .db
                .push_msg(Timestamp::now(), SequenceCommand::ID, &bytes);
        }
    }

    /// Load the named sequence into the channel named `channel`.
    pub fn load(&self, channel: &str, name: impl Into<String>) {
        self.publish(channel, SequenceCommandKind::Load { name: name.into() });
    }

    pub fn start(&self, channel: &str) {
        self.publish(channel, SequenceCommandKind::Start);
    }

    /// Commanded safe-termination.
    pub fn abort(&self, channel: &str) {
        self.publish(channel, SequenceCommandKind::Abort);
    }

    /// Hard-stop (drop) — may leave the system in an unsafe state. The UI guards this with
    /// a confirmation gesture before calling it.
    pub fn stop(&self, channel: &str) {
        self.publish(channel, SequenceCommandKind::Stop);
    }

    /// Rebuild the loaded sequence from the beginning. Only meaningful from a terminal state
    /// (see [`is_resettable`]); the control system ignores it otherwise.
    pub fn reset(&self, channel: &str) {
        self.publish(channel, SequenceCommandKind::Reset);
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
