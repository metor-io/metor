//! What happened on each channel, read back from its components.
//!
//! A channel's `message` component gets one sample per transition, progress,
//! refusal or failure, stamped when it was raised, so its samples are the
//! history: the same text raised twice is two samples, and two raised in one
//! cycle share a stamp. Entries are built once from the recorded history resident in the
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
use super::status::{code, text};
use crate::data_binding::{component_extent, hydrate};

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
    /// When the message was raised: its sample's stamp.
    pub at: Timestamp,
    /// The channel's state in the cycle that raised the message.
    pub state: Option<SequenceChannelState>,
    /// Never empty.
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
    message: ComponentId,
    state: Option<ComponentId>,
}

struct ChannelLog {
    name: SharedString,
    sources: Sources,
    entries: VecDeque<HistoryEntry>,
    /// The newest stamp folded in, and how many samples carry it: messages
    /// raised in one cycle share a stamp and may arrive across scans.
    scanned: Option<(Timestamp, usize)>,
    /// Where each source's resident history began at the last scan. Older
    /// history arriving (a fetch) or leaving (a purge) moves it, and only
    /// then is the channel rescanned.
    starts: [Option<Timestamp>; 2],
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

    /// Keep a log for each declared channel that names a `message`.
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
                        message: c.components.message?,
                        state: c.components.state,
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
                        starts: [None; 2],
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
            let message = crate::wait_for_component(&db, sources.message).await;
            let mut waits = vec![message.time_series.waiter()];
            if let Some(state) = sources.state {
                let state = crate::wait_for_component(&db, state).await;
                waits.push(state.time_series.waiter());
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
        let Some(message) = component(Some(log.sources.message)) else {
            return;
        };
        let state = component(log.sources.state);
        let starts = [Some(&message), state.as_ref()]
            .map(|c| c.and_then(|c| c.time_series.start_timestamp()));
        let rescan = starts != log.starts;
        if rescan {
            log.starts = starts;
            log.entries.clear();
            log.scanned = None;
            if let Some(extent) = component_extent(&message) {
                let range =
                    Timestamp(extent.start.0.max(extent.end.0 - FETCH_WINDOW_US))..extent.end;
                for component in [Some(&message), state.as_ref()].into_iter().flatten() {
                    hydrate(component, range.clone(), cx);
                }
            }
        }
        let added = log.fold(&message, state.as_ref(), &mut self.next_seq);
        if added || rescan {
            self.generation += 1;
            cx.notify();
        }
    }
}

impl ChannelLog {
    /// Fold every message sample not yet folded. Returns whether any entry
    /// was added.
    fn fold(&mut self, message: &Component, state: Option<&Component>, next_seq: &mut u64) -> bool {
        let series = &message.time_series;
        let scanned = self.scanned.map(|(at, _)| at);
        // Nodes run newest first; only those that can hold unfolded samples.
        let nodes: Vec<_> = series
            .iter_node_slices()
            .take_while(|node| {
                let last = node.full_timestamps().last().copied();
                scanned.is_none_or(|s| last.is_some_and(|l| l >= s))
            })
            .collect();
        let size = message.schema.size();
        // The target writes a cycle's messages before its status, so a
        // message can land before the state it is read with; it waits for
        // the next wake rather than keep no state.
        let state_head = state.map(|c| c.time_series.latest().map(|l| l.timestamp()));
        // Samples at the newest folded stamp that were already folded.
        let mut skip = self.scanned.map_or(0, |(_, n)| n);
        let mut added = false;
        for node in nodes.iter().rev() {
            let stamps = node.full_timestamps();
            let data = node.full_data();
            let from = match scanned {
                Some(s) => {
                    let older = stamps.partition_point(|t| *t < s);
                    let same = stamps.partition_point(|t| *t <= s) - older;
                    let skipped = skip.min(same);
                    skip -= skipped;
                    older + skipped
                }
                None => 0,
            };
            for (ix, &at) in stamps.iter().enumerate().skip(from) {
                if state_head.is_some_and(|head| head.is_none_or(|head| head < at)) {
                    return added;
                }
                let Some(bytes) = data.get(ix * size..(ix + 1) * size) else {
                    break;
                };
                self.scanned = Some(match self.scanned {
                    Some((last, n)) if last == at => (at, n + 1),
                    _ => (at, 1),
                });
                let Some(text) = message
                    .schema
                    .parse_value(bytes)
                    .ok()
                    .and_then(|(_, v)| text(v))
                    .filter(|t| !t.is_empty())
                else {
                    continue;
                };
                self.entries.push_back(HistoryEntry {
                    seq: *next_seq,
                    channel: self.name.clone(),
                    at,
                    state: sample_at(state, at, code).and_then(SequenceChannelState::from_code),
                    message: text,
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

/// `component`'s sample stamped exactly `at`, decoded: the status the cycle
/// that raised a message wrote, which carries the same stamp.
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
