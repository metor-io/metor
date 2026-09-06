//! Shared asynchronous predecessor reads. Views retain leases; the registry is weak.
use crate::{AsComponentView, ComponentStream};
use gpui::{App, AppContext, Context, Entity, Global, Subscription, WeakEntity};
use metor_db::{Component, DB, manifest::SpanState};
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
    replay_key: Option<(String, Arc<crate::dynamic::resolver::DbResolver>)>,
    replay_plan: Option<Option<crate::dynamic::ops::replay::ReplayPlan>>,
    replay_checked: Instant,
    freshness: crate::views::binding::Freshness,
    health_age: Duration,
    health_observed: Instant,
    retry_after: Instant,
    _clock: Subscription,
    _wal: gpui::Task<()>,
    _history: gpui::Task<()>,
    _query: gpui::Task<()>,
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
                        // Future arrivals only narrow the cached predecessor interval.
                        this.retry_after = Instant::now();
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
            replay_key: None,
            replay_plan: None,
            replay_checked: Instant::now(),
            freshness: crate::views::binding::Freshness::new(Instant::now()),
            health_age: Duration::ZERO,
            health_observed: Instant::now(),
            retry_after: Instant::now(),
            _clock: subscription,
            _wal: wal,
            _history: history,
            _query: gpui::Task::ready(()),
        }
    });
    let readers = cx.default_global::<Readers>();
    readers.0.retain(|_, weak| weak.upgrade().is_some());
    readers.0.insert(key, reader.downgrade());
    reader
}

impl SelectedReader {
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
            || self
                .selection
                .sample
                .as_ref()
                .map(|s| (s.timestamp, s.bytes.as_ref(), s.reconstructed))
                != sample
                    .as_ref()
                    .map(|s| (s.timestamp, s.bytes.as_ref(), s.reconstructed));
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
        self.retry_after = Instant::now();
        self.refresh(cx);
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let requested = super::view_time(cx);
        let Some(t) = requested else {
            self.publish(None, SampleStatus::Loading, None, cx);
            return;
        };
        let Some(component) = &self.component else {
            return;
        };
        let expression_text = crate::dynamic::expressions::binding_text(&self.db, self.id);
        if self.replay_key.as_ref().map(|key| &key.0) != expression_text.as_ref() {
            self.replay_plan = None;
            self.cache = None;
        }
        let expression = expression_text.is_some()
            || crate::dynamic::expressions::running(self.id, cx).is_some();
        // Metadata has no generation counter. Periodically validate the resolver
        // on the worker, including while paused, without compiling every seek.
        let validate_plan = expression && self.replay_checked.elapsed() >= Duration::from_secs(1);
        let manifest = component.time_series.manifest().generation;
        // Prefetch at most the next sealed span in one second of forward play.
        // The hydrator owns deduplication and its bounded request queue.
        if super::snapshot(cx).is_some_and(|s| s.playing)
            && let Some(hydrator) = crate::hydration::hydrator(cx)
        {
            let horizon =
                t.0.saturating_add((super::config(cx).rate * 1_000_000.0) as i64);
            if let Some(span) = component.time_series.manifest().spans.iter().find(|s| {
                s.state == SpanState::RemoteOnly
                    && s.seal.start_ts.0 > t.0
                    && s.seal.start_ts.0 <= horizon
            }) {
                hydrator.request(
                    self.id,
                    span.seal.start_ts..Timestamp(span.cover_end.0.saturating_add(1)),
                );
            }
        }
        if !validate_plan
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
        if !expression && let Some(result) = latest_at_or_before(component, t, &self.tail) {
            self.publish(result.sample.clone(), SampleStatus::Ready, Some(t), cx);
            self.cache = Some(result);
            return;
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
        if Instant::now() < self.retry_after {
            return;
        }
        self.busy = true;
        let component = self.component.clone().unwrap();
        let tail: Vec<_> = self.tail.iter().cloned().collect();
        let running = crate::dynamic::expressions::running(self.id, cx).map(|e| e.replay_plan());
        let cached_plan = self.replay_plan.clone();
        let replay_key = self.replay_key.clone();
        let db = self.db.clone();
        let id = self.id;
        self._query = cx.spawn(async move |this, cx| {
            let (mut result, plan, checked_key, key_changed) = cx
                .background_executor()
                .spawn(async move {
                    let checked_key =
                        crate::dynamic::expressions::binding_text(&db, id).map(|text| {
                            (
                                text,
                                Arc::new(crate::dynamic::resolver::DbResolver::snapshot(&db)),
                            )
                        });
                    let key_changed = checked_key != replay_key;
                    let plan = running.or_else(|| {
                        if !key_changed {
                            cached_plan.unwrap_or_else(|| {
                                crate::dynamic::expressions::replay_plan_from_metadata(id, &db)
                            })
                        } else {
                            crate::dynamic::expressions::replay_plan_from_metadata(id, &db)
                        }
                    });
                    (
                        query_selected(&component, t, &tail, plan.as_ref()),
                        plan,
                        checked_key,
                        key_changed,
                    )
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                if crate::dynamic::expressions::binding_text(&this.db, this.id).as_ref()
                    != checked_key.as_ref().map(|key| &key.0)
                {
                    this.refresh(cx);
                    return;
                }
                if key_changed {
                    this.cache = None;
                }
                this.replay_key = checked_key;
                this.replay_plan = Some(plan);
                this.replay_checked = Instant::now();
                if !validate_plan
                    && this.cache.as_ref().is_some_and(|cache| {
                        super::view_time(cx).is_some_and(|t| cache.contains(t))
                            && this.component.as_ref().is_some_and(|c| {
                                c.time_series.manifest().generation == cache.manifest
                            })
                            && cache.dependencies_current()
                    })
                {
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
                if result.missing.as_ref().is_some_and(|(id, range)| {
                    *id == this.id
                        && result
                            .sample
                            .as_ref()
                            .is_some_and(|s| s.timestamp.0 >= range.end.0.saturating_sub(1))
                }) {
                    result.missing = None;
                }
                if let Some((id, range)) = result.missing.clone() {
                    this.retry_after = Instant::now() + Duration::from_millis(250);
                    if let Some(hydrator) = crate::hydration::hydrator(cx) {
                        hydrator.request(id, range);
                        this.publish(None, SampleStatus::Loading, Some(t), cx);
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
                    this.retry_after = Instant::now() + Duration::from_millis(250);
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
                this.cache = Some(result);
            });
        });
    }
}

struct QueryResult {
    sample: Option<SelectedSample>,
    next: Option<Timestamp>,
    missing: Option<(ComponentId, Range<Timestamp>)>,
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
    for sample in tail {
        result.take(sample.clone(), t);
    }
    let predecessor = result.sample.as_ref()?.timestamp;
    if manifest.spans.iter().rev().any(|span| {
        span.state != SpanState::Resident && span.seal.start_ts <= t && span.cover_end > predecessor
    }) {
        return None;
    }
    let next = manifest
        .spans
        .partition_point(|span| span.seal.start_ts <= t);
    if let Some(span) = manifest.spans.get(next) {
        result.next = Some(
            result
                .next
                .map_or(span.seal.start_ts, |t| t.min(span.seal.start_ts)),
        );
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
    result.missing = manifest
        .spans
        .iter()
        .rev()
        .find(|span| {
            span.state != SpanState::Resident
                && span.seal.start_ts <= t
                && predecessor.is_none_or(|s| span.cover_end > s)
        })
        .map(|span| {
            (
                component.component_id,
                span.seal.start_ts..Timestamp(span.cover_end.0.saturating_add(1)),
            )
        });
    for span in &manifest.spans {
        if span.seal.start_ts > t {
            result.next = Some(
                result
                    .next
                    .map_or(span.seal.start_ts, |next| next.min(span.seal.start_ts)),
            );
        }
    }
    result
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
    use metor_db::{ComponentSchema, manifest::SpanSource, seal::SealRecord};
    use metor_proto::types::PrimType;

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
        assert_eq!(inside.missing.unwrap().1, Timestamp(200)..Timestamp(300));
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
}
