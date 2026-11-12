//! Shared asynchronous predecessor reads. Views retain leases; the registry is weak.
use gpui::{App, AppContext, Context, Entity, Global, Subscription, WeakEntity};
use metor_db::{
    Component, DB,
    manifest::{ComponentManifest, Gap, GapVec, SpanState},
    remote::{FetchError, HydratePriority},
};
use metor_proto::types::{ComponentId, Timestamp};
use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};
use stellarator::util::AtomicCell;

/// Owned bytes survive history installation, purging, and the WAL commit boundary.
#[derive(Clone, Debug)]
pub(crate) struct SelectedSample {
    pub timestamp: Timestamp,
    pub bytes: Arc<[u8]>,
    pub reconstructed: bool,
}

/// A shared buffer skips the byte compare, which for an image is a whole frame.
fn same_sample(a: Option<&SelectedSample>, b: Option<&SelectedSample>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            a.timestamp == b.timestamp
                && a.reconstructed == b.reconstructed
                && (Arc::ptr_eq(&a.bytes, &b.bytes) || a.bytes == b.bytes)
        }
        (a, b) => a.is_none() && b.is_none(),
    }
}

/// Absence is distinct from a stale but usable predecessor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SampleStatus {
    Ready,
    Loading,
    Missing,
    Error(String),
}

/// A cached display result, shared by formatting, copy, and transforms.
#[derive(Clone)]
pub(crate) struct Selection {
    pub sample: Option<SelectedSample>,
    pub status: SampleStatus,
    pub requested: Option<Timestamp>,
    pub stale: bool,
}
impl Default for Selection {
    fn default() -> Self {
        Self {
            sample: None,
            status: SampleStatus::Loading,
            requested: None,
            stale: false,
        }
    }
}

#[derive(Default)]
struct Readers(HashMap<(usize, ComponentId), WeakEntity<SelectedReader>>);
impl Global for Readers {}

/// One WAL subscription and at most one historical query per displayed component.
pub(crate) struct SelectedReader {
    db: Arc<DB>,
    id: ComponentId,
    pub component: Option<Component>,
    pub selection: Selection,
    pub changed: Arc<AtomicCell<u64>>,
    tail: VecDeque<SelectedSample>,
    tail_bytes: usize,
    cache: Option<QueryResult>,
    busy: bool,
    expression_text: Option<(u64, Option<String>)>,
    replay_plan: Option<(PlanKey, Option<crate::dynamic::ops::replay::ReplayPlan>)>,
    freshness: crate::views::binding::Freshness,
    health_age: Duration,
    health_observed: Instant,
    retry: Option<(Instant, u64)>,
    point: Option<PointRead>,
    _clock: Subscription,
    _wal: gpui::Task<()>,
    _history: gpui::Task<()>,
    _query: gpui::Task<()>,
    _point: gpui::Task<()>,
}

/// The last point read a reader asked for. One isn't asked again for the
/// same instant until coverage moves, so a read that lands nothing (or
/// fails) can't turn into a request loop.
#[derive(Clone, Debug, PartialEq)]
struct PointRead {
    id: ComponentId,
    at: Timestamp,
    /// The component's manifest generation when asked.
    generation: Option<u64>,
    failed: Option<String>,
}

/// Point reads queued during one effect flush, sent together when it ends:
/// a scrub refreshes every reader in the same flush, so a dashboard's
/// reads leave in one batch and cost one round trip.
#[derive(Default)]
struct PointBatch(Vec<(ComponentId, Timestamp, WeakEntity<SelectedReader>)>);
impl Global for PointBatch {}

/// Send the queued point reads, then await each for its reader.
fn send_points(cx: &mut App) {
    let reads = std::mem::take(&mut cx.default_global::<PointBatch>().0);
    let Some(hydrators) = crate::hydration::hydrator(cx) else {
        return;
    };
    let asks: Vec<_> = reads.iter().map(|(id, at, _)| (*id, *at)).collect();
    hydrators.request_points(&asks, HydratePriority::Selected);
    for (id, at, reader) in reads {
        let read = hydrators.point(id, at, HydratePriority::Selected);
        let landed = reader.clone();
        let task = cx.spawn(async move |cx| {
            let outcome = read.await;
            let _ = landed.update(cx, |reader, cx| reader.point_landed(id, at, outcome, cx));
        });
        let _ = reader.update(cx, |reader, _| reader._point = task);
    }
}

/// Have every live reader mark the history it shows as read. Readers touch
/// as they refresh, but a paused clock doesn't refresh them; the evictor
/// calls this before purging so a value view's instant survives however
/// long it sits still.
pub(crate) fn touch_selected(now: Instant, cx: &App) {
    let Some(readers) = cx.try_global::<Readers>() else {
        return;
    };
    for reader in readers.0.values().filter_map(|r| r.upgrade()) {
        reader.read(cx).touch(now, cx);
    }
}

pub(crate) fn acquire(db: Arc<DB>, id: ComponentId, cx: &mut App) -> Entity<SelectedReader> {
    let key = (Arc::as_ptr(&db) as usize, id);
    if let Some(existing) = cx
        .try_global::<Readers>()
        .and_then(|r| r.0.get(&key))
        .and_then(|r| r.upgrade())
    {
        return existing;
    }
    let clock = super::TemporalController::init(db.clone(), cx);
    let reader = cx.new(|cx| {
        let subscription = cx.observe(&clock, |this: &mut SelectedReader, _, cx| this.refresh(cx));
        let wal_db = db.clone();
        let wal = cx.spawn(async move |this, cx| {
            let component = crate::wait_for_component(&wal_db, id).await;
            let mut stream = crate::WalComponentStream::new(&component);
            if this
                .update(cx, |this, cx| {
                    let now = Instant::now();
                    if let Some(sample) = component.time_series.latest() {
                        this.health_age = this.freshness.observe(
                            Some(sample.timestamp()),
                            if super::config(cx).wall_clock {
                                Timestamp::now()
                            } else {
                                sample.timestamp()
                            },
                            now,
                        );
                        this.health_observed = now;
                    }
                    this.component = Some(component);
                    this.refresh(cx);
                })
                .is_err()
            {
                return;
            }
            loop {
                let sample = stream.next().await;
                let owned = SelectedSample {
                    timestamp: sample.sample_time().unwrap_or_else(Timestamp::now),
                    bytes: sample.bytes().into(),
                    reconstructed: false,
                };
                if this
                    .update(cx, |this, cx| {
                        let now = Instant::now();
                        this.health_age = this.freshness.observe(
                            Some(owned.timestamp),
                            if super::config(cx).wall_clock {
                                Timestamp::now()
                            } else {
                                owned.timestamp
                            },
                            now,
                        );
                        this.health_observed = now;
                        this.tail_bytes += owned.bytes.len();
                        if let Some(cache) = &mut this.cache {
                            cache.observe_arrival(&owned);
                        }
                        this.tail.push_back(owned);
                        while this.tail.len() > 256
                            || (this.tail_bytes > 4 * 1024 * 1024 && this.tail.len() > 1)
                        {
                            this.tail_bytes -= this.tail.pop_front().unwrap().bytes.len();
                        }
                        this.refresh(cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let history_db = db.clone();
        let history = cx.spawn(async move |this, cx| {
            let component = crate::wait_for_component(&history_db, id).await;
            loop {
                component.time_series.wait().await;
                if this.update(cx, |this, cx| this.invalidate(cx)).is_err() {
                    break;
                }
            }
        });
        SelectedReader {
            db,
            id,
            component: None,
            selection: Selection::default(),
            changed: Arc::new(AtomicCell::new(0)),
            tail: VecDeque::new(),
            tail_bytes: 0,
            cache: None,
            busy: false,
            expression_text: None,
            replay_plan: None,
            freshness: crate::views::binding::Freshness::new(Instant::now()),
            health_age: Duration::ZERO,
            health_observed: Instant::now(),
            retry: None,
            point: None,
            _clock: subscription,
            _wal: wal,
            _history: history,
            _query: gpui::Task::ready(()),
            _point: gpui::Task::ready(()),
        }
    });
    let readers = cx.default_global::<Readers>();
    readers.0.retain(|_, weak| weak.upgrade().is_some());
    readers.0.insert(key, reader.downgrade());
    reader
}

impl SelectedReader {
    /// Mark what this reader reads as in use: the span holding the selected
    /// instant's predecessor, and while playing, the read-ahead window the
    /// prefetch below fills.
    fn touch(&self, now: Instant, cx: &App) {
        let (Some(component), Some(t)) = (&self.component, super::view_time(cx)) else {
            return;
        };
        let series = &component.time_series;
        if super::snapshot(cx).is_some_and(|s| s.playing) {
            series.touch(t..read_ahead(t, cx), now);
        } else {
            series.touch(t..Timestamp(t.0.saturating_add(1)), now);
        }
    }

    fn publish(
        &mut self,
        sample: Option<SelectedSample>,
        status: SampleStatus,
        t: Option<Timestamp>,
        cx: &mut Context<Self>,
    ) {
        let threshold = crate::views::binding::STALE_AFTER;
        let stale = sample.as_ref().zip(t).is_some_and(|(s, t)| {
            t.0.saturating_sub(s.timestamp.0).max(0) as u64 >= threshold.as_micros() as u64
        }) || (sample.is_some()
            && super::is_live(cx)
            && self
                .health_age
                .saturating_add(self.health_observed.elapsed())
                >= threshold);
        let changed = self.selection.status != status
            || self.selection.stale != stale
            || !same_sample(self.selection.sample.as_ref(), sample.as_ref());
        self.selection = Selection {
            sample,
            status,
            requested: t,
            stale,
        };
        if changed {
            self.changed.store(self.changed.latest().wrapping_add(1));
            cx.notify();
        }
    }
    fn invalidate(&mut self, cx: &mut Context<Self>) {
        let preserve = self.cache.as_ref().is_some_and(|cache| {
            self.component.as_ref().is_some_and(|component| {
                component.time_series.manifest().generation == cache.manifest
            }) && cache
                .next
                .zip(cache.committed_head)
                .is_some_and(|(next, head)| next <= head)
        });
        // Ordinary ordered appends cannot change an interval already bounded
        // by committed history. Hydration changes the manifest and invalidates it.
        if !preserve {
            self.cache = None;
        }
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let requested = super::view_time(cx);
        let Some(t) = requested else {
            self.publish(None, SampleStatus::Loading, None, cx);
            return;
        };
        if self.component.is_none() {
            return;
        }
        let has_text = self.expression_text().is_some();
        let Some(component) = &self.component else {
            return;
        };
        let running = crate::dynamic::expressions::running(self.id, cx);
        let expression = has_text || running.is_some();
        // A recorded plan answers only for the tree it was resolved against;
        // a running expression carries its own.
        let tree = (self.db.vtable_gen.latest(), self.db.metadata_gen.latest());
        let plan_current = !expression
            || running.is_some()
            || self
                .replay_plan
                .as_ref()
                .is_some_and(|(key, _)| key.tree == tree);
        let manifest = component.time_series.manifest().generation;
        self.touch(Instant::now(), cx);
        // Prefetch the next remote stretch within a second of forward play,
        // a page at most. The hydrator owns deduplication and its bounded
        // request queue.
        if super::snapshot(cx).is_some_and(|s| s.playing)
            && let Some(hydrator) = crate::hydration::hydrator(cx)
            && let Some(range) = prefetch(&component.time_series.manifest(), t, read_ahead(t, cx))
        {
            hydrator.request_range(self.id, range, HydratePriority::Prefetch);
        }
        if plan_current
            && let Some(cache) = &self.cache
            && cache.manifest == manifest
            && cache.dependencies_current()
            && cache.contains(t)
        {
            self.publish(
                cache.sample.clone(),
                if cache.sample.is_some() {
                    SampleStatus::Ready
                } else {
                    SampleStatus::Missing
                },
                Some(t),
                cx,
            );
            return;
        }
        // A running expression records its own output, so at the head that
        // record is the answer; only history before it ran needs replay.
        if (!expression || running.is_some())
            && let Some(result) = latest_at_or_before(component, t, &self.tail)
        {
            self.publish(result.sample.clone(), SampleStatus::Ready, Some(t), cx);
            self.cache = Some(result);
            return;
        }
        // The predecessor is behind a remote stretch: ask now, not once the
        // query below finds out, so every reader this refresh moved asks
        // in the same batch.
        if hidden_predecessor(component, t, &self.tail)
            .is_some_and(|gap| gap.state == SpanState::RemoteOnly)
        {
            self.request_point(self.id, t, cx);
        }
        // A seek never presents the old result as the new selected instant.
        let clock = super::snapshot(cx);
        let seeking = clock.as_ref().is_some_and(|s| !s.live && !s.playing)
            && self.selection.requested != Some(t);
        if seeking
            || self
                .selection
                .sample
                .as_ref()
                .is_some_and(|sample| sample.timestamp > t)
        {
            self.publish(None, SampleStatus::Loading, Some(t), cx);
        }
        if self.busy {
            return;
        }
        // A failure waits out its backoff unless hydration moved this
        // component's coverage, which is what usually resolves it.
        if let Some((deadline, generation)) = self.retry
            && Instant::now() < deadline
            && generation == manifest
        {
            return;
        }
        self.busy = true;
        let component = self.component.clone().unwrap();
        let tail: Vec<_> = self.tail.iter().cloned().collect();
        let running = running.map(|e| e.replay_plan());
        let key = self
            .expression_text
            .as_ref()
            .and_then(|(_, text)| text.clone())
            .map(|text| PlanKey { text, tree });
        let cached_plan = self
            .replay_plan
            .as_ref()
            .filter(|(old, _)| Some(old) == key.as_ref())
            .map(|(_, plan)| plan.clone());
        let recorded = key.is_some();
        let db = self.db.clone();
        let id = self.id;
        self._query = cx.spawn(async move |this, cx| {
            let (mut result, plan) = cx
                .background_executor()
                .spawn(async move {
                    let plan = running.or_else(|| {
                        cached_plan.unwrap_or_else(|| {
                            recorded
                                .then(|| {
                                    crate::dynamic::expressions::replay_plan_from_metadata(id, &db)
                                })
                                .flatten()
                        })
                    });
                    (query_selected(&component, t, &tail, plan.as_ref()), plan)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                if this.expression_text() != key.as_ref().map(|key| key.text.as_str()) {
                    this.refresh(cx);
                    return;
                }
                if let Some(key) = key {
                    // The same text over a new tree resolves to this id with the
                    // same plan or not at all, so only a flip outdates the cache.
                    if this
                        .replay_plan
                        .as_ref()
                        .is_some_and(|(_, old)| old.is_some() != plan.is_some())
                    {
                        this.cache = None;
                    }
                    this.replay_plan = Some((key, plan));
                }
                if this.cache.as_ref().is_some_and(|cache| {
                    super::view_time(cx).is_some_and(|t| cache.contains(t))
                        && this
                            .component
                            .as_ref()
                            .is_some_and(|c| c.time_series.manifest().generation == cache.manifest)
                        && cache.dependencies_current()
                }) {
                    this.refresh(cx);
                    return;
                }
                // Samples arriving before the WAL commits remain eligible through Pause.
                let selected = super::view_time(cx);
                let complete_at = selected.filter(|now| result.contains(*now)).unwrap_or(t);
                for sample in &this.tail {
                    result.take(sample.clone(), complete_at);
                }
                let coverage_current = this
                    .component
                    .as_ref()
                    .is_some_and(|c| c.time_series.manifest().generation == result.manifest);
                if !coverage_current || selected.is_none_or(|now| !result.contains(now)) {
                    // A nearby advancing clock can reuse the proven predecessor interval.
                    if coverage_current && result.missing.is_none() && result.error.is_none() {
                        this.cache = Some(result);
                    }
                    this.refresh(cx);
                    return;
                }
                let t = selected.unwrap_or(t);
                if result.missing.as_ref().is_some_and(|missing| {
                    missing.id == this.id
                        && !hides(&missing.gap, result.sample.as_ref().map(|s| s.timestamp))
                }) {
                    result.missing = None;
                }
                if let Some(missing) = result.missing.clone() {
                    this.retry = Some((Instant::now() + RETRY_BACKOFF, result.manifest));
                    if crate::hydration::hydrator(cx).is_some() {
                        // A stretch mid-fetch as a whole node lands by
                        // itself, waking the history task.
                        let failed = if missing.remote {
                            this.request_point(missing.id, missing.at, cx)
                        } else {
                            None
                        };
                        let status = failed.map_or(SampleStatus::Loading, SampleStatus::Error);
                        this.publish(None, status, Some(t), cx);
                    } else {
                        this.publish(
                            None,
                            SampleStatus::Error(
                                "History is remote and its source is disconnected".into(),
                            ),
                            Some(t),
                            cx,
                        );
                    }
                    return;
                }
                if let Some(error) = result.error.clone() {
                    this.retry = Some((Instant::now() + RETRY_BACKOFF, result.manifest));
                    this.publish(None, SampleStatus::Error(error), Some(t), cx);
                    return;
                }
                this.publish(
                    result.sample.clone(),
                    if result.sample.is_some() {
                        SampleStatus::Ready
                    } else {
                        SampleStatus::Missing
                    },
                    Some(t),
                    cx,
                );
                this.retry = None;
                this.cache = Some(result);
            });
        });
    }

    /// Point-read `at` on `id` (this component, or an input a reconstruction
    /// reads): queued for the flush's batch unless this reader already
    /// asked for that instant at the current coverage. Returns why that
    /// earlier read failed, if it did.
    fn request_point(
        &mut self,
        id: ComponentId,
        at: Timestamp,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let generation = if id == self.id {
            self.component
                .as_ref()
                .map(|c| c.time_series.manifest().generation)
        } else {
            self.db.with_state(|state| {
                state
                    .get_component(id)
                    .map(|c| c.time_series.manifest().generation)
            })
        };
        if let Some(point) = &self.point
            && point.id == id
            && point.at == at
            && point.generation == generation
        {
            return point.failed.clone();
        }
        self.point = Some(PointRead {
            id,
            at,
            generation,
            failed: None,
        });
        let reader = cx.weak_entity();
        let batch = cx.default_global::<PointBatch>();
        let first = batch.0.is_empty();
        batch.0.push((id, at, reader));
        if first {
            cx.defer(send_points);
        }
        None
    }

    /// A point read finished. A landed fragment already woke the history
    /// task; this refresh covers the reads that land nothing new.
    fn point_landed(
        &mut self,
        id: ComponentId,
        at: Timestamp,
        outcome: Result<Range<Timestamp>, FetchError>,
        cx: &mut Context<Self>,
    ) {
        let Some(point) = self
            .point
            .as_mut()
            .filter(|point| point.id == id && point.at == at)
        else {
            return;
        };
        match outcome {
            Ok(_) => {}
            // Pushed out of a full queue unserved: free to ask again.
            Err(FetchError::Expired) => self.point = None,
            Err(err) => {
                point.failed = Some(format!("Fetching history failed: {err}"));
                self.retry = None;
            }
        }
        self.refresh(cx);
    }

    /// The expression recorded on this component, re-read only when metadata
    /// moves. A changed text is a different computation, so whatever was
    /// derived from the old one goes with it.
    fn expression_text(&mut self) -> Option<&str> {
        let generation = self.db.metadata_gen.latest();
        if self
            .expression_text
            .as_ref()
            .is_none_or(|(seen, _)| *seen != generation)
        {
            let text = crate::dynamic::expressions::binding_text(&self.db, self.id);
            if self
                .expression_text
                .as_ref()
                .is_some_and(|(_, old)| *old != text)
            {
                self.replay_plan = None;
                self.cache = None;
            }
            self.expression_text = Some((generation, text));
        }
        self.expression_text
            .as_ref()
            .and_then(|(_, text)| text.as_deref())
    }
}

/// The end of playback's read-ahead from `t`: one second of forward play.
fn read_ahead(t: Timestamp, cx: &App) -> Timestamp {
    Timestamp(t.0.saturating_add((super::config(cx).rate * 1_000_000.0) as i64))
}

/// The first remote stretch playback from `t` reaches by `horizon`, as one
/// range fetch.
///
/// The window is aligned to the lookahead, not to `t`: a playing reader
/// asks every frame, and a stretch clipped to `t` would be a new request
/// each time, each fetching most of the last one's page again. Aligned,
/// the ask stays the same until a page lands or play crosses a cell.
fn prefetch(
    manifest: &ComponentManifest,
    t: Timestamp,
    horizon: Timestamp,
) -> Option<Range<Timestamp>> {
    let lookahead = horizon.0.checked_sub(t.0).filter(|l| *l > 0)?;
    let start = t.0.div_euclid(lookahead) * lookahead;
    let window = Timestamp(start)..Timestamp(start.saturating_add(2 * lookahead));
    let mut gaps = GapVec::new();
    manifest.gaps(&window, &mut gaps);
    gaps.into_iter()
        .find(|gap| gap.state == SpanState::RemoteOnly && gap.range.end > t)
        .map(|gap| gap.range)
}

/// How long a failed query waits before it is asked again.
const RETRY_BACKOFF: Duration = Duration::from_millis(250);

/// What a recorded expression's replay plan was resolved against: its text,
/// and the component tree as `(vtable_gen, metadata_gen)`.
#[derive(Clone, PartialEq)]
struct PlanKey {
    text: String,
    tree: (u64, u64),
}

/// A predecessor this side can't answer: the newest stretch at or before
/// `at` that isn't held locally, with no local sample after it.
#[derive(Clone, Debug, PartialEq)]
struct Missing {
    id: ComponentId,
    /// The instant whose predecessor is wanted.
    at: Timestamp,
    gap: Range<Timestamp>,
    /// Whether a point read fetches it. Otherwise the stretch is already
    /// being fetched as a whole node.
    remote: bool,
}

struct QueryResult {
    sample: Option<SelectedSample>,
    next: Option<Timestamp>,
    missing: Option<Missing>,
    manifest: u64,
    committed_head: Option<Timestamp>,
    dependencies: Vec<(Component, u64, Option<Timestamp>)>,
    error: Option<String>,
}
impl QueryResult {
    fn dependencies_current(&self) -> bool {
        self.dependencies.iter().all(|(c, generation, latest)| {
            c.time_series.manifest().generation == *generation
                && c.time_series.latest().map(|s| s.timestamp()) == *latest
        })
    }
    fn observe_arrival(&mut self, sample: &SelectedSample) {
        // A late sample after the predecessor changes the answer from its own
        // timestamp onward. Earlier arrivals cannot change this interval.
        if self
            .sample
            .as_ref()
            .is_none_or(|old| sample.timestamp > old.timestamp)
        {
            self.next = Some(
                self.next
                    .map_or(sample.timestamp, |next| next.min(sample.timestamp)),
            );
        } else if self
            .sample
            .as_ref()
            .is_some_and(|old| sample.timestamp == old.timestamp)
        {
            self.sample = Some(sample.clone());
        }
    }
    fn take(&mut self, sample: SelectedSample, t: Timestamp) {
        if sample.timestamp <= t {
            if self
                .sample
                .as_ref()
                .is_none_or(|s| sample.timestamp >= s.timestamp)
            {
                self.sample = Some(sample);
            }
        } else {
            self.next = Some(
                self.next
                    .map_or(sample.timestamp, |next| next.min(sample.timestamp)),
            );
        }
    }
    fn contains(&self, t: Timestamp) -> bool {
        self.sample.as_ref().is_none_or(|s| s.timestamp <= t)
            && self.next.is_none_or(|next| t < next)
    }
}

// At the recording head there is no reason to traverse history or launch a
// worker. A reference clock behind this component still needs a predecessor query.
fn latest_at_or_before(
    component: &Component,
    t: Timestamp,
    tail: &VecDeque<SelectedSample>,
) -> Option<QueryResult> {
    let manifest = component.time_series.manifest();
    let latest = component.time_series.latest()?;
    if latest.timestamp() > t {
        return None;
    }
    let mut result = QueryResult {
        sample: Some(SelectedSample {
            timestamp: latest.timestamp(),
            bytes: latest.data().into(),
            reconstructed: false,
        }),
        next: None,
        missing: None,
        manifest: manifest.generation,
        committed_head: component
            .time_series
            .latest()
            .map(|sample| sample.timestamp()),
        dependencies: vec![],
        error: None,
    };
    // Committed samples are already in `latest`; only the uncommitted suffix
    // can move the answer.
    let head = latest.timestamp();
    for sample in tail.range(tail.partition_point(|s| s.timestamp < head)..) {
        result.take(sample.clone(), t);
    }
    let predecessor = result.sample.as_ref()?.timestamp;
    if newest_gap(&manifest, t).is_some_and(|gap| hides(&gap.range, Some(predecessor))) {
        return None;
    }
    let next = manifest.spans.partition_point(|span| span.cover_start <= t);
    for bound in manifest
        .spans
        .get(next)
        .map(|span| span.cover_start)
        .into_iter()
        .chain(first_gap_after(&manifest, t))
    {
        result.next = Some(result.next.map_or(bound, |t| t.min(bound)));
    }
    (component.time_series.manifest().generation == manifest.generation).then_some(result)
}

fn at_or_before(component: &Component, t: Timestamp, tail: &[SelectedSample]) -> QueryResult {
    let manifest = component.time_series.manifest();
    let mut result = QueryResult {
        sample: None,
        next: None,
        missing: None,
        manifest: manifest.generation,
        committed_head: component
            .time_series
            .latest()
            .map(|sample| sample.timestamp()),
        dependencies: vec![],
        error: None,
    };
    for node in component.time_series.iter_node_slices() {
        let times = node.timestamps();
        if result
            .sample
            .as_ref()
            .is_some_and(|s| times.last().is_some_and(|last| *last < s.timestamp))
        {
            break;
        }
        let i = times.partition_point(|ts| *ts <= t);
        if let Some(next) = times.get(i) {
            result.next = Some(result.next.map_or(*next, |old| old.min(*next)));
        }
        if let Some(index) = i.checked_sub(1)
            && result
                .sample
                .as_ref()
                .is_none_or(|old| times[index] >= old.timestamp)
        {
            let size = component.schema.size();
            if let Some(bytes) = node.data().get(index * size..(index + 1) * size) {
                result.take(
                    SelectedSample {
                        timestamp: times[index],
                        bytes: bytes.into(),
                        reconstructed: false,
                    },
                    t,
                );
            }
        }
    }
    for sample in tail {
        result.take(sample.clone(), t);
    }
    let predecessor = result.sample.as_ref().map(|s| s.timestamp);
    result.missing = newest_gap(&manifest, t)
        .filter(|gap| hides(&gap.range, predecessor))
        .map(|gap| Missing {
            id: component.component_id,
            at: t,
            remote: gap.state == SpanState::RemoteOnly,
            gap: gap.range,
        });
    // Past the first stretch held neither as a node nor as a fragment, a
    // newer predecessor may be hiding.
    for bound in manifest
        .spans
        .iter()
        .filter(|span| span.cover_start > t)
        .map(|span| span.cover_start)
        .chain(first_gap_after(&manifest, t))
    {
        result.next = Some(result.next.map_or(bound, |next| next.min(bound)));
    }
    result
}

/// Whether `gap` can hold a newer sample than `predecessor`, the newest
/// local sample at or before the instant.
fn hides(gap: &Range<Timestamp>, predecessor: Option<Timestamp>) -> bool {
    predecessor.is_none_or(|s| gap.end.0.saturating_sub(1) > s.0)
}

/// The newest stretch at or before `t` this side can't answer: a remote or
/// mid-fetch span, less what range fetches hold. Walks back from `t` and
/// stops at the first, so an archive behind it costs nothing.
fn newest_gap(manifest: &ComponentManifest, t: Timestamp) -> Option<Gap> {
    let end = manifest.spans.partition_point(|span| span.cover_start <= t);
    let mut gaps = GapVec::new();
    for span in manifest.spans[..end].iter().rev() {
        if span.state == SpanState::Resident {
            continue;
        }
        let through = Timestamp(span.cover_end.0.min(t.0).saturating_add(1));
        manifest.gaps(&(span.cover_start..through), &mut gaps);
        if let Some(gap) = gaps.pop() {
            return Some(gap);
        }
    }
    None
}

/// Where the first stretch after `t` this side can't answer starts.
fn first_gap_after(manifest: &ComponentManifest, t: Timestamp) -> Option<Timestamp> {
    let after = Timestamp(t.0.saturating_add(1));
    let start = manifest
        .spans
        .partition_point(|span| span.cover_end < after);
    let mut gaps = GapVec::new();
    for span in &manifest.spans[start..] {
        if span.state == SpanState::Resident {
            continue;
        }
        let through = Timestamp(span.cover_end.0.saturating_add(1));
        manifest.gaps(&(span.cover_start.max(after)..through), &mut gaps);
        if let Some(gap) = gaps.first() {
            return Some(gap.range.start);
        }
    }
    None
}

/// The stretch hiding `t`'s predecessor, when one does: [`at_or_before`]'s
/// `missing`, decided without collecting the sample. Cheap enough for every
/// refresh: the node walk only runs once a gap is found.
fn hidden_predecessor(
    component: &Component,
    t: Timestamp,
    tail: &VecDeque<SelectedSample>,
) -> Option<Gap> {
    let gap = newest_gap(&component.time_series.manifest(), t)?;
    let shown = |s: Timestamp| s <= t && !hides(&gap.range, Some(s));
    let held = component.time_series.iter_node_slices().any(|node| {
        let times = node.timestamps();
        let i = times.partition_point(|s| *s <= t);
        i.checked_sub(1).is_some_and(|i| shown(times[i]))
    }) || tail.iter().any(|s| shown(s.timestamp));
    (!held).then_some(gap)
}

/// Stateless point reconstruction reads only predecessor inputs. Stateful replay
/// needs a checkpoint/warmup policy; a cold start would invent a historical state.
fn query_selected(
    component: &Component,
    t: Timestamp,
    tail: &[SelectedSample],
    plan: Option<&crate::dynamic::ops::replay::ReplayPlan>,
) -> QueryResult {
    let mut result = at_or_before(component, t, tail);
    let Some(plan) = plan else {
        return result;
    };
    if result.missing.is_some() {
        return result;
    }
    let desc = &plan.compiled.manifest.systems[plan.system];
    let Some(driving_port) = desc.driving.and_then(|i| plan.ports.get(i)) else {
        if result.sample.is_none() {
            result.error =
                Some("Historical reconstruction unavailable: no recorded driving sample".into());
        }
        return result;
    };
    let driving = at_or_before(driving_port, t, &[]);
    result.dependencies = plan
        .ports
        .iter()
        .map(|c| {
            (
                c.clone(),
                c.time_series.manifest().generation,
                c.time_series.latest().map(|s| s.timestamp()),
            )
        })
        .collect();
    if let Some(next) = driving.next {
        result.next = Some(result.next.map_or(next, |old| old.min(next)));
    }
    if let Some(missing) = driving.missing {
        result.missing = Some(missing);
        return result;
    }
    let Some(driving) = driving.sample else {
        return result;
    };
    if result
        .sample
        .as_ref()
        .is_some_and(|s| s.timestamp >= driving.timestamp)
    {
        return result;
    }
    if !desc.state.is_empty() {
        result.error = Some(
            "Historical reconstruction unavailable: stateful expression requires a checkpoint"
                .into(),
        );
        return result;
    }
    // Held inputs must be complete at the driving instant, which can precede
    // view time. A later held sample cannot prove that earlier coverage exists.
    for port in &plan.ports {
        let input = at_or_before(port, driving.timestamp, &[]);
        if let Some(missing) = input.missing {
            result.missing = Some(missing);
            return result;
        }
        if input.sample.is_none() {
            result.error =
                Some("Historical reconstruction unavailable: an input has no predecessor".into());
            return result;
        }
    }
    let Some((field, _)) = plan
        .outputs
        .iter()
        .find(|(_, id)| *id == component.component_id)
    else {
        return result;
    };
    let start = driving.timestamp;
    let replay = crate::dynamic::ops::replay::replay(
        plan,
        start..Timestamp(start.0.saturating_add(1)),
        crate::dynamic::ops::program::DEFAULT_FUEL,
        &mut |timestamp, frame| {
            let mut bytes = Vec::new();
            plan.field(*field, frame, &mut bytes);
            result.sample = Some(SelectedSample {
                timestamp,
                bytes: bytes.into(),
                reconstructed: true,
            });
            false
        },
    );
    if let Err(error) = replay {
        result.error = Some(format!("Historical reconstruction failed: {error}"));
    }
    result
}

pub(crate) fn current(db: &Arc<DB>, id: ComponentId, cx: &App) -> Option<Selection> {
    let key = (Arc::as_ptr(db) as usize, id);
    let reader = cx.try_global::<Readers>()?.0.get(&key)?.upgrade()?;
    Some(reader.read(cx).selection.clone())
}

pub(crate) fn status_text(db: &Arc<DB>, id: ComponentId, cx: &App) -> String {
    let Some(selection) = current(db, id, cx) else {
        return "Loading".into();
    };
    match selection.status {
        SampleStatus::Loading => "Loading selected time…".into(),
        SampleStatus::Missing => "No sample at or before selected time".into(),
        SampleStatus::Error(error) => error,
        SampleStatus::Ready => {
            let Some(sample) = selection.sample else {
                return "No sample".into();
            };
            let mode = if sample.reconstructed {
                "Reconstructed · stateless"
            } else if super::is_live(cx) {
                "Live"
            } else {
                "Historical · provenance unknown"
            };
            format!(
                "{mode}{} · sample {}",
                if selection.stale { " · stale" } else { "" },
                super::model::timestamp_text(sample.timestamp, &super::config(cx).timezone)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metor_db::{ComponentSchema, manifest::SpanSource, seal::SealRecord, store};
    use metor_proto::types::PrimType;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fixture() -> (tempfile::TempDir, Arc<DB>, Component) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DB::create(temp.path().join("db")).unwrap());
        let id = ComponentId::new("sensor");
        db.with_state_mut(|s| {
            s.insert_component(id, ComponentSchema::new(PrimType::F64, &[][..]), &db.path)
        })
        .unwrap();
        let component = db.with_state(|s| s.get_component(id).cloned()).unwrap();
        install(&component, &[10, 20, 100]);
        (temp, db, component)
    }
    fn install(c: &Component, stamps: &[i64]) {
        let bytes: Vec<_> = stamps.iter().map(|s| (*s as f64).to_le_bytes()).collect();
        c.time_series
            .install_samples(
                8,
                stamps
                    .iter()
                    .zip(&bytes)
                    .map(|(s, b)| (Timestamp(*s), b.as_slice())),
                SpanSource::RemoteFetch,
            )
            .unwrap();
    }
    fn stamp(q: &QueryResult) -> Option<i64> {
        q.sample.as_ref().map(|s| s.timestamp.0)
    }

    #[test]
    fn predecessor_is_inclusive_and_never_nearest_future() {
        let (_temp, _db, c) = fixture();
        for (at, expected) in [
            (0, None),
            (10, Some(10)),
            (19, Some(10)),
            (20, Some(20)),
            (99, Some(20)),
            (100, Some(100)),
            (110, Some(100)),
        ] {
            let q = at_or_before(&c, Timestamp(at), &[]);
            assert_eq!(stamp(&q), expected);
            assert!(q.contains(Timestamp(at)));
            let fast = latest_at_or_before(&c, Timestamp(at), &VecDeque::new());
            if at >= 100 {
                assert_eq!(stamp(&fast.unwrap()), expected);
            } else {
                assert!(fast.is_none());
            }
        }
        assert!(!at_or_before(&c, Timestamp(99), &[]).contains(Timestamp(100)));
        assert!(!at_or_before(&c, Timestamp(20), &[]).contains(Timestamp(19)));
    }

    #[test]
    fn remote_coverage_blocks_an_incomplete_predecessor_and_bounds_cache() {
        let (_temp, _db, c) = fixture();
        // A separate archive extent after the installed node.
        c.time_series
            .merge_remote_spans([SealRecord {
                start_ts: Timestamp(200),
                end_ts: Timestamp(299),
                count: 100,
                index_len: 800,
                data_len: 800,
                checksum: 1,
                element_size: 8,
            }])
            .unwrap();
        let before = at_or_before(&c, Timestamp(150), &[]);
        assert_eq!(stamp(&before), Some(100));
        assert!(before.missing.is_none());
        assert!(!before.contains(Timestamp(200)));
        let inside = at_or_before(&c, Timestamp(250), &[]);
        assert_eq!(inside.missing.unwrap().gap, Timestamp(200)..Timestamp(251));
        assert!(latest_at_or_before(&c, Timestamp(250), &VecDeque::new()).is_none());
        let fast = latest_at_or_before(&c, Timestamp(150), &VecDeque::new()).unwrap();
        assert_eq!(stamp(&fast), Some(100));
        assert!(!fast.contains(Timestamp(200)));
        let tail = [SelectedSample {
            timestamp: Timestamp(300),
            bytes: 300f64.to_le_bytes().into(),
            reconstructed: false,
        }];
        assert!(at_or_before(&c, Timestamp(300), &tail).missing.is_none());
    }

    #[test]
    fn wal_tail_survives_pause_before_commit_and_owns_its_bytes() {
        let (_temp, _db, c) = fixture();
        let tail = vec![
            SelectedSample {
                timestamp: Timestamp(120),
                bytes: 42f64.to_le_bytes().into(),
                reconstructed: false,
            },
            SelectedSample {
                timestamp: Timestamp(130),
                bytes: 99f64.to_le_bytes().into(),
                reconstructed: false,
            },
        ];
        let fast =
            latest_at_or_before(&c, Timestamp(125), &tail.iter().cloned().collect()).unwrap();
        assert_eq!(stamp(&fast), Some(120));
        assert!(!fast.contains(Timestamp(130)));
        let paused = at_or_before(&c, Timestamp(125), &tail).sample.unwrap();
        assert_eq!(paused.timestamp, Timestamp(120));
        install(&c, &[120, 130]);
        drop(tail);
        assert_eq!(
            f64::from_le_bytes(paused.bytes.as_ref().try_into().unwrap()),
            42.0
        );
        assert_eq!(stamp(&at_or_before(&c, Timestamp(125), &[])), Some(120));
    }

    #[test]
    fn stateless_point_reconstruction_does_not_write_or_cold_start_state() {
        let (_temp, db, input) = fixture();
        db.with_state_mut(|s| {
            s.set_component_metadata(
                metor_proto_wkt::ComponentMetadata {
                    component_id: input.component_id,
                    name: "sensor".into(),
                    metadata: Default::default(),
                },
                &db.path,
            )
        })
        .unwrap();
        let id = ComponentId::new("out");
        db.with_state_mut(|s| {
            s.insert_component(id, ComponentSchema::new(PrimType::F64, &[][..]), &db.path)
        })
        .unwrap();
        let out = db.with_state(|s| s.get_component(id).cloned()).unwrap();
        let resolver = crate::dynamic::resolver::DbResolver::snapshot(&db);
        let plan = |expr| crate::dynamic::ops::replay::ReplayPlan {
            compiled: Arc::new(
                crate::dynamic::ops::program::Compiled::expression(expr, &resolver).unwrap(),
            ),
            system: 0,
            ports: vec![input.clone()],
            outputs: vec![(0, id)],
        };
        let selected = query_selected(&out, Timestamp(99), &[], Some(&plan("sensor * 2.0")));
        assert!(selected.error.is_none(), "{:?}", selected.error);
        let sample = selected.sample.unwrap();
        assert_eq!(sample.timestamp, Timestamp(20));
        assert_eq!(
            f64::from_le_bytes(sample.bytes.as_ref().try_into().unwrap()),
            40.0
        );
        assert!(sample.reconstructed);
        assert!(out.time_series.latest().is_none());
        let stateful = query_selected(&out, Timestamp(99), &[], Some(&plan("mean(sensor, 3)")));
        assert!(stateful.sample.is_none());
        assert!(stateful.error.unwrap().contains("checkpoint"));
    }

    #[test]
    fn arrivals_preserve_unaffected_predecessor_intervals() {
        let (_temp, _db, c) = fixture();
        let mut query = at_or_before(&c, Timestamp(99), &[]);
        let sample = |stamp| SelectedSample {
            timestamp: Timestamp(stamp),
            bytes: (stamp as f64).to_le_bytes().into(),
            reconstructed: false,
        };
        query.observe_arrival(&sample(120));
        assert!(query.contains(Timestamp(99)));
        query.observe_arrival(&sample(5));
        assert!(query.contains(Timestamp(99)));
        query.observe_arrival(&sample(50));
        assert!(query.contains(Timestamp(49)));
        assert!(!query.contains(Timestamp(50)));
        query.observe_arrival(&sample(20));
        assert_eq!(stamp(&query), Some(20));
    }

    #[gpui::test]
    fn archived_partial_expression_output_reconstructs_through_reader(
        cx: &mut gpui::TestAppContext,
    ) {
        use metor_proto_wkt::MetadataExt;
        let (_temp, db, input) = fixture();
        db.with_state_mut(|state| {
            state.set_component_metadata(
                metor_proto_wkt::ComponentMetadata {
                    component_id: input.component_id,
                    name: "sensor".into(),
                    metadata: Default::default(),
                },
                &db.path,
            )
        })
        .unwrap();
        for text in ["sensor * 2.0", "mean(sensor, 3)"] {
            let compiled = crate::dynamic::ops::program::Compiled::expression(
                text,
                &crate::dynamic::resolver::DbResolver::snapshot(&db),
            )
            .unwrap();
            let system = compiled.system_hash(
                0,
                &[crate::dynamic::ops::db_source::from_db_id(
                    input.component_id,
                )],
            );
            let id = ComponentId::new(&crate::dynamic::expressions::component_name(
                crate::dynamic::ops::program::field_id(system, 0),
            ));
            db.with_state_mut(|state| {
                state
                    .insert_component(id, ComponentSchema::new(PrimType::F64, &[][..]), &db.path)
                    .unwrap();
                let mut metadata = metor_proto_wkt::ComponentMetadata {
                    component_id: id,
                    name: text.into(),
                    metadata: Default::default(),
                };
                metadata.set("expression", text);
                metadata.set("hidden", "true");
                state.set_component_metadata(metadata, &db.path).unwrap();
            });
            let out = db
                .with_state(|state| state.get_component(id).cloned())
                .unwrap();
            install(&out, &[10]);
            let reader = cx.update(|cx| {
                super::super::TemporalController::init(db.clone(), cx);
                super::super::dispatch(
                    super::super::TimeAction::Seek(super::super::TimeExpr::fixed(Timestamp(99))),
                    cx,
                )
                .unwrap();
                acquire(db.clone(), id, cx)
            });
            cx.run_until_parked();
            cx.update(|cx| {
                let selected = &reader.read(cx).selection;
                if text.starts_with("mean") {
                    assert!(matches!(&selected.status, SampleStatus::Error(error) if error.contains("checkpoint")));
                } else {
                    let sample = selected.sample.as_ref().unwrap();
                    assert_eq!(sample.timestamp, Timestamp(20));
                    assert!(sample.reconstructed);
                    assert_eq!(f64::from_le_bytes(sample.bytes.as_ref().try_into().unwrap()), 40.0);
                }
                assert_eq!(out.time_series.latest().unwrap().timestamp(), Timestamp(10));
            });
        }
    }

    #[gpui::test]
    fn committed_future_appends_preserve_paused_cache(cx: &mut gpui::TestAppContext) {
        let (_temp, db, c) = fixture();
        let reader = cx.update(|cx| {
            super::super::TemporalController::init(db.clone(), cx);
            super::super::dispatch(
                super::super::TimeAction::Seek(super::super::TimeExpr::fixed(Timestamp(99))),
                cx,
            )
            .unwrap();
            acquire(db.clone(), c.component_id, cx)
        });
        cx.run_until_parked();
        let bytes = cx.update(|cx| {
            reader
                .read(cx)
                .cache
                .as_ref()
                .unwrap()
                .sample
                .as_ref()
                .unwrap()
                .bytes
                .clone()
        });
        let mut writer = c.time_series.writer().unwrap();
        writer
            .push_buf(Timestamp(120), &120f64.to_le_bytes())
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let reader = reader.read(cx);
            assert!(!reader.busy);
            assert!(Arc::ptr_eq(
                &bytes,
                &reader
                    .cache
                    .as_ref()
                    .unwrap()
                    .sample
                    .as_ref()
                    .unwrap()
                    .bytes
            ));
            assert_eq!(
                reader.selection.sample.as_ref().unwrap().timestamp,
                Timestamp(20)
            );
        });
        cx.update(|cx| {
            super::super::dispatch(
                super::super::TimeAction::Seek(super::super::TimeExpr::fixed(Timestamp(125))),
                cx,
            )
            .unwrap()
        });
        cx.run_until_parked();
        // A coalesced notification whose newest timestamp is in the future
        // must still discover the preceding append inside the cached interval.
        writer
            .push_buf(Timestamp(123), &123f64.to_le_bytes())
            .unwrap();
        writer
            .push_buf(Timestamp(150), &150f64.to_le_bytes())
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                reader.read(cx).selection.sample.as_ref().unwrap().timestamp,
                Timestamp(123)
            )
        });
    }

    #[gpui::test]
    fn committed_sample_ages_without_a_new_wal_arrival(cx: &mut gpui::TestAppContext) {
        let (_temp, db, c) = fixture();
        let reader = cx.update(|cx| acquire(db.clone(), c.component_id, cx));
        cx.run_until_parked();
        cx.update(|cx| {
            reader.update(cx, |reader, cx| {
                assert!(reader.tail.is_empty());
                assert!(!reader.selection.stale);
                reader.health_observed -= crate::views::binding::STALE_AFTER;
                reader.refresh(cx);
                assert!(reader.selection.stale);
                super::super::dispatch(
                    super::super::TimeAction::Seek(super::super::TimeExpr::fixed(Timestamp(100))),
                    cx,
                )
                .unwrap();
            })
        });
        cx.run_until_parked();
        cx.update(|cx| assert!(!reader.read(cx).selection.stale));
    }

    #[gpui::test]
    fn shared_leases_seek_backwards_and_history_install_wakes_without_live_data(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_temp, db, c) = fixture();
        let reader = cx.update(|cx| {
            super::super::TemporalController::init(db.clone(), cx);
            super::super::dispatch(
                super::super::TimeAction::Seek(super::super::TimeExpr::fixed(Timestamp(99))),
                cx,
            )
            .unwrap();
            let reader = acquire(db.clone(), c.component_id, cx);
            assert_eq!(
                reader.entity_id(),
                acquire(db.clone(), c.component_id, cx).entity_id()
            );
            reader
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                reader.read(cx).selection.sample.as_ref().unwrap().timestamp,
                Timestamp(20)
            );
            super::super::dispatch(
                super::super::TimeAction::Seek(super::super::TimeExpr::fixed(Timestamp(5))),
                cx,
            )
            .unwrap();
        });
        cx.run_until_parked();
        cx.update(|cx| assert!(reader.read(cx).selection.sample.is_none()));
        install(&c, &[1, 4]);
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                reader.read(cx).selection.sample.as_ref().unwrap().timestamp,
                Timestamp(4)
            )
        });
        let weak = reader.downgrade();
        drop(reader);
        cx.run_until_parked();
        assert!(weak.upgrade().is_none());
    }

    fn sample(timestamp: i64, bytes: &Arc<[u8]>) -> SelectedSample {
        SelectedSample {
            timestamp: Timestamp(timestamp),
            bytes: bytes.clone(),
            reconstructed: false,
        }
    }

    #[gpui::test]
    fn republishing_a_sample_notifies_once(cx: &mut gpui::TestAppContext) {
        let (_temp, db, c) = fixture();
        let reader = cx.update(|cx| acquire(db.clone(), c.component_id, cx));
        cx.run_until_parked();
        let frame: Arc<[u8]> = Arc::from(&[7u8; 64][..]);
        cx.update(|cx| {
            reader.update(cx, |reader, cx| {
                let before = reader.changed.latest();
                for _ in 0..2 {
                    let t = Some(Timestamp(500));
                    reader.publish(Some(sample(500, &frame)), SampleStatus::Ready, t, cx);
                }
                assert_eq!(reader.changed.latest(), before.wrapping_add(1));
            })
        });
    }

    fn remote_seal(start: i64, end: i64) -> SealRecord {
        SealRecord {
            start_ts: Timestamp(start),
            end_ts: Timestamp(end),
            count: (end - start + 1) as u64,
            index_len: 800,
            data_len: 800,
            checksum: 1,
            element_size: 8,
        }
    }

    /// History a peer holds unsealed, from before this side's data, reaches
    /// the extent (so the temporal range's data start), and an instant in
    /// it is a remote gap a point read fetches.
    #[test]
    fn a_peers_unsealed_head_is_history_like_any_remote_span() {
        let (_temp, _db, c) = fixture();
        let head = metor_proto_wkt::UnsealedHead {
            start_ts: Timestamp(-500),
            end_ts: Timestamp(5),
            count: 50,
            element_size: 8,
        };
        c.time_series.sync_remote(Vec::new(), Some(head)).unwrap();
        let extent = crate::data_binding::component_extent(&c).unwrap();
        assert_eq!(extent, Timestamp(-500)..Timestamp(101));
        let gap = newest_gap(&c.time_series.manifest(), Timestamp(0)).unwrap();
        assert_eq!(
            (gap.range, gap.state),
            (Timestamp(-500)..Timestamp(1), SpanState::RemoteOnly)
        );
        assert_eq!(
            prefetch(&c.time_series.manifest(), Timestamp(-400), Timestamp(-350)),
            Some(Timestamp(-400)..Timestamp(-300))
        );
    }

    #[test]
    fn fragments_answer_what_they_claim_and_nothing_past_it() {
        let (_temp, _db, c) = fixture();
        c.time_series
            .merge_remote_spans([remote_seal(200, 299)])
            .unwrap();
        // A range fetch vouched for 240..=259 and found one sample there.
        c.time_series
            .install_fragment(
                Timestamp(240)..Timestamp(260),
                8,
                &[Timestamp(250)],
                &250f64.to_le_bytes(),
            )
            .unwrap();
        let held = at_or_before(&c, Timestamp(255), &[]);
        assert_eq!(stamp(&held), Some(250));
        assert!(held.missing.is_none());
        assert!(held.contains(Timestamp(259)));
        assert!(!held.contains(Timestamp(260)));
        // Past the claim the span is remote again, and so is its start:
        // the fragment's sample doesn't answer before it.
        for (at, gap) in [(270, 260..271), (245, 200..240)] {
            let query = at_or_before(&c, Timestamp(at), &[]);
            let missing = query.missing.unwrap();
            assert_eq!(missing.gap, Timestamp(gap.start)..Timestamp(gap.end));
            assert_eq!((missing.at, missing.remote), (Timestamp(at), true));
            let hidden = hidden_predecessor(&c, Timestamp(at), &VecDeque::new());
            assert_eq!(hidden.map(|g| g.range), Some(missing.gap));
        }
        assert!(hidden_predecessor(&c, Timestamp(255), &VecDeque::new()).is_none());
        assert!(hidden_predecessor(&c, Timestamp(150), &VecDeque::new()).is_none());
    }

    #[test]
    fn playback_prefetches_one_stable_stretch() {
        let (_temp, _db, c) = fixture();
        c.time_series
            .merge_remote_spans([remote_seal(200, 299)])
            .unwrap();
        let manifest = c.time_series.manifest();
        let ask = |t: i64| prefetch(&manifest, Timestamp(t), Timestamp(t + 50));
        assert_eq!(ask(150), Some(Timestamp(200)..Timestamp(250)));
        // Frames later, the same ask, so the hydrator serves it once.
        assert_eq!(ask(170), ask(150));
        assert_eq!(ask(210), Some(Timestamp(200)..Timestamp(300)));
        assert_eq!(ask(20), None);
    }

    /// A peer store that counts what it is asked for.
    struct Counting {
        store: store::PeerStore,
        batches: std::sync::Mutex<Vec<usize>>,
        nodes: AtomicUsize,
    }

    impl store::NodeStore for Counting {
        fn put<'a>(
            &'a self,
            key: store::NodeKey<'a>,
            node: store::SealedNode<'a>,
        ) -> store::BoxFuture<'a, Result<(), store::StoreError>> {
            self.store.put(key, node)
        }

        fn get<'a>(
            &'a self,
            key: store::NodeKey<'a>,
            dst: &'a store::NodeStaging,
        ) -> store::BoxFuture<'a, Result<SealRecord, store::StoreError>> {
            self.nodes.fetch_add(1, Ordering::Relaxed);
            self.store.get(key, dst)
        }

        fn list<'a>(
            &'a self,
            component_id: ComponentId,
            component_name: &'a str,
        ) -> store::BoxFuture<'a, Result<Vec<SealRecord>, store::StoreError>> {
            self.store.list(component_id, component_name)
        }

        fn contains<'a>(
            &'a self,
            key: store::NodeKey<'a>,
        ) -> store::BoxFuture<'a, Result<bool, store::StoreError>> {
            self.store.contains(key)
        }

        fn time_series<'a>(
            &'a self,
            requests: &'a [metor_proto_wkt::GetTimeSeries],
        ) -> store::BoxFuture<
            'a,
            Result<Vec<Result<store::SeriesReply, store::StoreError>>, store::StoreError>,
        > {
            self.batches.lock().unwrap().push(requests.len());
            self.store.time_series(requests)
        }
    }

    struct Remote {
        _temp: tempfile::TempDir,
        mirror: Arc<DB>,
        store: Arc<Counting>,
        ids: Vec<ComponentId>,
        _threads: Vec<Box<dyn std::any::Any>>,
    }

    /// A peer holding `count` components, each sampled every 10 over
    /// 0..2000 in two sealed nodes, and a mirror that knows them only as
    /// remote spans, hydrating from the peer through the app's hydrators.
    fn remote(count: usize, cx: &mut gpui::TestAppContext) -> Remote {
        // The hydrator and the peer run on threads of their own.
        cx.executor().allow_parking();
        let temp = tempfile::tempdir().unwrap();
        let peer = Arc::new(DB::create(temp.path().join("peer")).unwrap());
        let mirror = Arc::new(DB::create(temp.path().join("mirror")).unwrap());
        let ids: Vec<_> = (0..count)
            .map(|i| ComponentId::new(&format!("sensor{i}")))
            .collect();
        for id in &ids {
            for db in [&peer, &mirror] {
                db.with_state_mut(|s| {
                    s.insert_component(*id, ComponentSchema::new(PrimType::F64, &[][..]), &db.path)
                })
                .unwrap();
            }
            let component = |db: &DB| db.with_state(|s| s.get_component(*id).cloned()).unwrap();
            let source = component(&peer);
            for node in [0i64..1000, 1000..2000] {
                let stamps: Vec<i64> = node.step_by(10).collect();
                install(&source, &stamps);
            }
            let seals: Vec<SealRecord> = source
                .time_series
                .manifest()
                .spans
                .iter()
                .map(|s| s.seal)
                .collect();
            component(&mirror)
                .time_series
                .merge_remote_spans(seals)
                .unwrap();
        }
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = metor_db::Server { listener, db: peer };
        let server = stellarator::struc_con::stellar(move || server.run());
        let store = Arc::new(Counting {
            store: store::PeerStore::new(addr),
            batches: Default::default(),
            nodes: Default::default(),
        });
        let hydrator = metor_db::remote::Hydrator::new();
        let (run, run_db, run_store) = (hydrator.clone(), mirror.clone(), store.clone());
        let worker = stellarator::struc_con::stellar(move || run.run(run_db, run_store));
        cx.update(|cx| {
            crate::hydration::Hydrators::global(cx)
                .insert(crate::connections::TargetId("peer".into()), hydrator);
        });
        Remote {
            _temp: temp,
            mirror,
            store,
            ids,
            _threads: vec![Box::new(server), Box::new(worker)],
        }
    }

    /// Run the app until `done`, letting the hydrator's threads work.
    fn settle(
        cx: &mut gpui::TestAppContext,
        mut done: impl FnMut(&mut gpui::TestAppContext) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn seek(t: i64, cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            super::super::dispatch(
                super::super::TimeAction::Seek(super::super::TimeExpr::fixed(Timestamp(t))),
                cx,
            )
            .unwrap()
        });
    }

    fn shown(reader: &Entity<SelectedReader>, cx: &mut gpui::TestAppContext) -> Option<f64> {
        cx.update(|cx| {
            let selection = &reader.read(cx).selection;
            (selection.status == SampleStatus::Ready).then(|| {
                let sample = selection.sample.as_ref().unwrap();
                f64::from_le_bytes(sample.bytes.as_ref().try_into().unwrap())
            })
        })
    }

    #[gpui::test]
    fn a_scrub_into_remote_history_point_reads_the_instant(cx: &mut gpui::TestAppContext) {
        let remote = remote(1, cx);
        let id = remote.ids[0];
        let reader = cx.update(|cx| {
            super::super::TemporalController::init(remote.mirror.clone(), cx);
            acquire(remote.mirror.clone(), id, cx)
        });
        seek(1234, cx);
        settle(cx, |cx| shown(&reader, cx).is_some());
        assert_eq!(shown(&reader, cx), Some(1230.0));
        assert_eq!(remote.store.nodes.load(Ordering::Relaxed), 0);
        let series = remote
            .mirror
            .with_state(|s| s.get_component(id).unwrap().time_series.clone());
        assert_eq!(
            series.manifest().span(Timestamp(1000)).unwrap().state,
            SpanState::RemoteOnly
        );
        let asked = remote.store.batches.lock().unwrap().clone();
        assert!(!asked.is_empty());
        // Inside what the read vouched for, the reader answers itself.
        seek(1232, cx);
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(50));
        cx.run_until_parked();
        assert_eq!(shown(&reader, cx), Some(1230.0));
        assert_eq!(*remote.store.batches.lock().unwrap(), asked);
        // Before it, it asks again.
        seek(1225, cx);
        settle(cx, |cx| shown(&reader, cx) == Some(1220.0));
        assert_eq!(remote.store.batches.lock().unwrap().len(), asked.len() + 1);
    }

    #[gpui::test]
    fn a_scrubbed_dashboard_point_reads_in_one_batch(cx: &mut gpui::TestAppContext) {
        const READERS: usize = 12;
        let remote = remote(READERS, cx);
        let readers: Vec<_> = cx.update(|cx| {
            super::super::TemporalController::init(remote.mirror.clone(), cx);
            remote
                .ids
                .iter()
                .map(|id| acquire(remote.mirror.clone(), *id, cx))
                .collect()
        });
        seek(555, cx);
        settle(cx, |cx| readers.iter().all(|r| shown(r, cx) == Some(550.0)));
        remote.store.batches.lock().unwrap().clear();
        seek(1555, cx);
        settle(cx, |cx| {
            readers.iter().all(|r| shown(r, cx) == Some(1550.0))
        });
        assert_eq!(*remote.store.batches.lock().unwrap(), [READERS]);
        assert_eq!(remote.store.nodes.load(Ordering::Relaxed), 0);
    }
}
