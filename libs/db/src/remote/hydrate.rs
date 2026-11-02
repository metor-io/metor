use std::{
    collections::HashMap,
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use metor_proto::types::{ComponentId, Timestamp};
use stellarator::sync::WaitQueue;
use tracing::warn;

use crate::{
    AtomicTimestampExt, ComponentSchema, DB, Error,
    manifest::{GapVec, SpanState},
    store::{NodeKey, NodeStaging, NodeStore, StoreError},
    time_series::TimeSeries,
};

/// Most requests waiting at once; past this the least important, oldest one
/// is dropped. Render loops re-request visible gaps every frame, so dropped
/// entries are re-discovered immediately.
const QUEUE_CAP: usize = 64;

/// How long a waiting request lives without being asked for again. Views
/// re-request what they still need every frame, so a request that has gone
/// quiet this long is for a position the user has left — serving it would
/// only delay the one they moved to.
pub const REQUEST_TTL: Duration = Duration::from_secs(1);

/// First retry delay after a span fails to hydrate; doubles per attempt.
const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Ceiling on the retry delay. A span never gives up permanently — state
/// can change (the peer comes back, the span becomes installable) — so the
/// backoff caps rather than abandons.
const BACKOFF_CAP: Duration = Duration::from_secs(60);
/// Soft bound on the backoff memo; pruned past this. Render loops only ever
/// request visible gaps, so the live working set is tiny.
const MEMO_CAP: usize = 256;

/// Per-span retry state: a deterministically-failing span (corrupt remote
/// bytes, a never-installable peer span) would otherwise be re-downloaded
/// every frame forever, since the panel re-requests the same gap each frame.
struct Backoff {
    attempts: u32,
    retry_at: Instant,
}

/// Fetch one remote-only span, keyed by its `cover_start`, into
/// residency. The download is the span's whole seal, so every other piece
/// of that seal lands with it. Returns `Ok(false)` when there is nothing
/// to do — the span is unknown, already resident, or another fetch holds
/// the claim.
pub async fn hydrate_span(
    time_series: &TimeSeries,
    store: &dyn NodeStore,
    component_id: ComponentId,
    component_name: &str,
    schema: &ComponentSchema,
    cover_start: Timestamp,
) -> Result<bool, Error> {
    let Some(span) = time_series.begin_fetch(cover_start) else {
        return Ok(false);
    };
    let seal = span.seal;
    let result: Result<(), Error> = async {
        let staging = NodeStaging::create(time_series.path(), &seal)?;
        let wire_schema = schema.to_schema();
        let key = NodeKey {
            component_id,
            component_name,
            schema: &wire_schema,
            start_ts: seal.start_ts,
            checksum: seal.checksum,
        };
        let fetched = store.get(key, &staging).await?;
        if fetched.checksum != seal.checksum {
            return Err(StoreError::ChecksumMismatch.into());
        }
        time_series.install_fetched(staging, &seal)
    }
    .await;
    match result {
        Ok(()) => Ok(true),
        Err(err) => {
            time_series.abort_fetch(cover_start);
            Err(err)
        }
    }
}

/// How urgently a view needs a range, highest first. The worker always
/// serves the most urgent waiting request, so a scrubbed value never queues
/// behind a plot window or read-ahead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HydratePriority {
    /// Read-ahead for playback: wanted soon, not shown yet.
    Prefetch,
    /// A window on screen: plot ranges and their gap bands.
    Visible,
    /// The instant a value view shows right now.
    Selected,
}

struct Request {
    component_id: ComponentId,
    range: Range<Timestamp>,
    priority: HydratePriority,
    /// Order of the latest ask; newer wins within a priority.
    seq: u64,
    asked: Instant,
}

/// The waiting requests: the most urgent, most recently asked first, and
/// anything not re-asked within [`REQUEST_TTL`] forgotten. Kept apart from
/// the worker so the ordering is testable without a store.
#[derive(Default)]
struct Queue {
    waiting: Vec<Request>,
    seq: u64,
}

impl Queue {
    /// Add or refresh a request. A repeat keeps one entry, taking the newer
    /// ask and the higher of the two priorities.
    fn push(
        &mut self,
        component_id: ComponentId,
        range: Range<Timestamp>,
        priority: HydratePriority,
        now: Instant,
    ) {
        self.seq += 1;
        let seq = self.seq;
        if let Some(existing) = self
            .waiting
            .iter_mut()
            .find(|r| r.component_id == component_id && r.range == range)
        {
            existing.priority = existing.priority.max(priority);
            existing.seq = seq;
            existing.asked = now;
            return;
        }
        if self.waiting.len() >= QUEUE_CAP
            && let Some(least) = self.least_important()
        {
            self.waiting.swap_remove(least);
        }
        self.waiting.push(Request {
            component_id,
            range,
            priority,
            seq,
            asked: now,
        });
    }

    /// Take the request to serve next, dropping any that expired.
    fn pop(&mut self, now: Instant) -> Option<(ComponentId, Range<Timestamp>)> {
        self.waiting
            .retain(|r| now.saturating_duration_since(r.asked) < REQUEST_TTL);
        let next = self
            .waiting
            .iter()
            .enumerate()
            .max_by_key(|(_, r)| (r.priority, r.seq))
            .map(|(i, _)| i)?;
        let request = self.waiting.swap_remove(next);
        Some((request.component_id, request.range))
    }

    fn least_important(&self) -> Option<usize> {
        self.waiting
            .iter()
            .enumerate()
            .min_by_key(|(_, r)| (r.priority, r.seq))
            .map(|(i, _)| i)
    }
}

/// Demand-driven fetcher for remote-only history. Frame loops call
/// [`Self::request`] — non-blocking, lossy, most urgent and newest first,
/// forgetting what stops being asked for — and the
/// [`Self::run`] task downloads, verifies, and installs nodes, waking the
/// component's data waker so views repaint as spans land.
#[derive(Clone)]
pub struct Hydrator {
    inner: Arc<HydratorInner>,
}

impl Default for Hydrator {
    fn default() -> Self {
        Self::new()
    }
}

struct HydratorInner {
    queue: std::sync::Mutex<Queue>,
    backoff: std::sync::Mutex<HashMap<(ComponentId, Timestamp), Backoff>>,
    waker: WaitQueue,
}

impl Hydrator {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HydratorInner {
                queue: std::sync::Mutex::new(Queue::default()),
                backoff: std::sync::Mutex::new(HashMap::new()),
                waker: WaitQueue::new(),
            }),
        }
    }

    /// Whether `key` is still inside its retry backoff window.
    fn backed_off(&self, key: (ComponentId, Timestamp)) -> bool {
        self.inner
            .backoff
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|b| b.retry_at > Instant::now())
    }

    /// Record a failed hydration: bump the attempt count and push
    /// `retry_at` out by exponential backoff (capped). Prunes elapsed
    /// entries when the memo grows past [`MEMO_CAP`].
    fn record_failure(&self, key: (ComponentId, Timestamp)) {
        let mut memo = self.inner.backoff.lock().unwrap();
        let now = Instant::now();
        let entry = memo.entry(key).or_insert(Backoff {
            attempts: 0,
            retry_at: now,
        });
        entry.attempts = entry.attempts.saturating_add(1);
        let shift = entry.attempts.saturating_sub(1).min(31);
        let delay = BACKOFF_BASE.saturating_mul(1u32 << shift).min(BACKOFF_CAP);
        entry.retry_at = now + delay;
        if memo.len() > MEMO_CAP {
            memo.retain(|_, b| b.retry_at > now);
        }
    }

    /// Clear a span's backoff once it hydrates (or no longer needs to).
    fn clear_backoff(&self, key: (ComponentId, Timestamp)) {
        self.inner.backoff.lock().unwrap().remove(&key);
    }

    /// Ask for `range` of `component_id` to become resident. Never blocks
    /// and never allocates beyond the bounded queue. A request lives for
    /// [`REQUEST_TTL`] unless asked again, so callers repeat it for as long
    /// as they still need it.
    pub fn request(
        &self,
        component_id: ComponentId,
        range: Range<Timestamp>,
        priority: HydratePriority,
    ) {
        self.inner
            .queue
            .lock()
            .unwrap()
            .push(component_id, range, priority, Instant::now());
        // wake() stores a wakeup if it lands while run() is between its
        // queue pop and wait(); one stored wakeup suffices because the
        // single consumer drains the whole queue before re-waiting.
        self.inner.waker.wake();
    }

    /// Whether requests are waiting behind the one being served — the
    /// signal for callers that only re-request when they repaint to repaint.
    pub fn backlogged(&self) -> bool {
        !self.inner.queue.lock().unwrap().waiting.is_empty()
    }

    /// Serve requests until the runtime shuts down. Spawn once per DB
    /// with the store hydration should pull from.
    pub async fn run(self, db: Arc<DB>, store: Arc<dyn NodeStore>) {
        loop {
            let next = self.inner.queue.lock().unwrap().pop(Instant::now());
            let Some((component_id, range)) = next else {
                let _ = self.inner.waker.wait().await;
                continue;
            };
            if let Err(err) = self.serve(&db, store.as_ref(), component_id, range).await {
                warn!(?err, ?component_id, "hydration request failed");
            }
        }
    }

    async fn serve(
        &self,
        db: &Arc<DB>,
        store: &dyn NodeStore,
        component_id: ComponentId,
        range: Range<Timestamp>,
    ) -> Result<(), Error> {
        let Some((component, name)) = db.with_state(|state| {
            let component = state.components.get(&component_id)?.clone();
            let name = state
                .get_component_metadata(component_id)
                .map(|m| m.name.clone())
                .unwrap_or_else(|| component_id.to_string());
            Some((component, name))
        }) else {
            return Ok(());
        };
        let mut gaps = GapVec::new();
        component.time_series.coverage(range, &mut gaps);
        for gap in gaps {
            if gap.state != SpanState::RemoteOnly {
                continue;
            }
            let key = (component_id, gap.start_ts);
            if self.backed_off(key) {
                continue;
            }
            // A per-gap failure no longer aborts the whole request: it
            // records backoff and moves on, so one bad span can't starve
            // the others sharing this range.
            match hydrate_span(
                &component.time_series,
                store,
                component_id,
                &name,
                &component.schema,
                gap.start_ts,
            )
            .await
            {
                Ok(installed) => {
                    self.clear_backoff(key);
                    if installed {
                        db.earliest_timestamp.update_min(gap.start_ts);
                    }
                }
                Err(err) => {
                    self.record_failure(key);
                    warn!(
                        ?err,
                        ?component_id,
                        start_ts = gap.start_ts.0,
                        "hydrate span failed; backing off"
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: i64) -> Range<Timestamp> {
        Timestamp(start)..Timestamp(start + 10)
    }

    fn drain(queue: &mut Queue, now: Instant) -> Vec<(ComponentId, Range<Timestamp>)> {
        std::iter::from_fn(|| queue.pop(now)).collect()
    }

    #[test]
    fn serves_most_urgent_then_newest() {
        let mut queue = Queue::default();
        let now = Instant::now();
        let id = ComponentId(1);
        queue.push(id, range(0), HydratePriority::Prefetch, now);
        queue.push(id, range(10), HydratePriority::Visible, now);
        queue.push(id, range(20), HydratePriority::Selected, now);
        queue.push(id, range(30), HydratePriority::Visible, now);
        queue.push(id, range(40), HydratePriority::Selected, now);
        let order: Vec<_> = drain(&mut queue, now)
            .into_iter()
            .map(|(_, r)| r.start.0)
            .collect();
        assert_eq!(order, [40, 20, 30, 10, 0]);
    }

    #[test]
    fn repeat_refreshes_and_raises() {
        let mut queue = Queue::default();
        let now = Instant::now();
        let id = ComponentId(1);
        queue.push(id, range(0), HydratePriority::Visible, now);
        queue.push(id, range(10), HydratePriority::Visible, now);
        // Asked again: newest within its priority, and never lowered.
        queue.push(id, range(0), HydratePriority::Visible, now);
        assert_eq!(queue.waiting.len(), 2);
        assert_eq!(queue.pop(now).map(|(_, r)| r.start.0), Some(0));
        queue.push(id, range(20), HydratePriority::Selected, now);
        queue.push(id, range(20), HydratePriority::Prefetch, now);
        queue.push(id, range(30), HydratePriority::Visible, now);
        assert_eq!(queue.pop(now).map(|(_, r)| r.start.0), Some(20));
    }

    #[test]
    fn unrepeated_requests_expire() {
        let mut queue = Queue::default();
        let start = Instant::now();
        let id = ComponentId(1);
        queue.push(id, range(0), HydratePriority::Selected, start);
        queue.push(id, range(10), HydratePriority::Visible, start);
        let later = start + REQUEST_TTL / 2;
        queue.push(id, range(10), HydratePriority::Visible, later);
        // range(0) went quiet a full TTL ago; range(10) was asked again.
        let expired = start + REQUEST_TTL;
        assert_eq!(
            drain(&mut queue, expired),
            [(id, range(10))],
            "only the refreshed request survives"
        );
    }

    #[test]
    fn full_queue_drops_least_important() {
        let mut queue = Queue::default();
        let now = Instant::now();
        let id = ComponentId(1);
        queue.push(id, range(-10), HydratePriority::Prefetch, now);
        for i in 0..QUEUE_CAP as i64 - 1 {
            queue.push(id, range(i * 10), HydratePriority::Visible, now);
        }
        queue.push(id, range(10_000), HydratePriority::Selected, now);
        assert_eq!(queue.waiting.len(), QUEUE_CAP);
        let served = drain(&mut queue, now);
        assert_eq!(served.first().map(|(_, r)| r.start.0), Some(10_000));
        assert!(served.iter().all(|(_, r)| r.start.0 != -10));
    }
}
