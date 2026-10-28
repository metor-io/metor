//! What happened on each channel, read back from its components.
//!
//! A channel's `message` component holds its latest transition, progress,
//! refusal or failure, and `message_at` when it was raised. Telemetry repeats
//! both every cycle, so the history is one entry per distinct `message_at`:
//! a new message always moves it, so the same text raised twice is two
//! entries. Entries are built once from the recorded history resident in the
//! db, then extended from each newly arrived sample, so a cycle costs the
//! samples it added rather than a rescan. History doesn't follow the scrub
//! time: it is the whole record, for the history tab and the plot's flags.

use std::collections::VecDeque;
use std::sync::Arc;

use gpui::{App, AppContext, Context, Entity, Global, SharedString, Subscription, Task};
use metor_db::{Component, DB};
use metor_proto::types::{ComponentId, ComponentView, Timestamp};
use metor_proto_wkt::SequenceChannelState;

use super::SequenceStore;
use super::status::{code, stamp, text};
use crate::data_binding::{BoundHistory, component_extent};

/// Most entries kept per channel; the oldest go first.
const MAX_ENTRIES: usize = 1000;

/// How far back from a channel's newest record remote history is fetched.
const FETCH_WINDOW_US: i64 = 60 * 60 * 1_000_000;

/// One message a channel raised.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    /// Unique across the history's lifetime, for caches keyed by entry.
    pub seq: u64,
    pub channel: SharedString,
    /// When the message was raised (its `message_at`).
    pub at: Timestamp,
    /// The channel's state in the same sample as the message.
    pub state: Option<SequenceChannelState>,
    pub message: SharedString,
}

/// Every declared channel's message history.
pub struct SequenceHistory {
    db: Arc<DB>,
    channels: Vec<ChannelLog>,
    next_seq: u64,
    /// Bumped on every change; a staleness stamp for observers.
    generation: u64,
    _registry: Subscription,
}

struct GlobalHistory(Entity<SequenceHistory>);
impl Global for GlobalHistory {}

/// The components one channel's history reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sources {
    message_at: ComponentId,
    state: Option<ComponentId>,
    message: Option<ComponentId>,
}

struct ChannelLog {
    name: SharedString,
    sources: Sources,
    entries: VecDeque<HistoryEntry>,
    /// The newest `message_at` sample folded in.
    scanned: Option<Timestamp>,
    /// That sample's `message_at`, so a repeat of it adds nothing.
    last_at: Option<Timestamp>,
    /// Where each source's resident history began at the last scan. Older
    /// history arriving (a fetch) or leaving (a purge) moves it, and only
    /// then is the channel rescanned.
    starts: [Option<Timestamp>; 3],
    _tail: Task<()>,
}

impl SequenceHistory {
    pub(super) fn init(store: Entity<SequenceStore>, cx: &mut App) {
        let db = store.read(cx).db().clone();
        let history = cx.new(|cx| {
            let mut this = Self {
                db,
                channels: Vec::new(),
                next_seq: 0,
                generation: 0,
                _registry: cx.observe(&store, |this: &mut Self, store, cx| this.follow(&store, cx)),
            };
            this.follow(&store, cx);
            this
        });
        cx.set_global(GlobalHistory(history));
    }

    /// The shared history, or `None` without a sequence store.
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalHistory>().map(|g| g.0.clone())
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Every channel's entries, each channel's oldest first.
    pub fn entries(&self) -> impl Iterator<Item = &HistoryEntry> {
        self.channels.iter().flat_map(|c| c.entries.iter())
    }

    /// Every entry, newest first across channels.
    pub fn newest_first(&self) -> Vec<&HistoryEntry> {
        let mut all: Vec<&HistoryEntry> = self.entries().collect();
        all.sort_by_key(|e| std::cmp::Reverse((e.at, e.seq)));
        all
    }

    /// Keep a log for each declared channel that names a `message_at`.
    fn follow(&mut self, store: &Entity<SequenceStore>, cx: &mut Context<Self>) {
        let declared: Vec<(SharedString, Sources)> = store
            .read(cx)
            .state()
            .channels_ordered()
            .iter()
            .filter_map(|c| {
                Some((
                    c.name.clone(),
                    Sources {
                        message_at: c.components.message_at?,
                        state: c.components.state,
                        message: c.components.message,
                    },
                ))
            })
            .collect();
        let unchanged = declared.len() == self.channels.len()
            && declared
                .iter()
                .zip(&self.channels)
                .all(|((name, sources), log)| *name == log.name && *sources == log.sources);
        if unchanged {
            return;
        }
        let mut old = std::mem::take(&mut self.channels);
        self.channels = declared
            .into_iter()
            .map(|(name, sources)| {
                match old
                    .iter()
                    .position(|log| log.name == name && log.sources == sources)
                {
                    Some(ix) => old.swap_remove(ix),
                    None => ChannelLog {
                        _tail: self.tail(name.clone(), sources, cx),
                        name,
                        sources,
                        entries: VecDeque::new(),
                        scanned: None,
                        last_at: None,
                        starts: [None; 3],
                    },
                }
            })
            .collect();
        self.generation += 1;
        cx.notify();
    }

    /// Scan the channel once its components register, then again whenever
    /// any of them gains samples.
    fn tail(&self, name: SharedString, sources: Sources, cx: &mut Context<Self>) -> Task<()> {
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let message_at = crate::wait_for_component(&db, sources.message_at).await;
            let mut waits = vec![message_at.time_series.waiter()];
            for id in [sources.state, sources.message].into_iter().flatten() {
                waits.push(
                    crate::wait_for_component(&db, id)
                        .await
                        .time_series
                        .waiter(),
                );
            }
            loop {
                if this.update(cx, |this, cx| this.scan(&name, cx)).is_err() {
                    return;
                }
                let mut pending: Vec<_> = waits.iter().map(|w| Box::pin(w.wait())).collect();
                let _ = crate::first_ready(&mut pending).await;
            }
        })
    }

    fn scan(&mut self, name: &SharedString, cx: &mut Context<Self>) {
        let Some(log) = self.channels.iter_mut().find(|log| log.name == *name) else {
            return;
        };
        let component = |id: Option<ComponentId>| {
            id.and_then(|id| self.db.with_state(|s| s.get_component(id).cloned()))
        };
        let Some(at) = component(Some(log.sources.message_at)) else {
            return;
        };
        let state = component(log.sources.state);
        let message = component(log.sources.message);
        let starts = [Some(&at), state.as_ref(), message.as_ref()]
            .map(|c| c.and_then(|c| c.time_series.start_timestamp()));
        let rescan = starts != log.starts;
        if rescan {
            log.starts = starts;
            log.entries.clear();
            log.scanned = None;
            log.last_at = None;
            if let Some(extent) = component_extent(&at) {
                let range =
                    Timestamp(extent.start.0.max(extent.end.0 - FETCH_WINDOW_US))..extent.end;
                for component in [Some(&at), state.as_ref(), message.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    BoundHistory {
                        component: component.clone(),
                        plan: None,
                    }
                    .request(range.clone(), cx);
                }
            }
        }
        let added = log.fold(&at, state.as_ref(), message.as_ref(), &mut self.next_seq);
        if added || rescan {
            self.generation += 1;
            cx.notify();
        }
    }
}

impl ChannelLog {
    /// Fold every `message_at` sample newer than the last one scanned.
    /// Returns whether any entry was added.
    fn fold(
        &mut self,
        at: &Component,
        state: Option<&Component>,
        message: Option<&Component>,
        next_seq: &mut u64,
    ) -> bool {
        let series = &at.time_series;
        let scanned = self.scanned;
        // Nodes run newest first; only those holding unscanned samples.
        let nodes: Vec<_> = series
            .iter_node_slices()
            .take_while(|node| {
                let last = node.full_timestamps().last().copied();
                scanned.is_none_or(|s| last.is_some_and(|l| l > s))
            })
            .collect();
        let size = at.schema.size();
        let mut added = false;
        for node in nodes.iter().rev() {
            let stamps = node.full_timestamps();
            let data = node.full_data();
            let from = self
                .scanned
                .map_or(0, |s| stamps.partition_point(|t| *t <= s));
            for (ix, &sampled) in stamps.iter().enumerate().skip(from) {
                let Some(bytes) = data.get(ix * size..(ix + 1) * size) else {
                    break;
                };
                self.scanned = Some(sampled);
                let Some(raised) = at
                    .schema
                    .parse_value(bytes)
                    .ok()
                    .and_then(|(_, v)| stamp(v))
                else {
                    continue;
                };
                // Zero is a channel that has raised nothing yet.
                if raised.0 <= 0 || self.last_at == Some(raised) {
                    continue;
                }
                self.last_at = Some(raised);
                self.entries.push_back(HistoryEntry {
                    seq: *next_seq,
                    channel: self.name.clone(),
                    at: raised,
                    state: sample_at(state, sampled, code)
                        .and_then(SequenceChannelState::from_code),
                    message: sample_at(message, sampled, text).unwrap_or_default(),
                });
                *next_seq += 1;
                added = true;
                if self.entries.len() > MAX_ENTRIES {
                    self.entries.pop_front();
                }
            }
        }
        added
    }
}

/// `component`'s sample stamped exactly `at` (the same frame as the
/// `message_at` sample), decoded.
fn sample_at<T>(
    component: Option<&Component>,
    at: Timestamp,
    decode: fn(ComponentView<'_>) -> Option<T>,
) -> Option<T> {
    let component = component?;
    let sample = component.time_series.get(at)?;
    let (_, view) = component.schema.parse_value(sample.data()).ok()?;
    decode(view)
}
