//! The control system's sequence channels: which exist, what each may load,
//! and which components carry each one's state, plus the operator's commands
//! back to the control system.
//!
//! A *channel* is a slot that holds at most one loaded *sequence* (a small script that
//! drives the system). The control system is the source of truth: it declares the channels
//! via [`SequenceRegistry`] and telemeters each channel's whole state as ordinary
//! components every cycle, naming them in the registry. [`SequenceStore`] keeps only that
//! declaration. A channel's state is read like any other telemetry, at the selected time,
//! by [`ChannelStatusReader`], and its history of messages by [`SequenceHistory`]; the
//! panel never decides what a channel's state is or which commands it would take.
//!
//! Commands ([`SequenceCommand`]/[`ReloadSequences`]) are fire-and-forget: the control
//! system executes one and the result shows up in the channel's components.

mod history;
mod status;

use std::sync::Arc;

use gpui::{App, Entity, SharedString, Task};
use metor_db::DB;
use metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_proto_wkt::{
    ReloadSequences, SequenceChannelComponents, SequenceChannelState, SequenceCommand,
    SequenceCommandKind, SequenceRegistry,
};

use crate::msg_ingest::{self, IngestSource};

pub use history::{HistoryEntry, SequenceHistory};
pub use status::{ChannelReaders, ChannelStatus, ChannelStatusReader};

#[cfg(test)]
pub(crate) mod tests;

/// One declared channel. The **name** (the slot's instance name on the control
/// system) is its identity: the key `SequenceCommand::channel` addresses.
#[derive(Clone, Debug, PartialEq)]
pub struct Channel {
    pub name: SharedString,
    /// Sequence names that may be loaded into this channel; the channel's
    /// `loaded` component indexes it.
    pub available: Vec<SharedString>,
    /// The wrapped system's type, shown in place of a sequence when the
    /// channel loads nothing.
    pub system: SharedString,
    pub components: ChannelComponents,
}

impl Channel {
    /// The sequence the `loaded` index names, if it is in range.
    pub fn sequence(&self, loaded: Option<u32>) -> Option<SharedString> {
        self.available.get(loaded? as usize).cloned()
    }
}

/// The component carrying each field of a channel's state. `None` where the
/// registry names nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChannelComponents {
    pub state: Option<ComponentId>,
    pub loaded: Option<ComponentId>,
    pub artifact: Option<ComponentId>,
    pub accepts: Option<ComponentId>,
    pub message: Option<ComponentId>,
    pub elapsed: Option<ComponentId>,
    pub abort_elapsed: Option<ComponentId>,
}

impl ChannelComponents {
    fn from_names(names: &SequenceChannelComponents) -> Self {
        let id = |name: &str| (!name.is_empty()).then(|| ComponentId::new(name));
        Self {
            state: id(&names.state),
            loaded: id(&names.loaded),
            artifact: id(&names.artifact),
            accepts: id(&names.accepts),
            message: id(&names.message),
            elapsed: id(&names.elapsed),
            abort_elapsed: id(&names.abort_elapsed),
        }
    }
}

/// The declared channels, as of the latest registry. Kept free of gpui/DB so
/// it can be tested directly.
#[derive(Default)]
pub struct SequenceState {
    channels: Vec<Channel>,
}

impl SequenceState {
    /// Replace the declaration: the registry is always whole, so channels it
    /// no longer names are gone.
    pub fn apply_registry(&mut self, registry: SequenceRegistry) {
        self.channels = registry
            .channels
            .into_iter()
            .map(|spec| Channel {
                name: spec.name.into(),
                available: spec.available.into_iter().map(SharedString::from).collect(),
                system: spec.system.into(),
                components: ChannelComponents::from_names(&spec.components),
            })
            .collect();
    }

    /// Channels in registry declaration order.
    pub fn channels_ordered(&self) -> &[Channel] {
        &self.channels
    }

    pub fn channel(&self, name: &str) -> Option<&Channel> {
        self.channels.iter().find(|channel| channel.name == name)
    }
}

/// Display name for a channel state; `None` (no reading yet, or a code this
/// panel doesn't know) reads as unknown.
pub fn state_label(state: Option<SequenceChannelState>) -> &'static str {
    match state {
        Some(SequenceChannelState::Running) => "Running",
        Some(SequenceChannelState::Stopped) => "Stopped",
        Some(SequenceChannelState::Completed) => "Completed",
        Some(SequenceChannelState::Aborted) => "Aborted",
        Some(SequenceChannelState::Failed) => "Failed",
        Some(SequenceChannelState::Panicked) => "Panicked",
        Some(SequenceChannelState::Unloaded) => "Unloaded",
        Some(SequenceChannelState::Quiescing) => "Quiescing",
        None => "Unknown",
    }
}

/// How long before `now` the instant `at` was: `"2s ago"`, `"5m ago"`, `"1.5h ago"`.
pub fn format_age(at: Timestamp, now: Timestamp) -> String {
    let secs = (now.0 - at.0).max(0) as f64 / 1e6;
    if secs < 60.0 {
        format!("{secs:.0}s ago")
    } else if secs < 3600.0 {
        format!("{:.0}m ago", secs / 60.0)
    } else {
        format!("{:.1}h ago", secs / 3600.0)
    }
}

/// The gpui entity wrapping [`SequenceState`]; owns the registry ingest task
/// and the DB handle used to publish commands.
pub struct SequenceStore {
    state: SequenceState,
    db: Arc<DB>,
    _task: Task<()>,
}

/// The shared sequence store, or `None` if it was never initialized (e.g. in tests).
pub fn try_global(cx: &App) -> Option<Entity<SequenceStore>> {
    msg_ingest::global(cx)
}

impl SequenceStore {
    /// Install the store and the [`SequenceHistory`] that follows its channels.
    pub fn init(db: Arc<DB>, cx: &mut App) {
        let store = msg_ingest::install(cx, |cx| {
            let sources = vec![IngestSource::new(
                SequenceRegistry::ID,
                |store: &mut Self, _ts, reg| store.state.apply_registry(reg),
            )];
            Self {
                state: SequenceState::default(),
                _task: msg_ingest::spawn_ingest(db.clone(), sources, cx),
                db,
            }
        });
        SequenceHistory::init(store, cx);
    }

    pub fn state(&self) -> &SequenceState {
        &self.state
    }

    pub(crate) fn db(&self) -> &Arc<DB> {
        &self.db
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

    /// Rebuild the loaded sequence from the beginning.
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
