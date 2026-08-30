//! App-wide handle for computing history an expression never produced.
//!
//! The counterpart to [`hydration`](crate::hydration): where a hydrator
//! fetches history that exists elsewhere, the backfiller computes history
//! that exists nowhere, by replaying an expression's inputs through it (see
//! [`replay`](crate::dynamic::ops::replay)) and landing what comes out
//! behind the expression component's live head. A plot asks for the
//! uncovered stretches of its window every frame; the requests dedupe here,
//! and one thread serves them oldest-first in bounded jobs so a wide window
//! fills progressively rather than all at once.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gpui::Global;
use metor_db::manifest::{RangeVec, SpanSource};
use metor_db::{AtomicTimestampExt, Component, DB};
use metor_proto::types::{ComponentId, Timestamp};

use crate::dynamic::BuildError;
use crate::dynamic::ops::program::DEFAULT_FUEL;
use crate::dynamic::ops::replay::{ReplayPlan, replay};

/// Samples per installed node. Bounds the memory a job holds before it
/// lands anything, and the size of the stretch a plot sees appear at once.
pub const CHUNK_SAMPLES: usize = 65_536;
/// Frames one job emits before handing the rest back to the queue, so a
/// year-wide window never monopolises the thread.
pub const JOB_BUDGET: usize = 1_000_000;
/// Requests waiting; past this the oldest is dropped — the newest view wins.
const QUEUE_CAP: usize = 64;
/// How long a stretch the inputs could not fill stays out of the queue.
const EXHAUSTED_FOR: Duration = Duration::from_secs(30);

/// A stretch of one component to compute, and how.
struct Request {
    component: ComponentId,
    range: Range<Timestamp>,
    plan: ReplayPlan,
}

#[derive(Default)]
struct Queue {
    waiting: VecDeque<Request>,
    in_flight: Option<(ComponentId, Range<Timestamp>)>,
    /// Stretches whose inputs had nothing in them, with when to ask again.
    exhausted: HashMap<(ComponentId, Range<Timestamp>), Instant>,
}

struct Inner {
    db: Arc<DB>,
    queue: Mutex<Queue>,
    wake: Condvar,
}

/// The backfill service. Cheap to clone — a shared handle.
#[derive(Clone)]
pub struct Backfiller(Arc<Inner>);

impl Backfiller {
    /// Ask for `range` of `component` to be computed under `plan`. Never
    /// blocks, never allocates beyond the bounded queue; safe per frame.
    pub fn request(&self, component: ComponentId, range: Range<Timestamp>, plan: ReplayPlan) {
        if range.start >= range.end {
            return;
        }
        let mut queue = self.0.queue.lock().unwrap();
        let key = (component, range.clone());
        if queue.in_flight.as_ref() == Some(&key)
            || queue
                .waiting
                .iter()
                .any(|r| r.component == component && r.range == range)
        {
            return;
        }
        match queue.exhausted.get(&key) {
            Some(until) if *until > Instant::now() => return,
            Some(_) => {
                queue.exhausted.remove(&key);
            }
            None => {}
        }
        if queue.waiting.len() >= QUEUE_CAP {
            queue.waiting.pop_front();
        }
        queue.waiting.push_back(Request {
            component,
            range,
            plan,
        });
        drop(queue);
        self.0.wake.notify_one();
    }

    /// Start the service thread and install the handle.
    pub fn init(db: Arc<DB>, cx: &mut gpui::App) {
        let inner = Arc::new(Inner {
            db,
            queue: Mutex::new(Queue::default()),
            wake: Condvar::new(),
        });
        let served = inner.clone();
        std::thread::Builder::new()
            .name("backfill".into())
            .spawn(move || serve(served))
            .expect("spawn the backfill thread");
        cx.set_global(BackfillGlobal(Backfiller(inner)));
    }
}

struct BackfillGlobal(Backfiller);

impl Global for BackfillGlobal {}

/// The installed backfiller, if the app started one. `None` in contexts
/// (tests, tools) that show what exists and compute nothing.
pub fn backfiller(cx: &gpui::App) -> Option<Backfiller> {
    cx.try_global::<BackfillGlobal>().map(|b| b.0.clone())
}

fn serve(inner: Arc<Inner>) {
    loop {
        let request = {
            let mut queue = inner.queue.lock().unwrap();
            loop {
                if let Some(request) = queue.waiting.pop_front() {
                    queue.in_flight = Some((request.component, request.range.clone()));
                    break request;
                }
                queue = inner.wake.wait(queue).unwrap();
            }
        };
        let key = (request.component, request.range.clone());
        let emitted = match fill(&inner.db, request.component, request.range, &request.plan) {
            Ok(emitted) => emitted,
            Err(err) => {
                tracing::warn!(?err, component = ?request.component, "backfill failed");
                0
            }
        };
        let mut queue = inner.queue.lock().unwrap();
        queue.in_flight = None;
        if emitted == 0 {
            let now = Instant::now();
            queue.exhausted.retain(|_, until| *until > now);
            queue.exhausted.insert(key, now + EXHAUSTED_FOR);
        }
    }
}

/// The newest instant a backfill of `component` may write up to, exclusive.
///
/// Everything behind the live head is fair game and nothing at or past it
/// is: the writer owns that node and the install path refuses to outrank
/// it. An empty component has no head, so its ceiling is the newest sample
/// its driving input holds — anything the live system publishes later is
/// newer than that by construction.
pub fn ceiling(component: &Component, plan: &ReplayPlan) -> Option<Timestamp> {
    if let Some(head) = component.time_series.list.head() {
        return head.timestamps().first().copied();
    }
    let desc = &plan.compiled.manifest.systems[plan.system];
    match desc.rate {
        Some(_) => Some(Timestamp::now()),
        None => plan.ports[desc.driving.unwrap_or(0)]
            .time_series
            .latest()
            .map(|latest| latest.timestamp()),
    }
}

/// The stretches of `range` a backfill of `component` should compute now:
/// what nothing accounts for, clipped below the [`ceiling`], and only where
/// the driving input has a sample to fire on. A stretch between two
/// consecutive input samples is not history waiting to be computed — it is
/// where none will ever be — and asking for it would only ask again.
pub fn wanted(
    component: &Component,
    plan: &ReplayPlan,
    range: Range<Timestamp>,
    out: &mut RangeVec,
) {
    let Some(ceiling) = ceiling(component, plan) else {
        return;
    };
    let end = range.end.min(ceiling);
    if range.start >= end {
        return;
    }
    let desc = &plan.compiled.manifest.systems[plan.system];
    let driving = desc
        .rate
        .is_none()
        .then(|| &plan.ports[desc.driving.unwrap_or(0)]);
    let from = out.len();
    component.time_series.uncovered(range.start..end, out);
    if let Some(driving) = driving {
        let mut at = from;
        while at < out.len() {
            if has_sample(driving, &out[at]) {
                at += 1;
            } else {
                out.remove(at);
            }
        }
    }
}

/// Whether `component` holds at least one sample inside `range`.
fn has_sample(component: &Component, range: &Range<Timestamp>) -> bool {
    component.time_series.iter_node_slices().any(|node| {
        let timestamps = node.timestamps();
        let at = timestamps.partition_point(|t| t.0 < range.start.0);
        timestamps.get(at).is_some_and(|t| t.0 < range.end.0)
    })
}

/// Compute `range` of `component` under `plan`, landing chunks as it goes.
/// Returns how many frames were emitted; zero means the inputs had nothing
/// for this stretch.
pub fn fill(
    db: &DB,
    component: ComponentId,
    range: Range<Timestamp>,
    plan: &ReplayPlan,
) -> Result<usize, BuildError> {
    let Some(output) = db.with_state(|s| s.get_component(component).cloned()) else {
        return Ok(0);
    };
    // Coverage may have moved since the frame that asked, so the split is
    // recomputed here and the result, not the request, is what gets filled.
    let mut wanted_ranges = RangeVec::new();
    wanted(&output, plan, range, &mut wanted_ranges);

    let sizes: Vec<usize> = plan
        .outputs
        .iter()
        .map(|(field, _)| plan.field_schema(*field).size())
        .collect();
    let targets: Vec<Option<Component>> = plan
        .outputs
        .iter()
        .map(|(_, id)| db.with_state(|s| s.get_component(*id).cloned()))
        .collect();
    let mut chunks: Vec<Chunk> = sizes.iter().map(|_| Chunk::default()).collect();
    let mut emitted = 0;
    let mut scratch = Vec::new();

    for range in wanted_ranges {
        let stats = replay(plan, range, DEFAULT_FUEL, &mut |ts, frame| {
            emitted += 1;
            for (i, (field, _)) in plan.outputs.iter().enumerate() {
                plan.field(*field, frame, &mut scratch);
                // A chunk closes only at a timestamp change: the install
                // path's overlap test is inclusive, so two nodes may never
                // share an instant.
                if chunks[i].len() >= CHUNK_SAMPLES && chunks[i].last != Some(ts) {
                    land(db, &targets[i], sizes[i], &mut chunks[i]);
                }
                chunks[i].push(ts, &scratch);
            }
            emitted < JOB_BUDGET
        })?;
        for (i, chunk) in chunks.iter_mut().enumerate() {
            land(db, &targets[i], sizes[i], chunk);
        }
        if stats.stopped {
            break;
        }
    }
    Ok(emitted)
}

/// Samples waiting to become one node.
#[derive(Default)]
struct Chunk {
    timestamps: Vec<Timestamp>,
    data: Vec<u8>,
    last: Option<Timestamp>,
}

impl Chunk {
    fn len(&self) -> usize {
        self.timestamps.len()
    }

    fn push(&mut self, ts: Timestamp, bytes: &[u8]) {
        self.timestamps.push(ts);
        self.data.extend_from_slice(bytes);
        self.last = Some(ts);
    }

    fn samples(&self, size: usize) -> impl Iterator<Item = (Timestamp, &[u8])> {
        self.timestamps
            .iter()
            .zip(self.data.chunks_exact(size))
            .map(|(ts, bytes)| (*ts, bytes))
    }
}

/// Install a chunk and empty it. A refused install is not an error to
/// retry: it means the stretch was covered by someone else in the meantime,
/// and the next frame's split will say so.
fn land(db: &DB, target: &Option<Component>, size: usize, chunk: &mut Chunk) {
    if let Some(target) = target
        && chunk.len() > 0
    {
        match target
            .time_series
            .install_samples(size, chunk.samples(size), SpanSource::LocalIngest)
        {
            Ok(Some(seal)) => db.earliest_timestamp.update_min(seal.start_ts),
            Ok(None) => {}
            Err(err) => tracing::debug!(?err, "backfill chunk was not installed"),
        }
    }
    chunk.timestamps.clear();
    chunk.data.clear();
    chunk.last = None;
}
