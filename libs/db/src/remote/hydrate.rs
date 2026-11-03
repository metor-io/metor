use std::{
    collections::HashMap,
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use metor_proto::types::{ComponentId, Timestamp};
use stellarator::sync::WaitQueue;
use tracing::warn;

use super::range::{Completion, FetchError, Outcome, RangeKey};
use crate::{
    AtomicTimestampExt, ComponentSchema, DB, Error,
    manifest::{GapVec, SpanState},
    store::{NodeKey, NodeStaging, NodeStore, StoreError},
    time_series::TimeSeries,
};

/// Most whole-node requests waiting at once; past this the least important,
/// oldest one is dropped. Render loops re-request visible gaps every frame,
/// so dropped entries are re-discovered immediately.
const QUEUE_CAP: usize = 64;

/// [`QUEUE_CAP`] for range fetches and point reads. A dashboard scrub asks
/// one point read per instrument at once, and they are cheap, so they must
/// not push each other out.
pub(super) const RANGE_QUEUE_CAP: usize = 1024;

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

pub(super) struct Request<K> {
    pub key: K,
    priority: HydratePriority,
    /// Order of the latest ask; newer wins within a priority.
    seq: u64,
    asked: Instant,
    /// Where the outcome goes, once someone awaits it.
    pub done: Option<Arc<Completion>>,
}

impl<K> Request<K> {
    /// Someone still awaits the outcome, which keeps the request alive
    /// past [`REQUEST_TTL`]: an await is an ask that never stops.
    fn awaited(&self) -> bool {
        self.done.as_ref().is_some_and(|d| Arc::strong_count(d) > 1)
    }

    pub(super) fn finish(&self, outcome: Result<Outcome, FetchError>) {
        if let Some(done) = &self.done {
            done.finish(outcome);
        }
    }
}

/// The waiting requests: the most urgent, most recently asked first, and
/// anything not re-asked within [`REQUEST_TTL`] (nor awaited) forgotten.
/// Kept apart from the worker so the ordering is testable without a store.
pub(super) struct Queue<K> {
    waiting: Vec<Request<K>>,
    seq: u64,
    cap: usize,
}

impl<K> Default for Queue<K> {
    fn default() -> Self {
        Self::with_cap(QUEUE_CAP)
    }
}

impl<K> Queue<K> {
    pub(super) fn with_cap(cap: usize) -> Self {
        Self {
            waiting: Vec::new(),
            seq: 0,
            cap,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

impl<K: PartialEq> Queue<K> {
    /// Add or refresh a request. A repeat keeps one entry, taking the newer
    /// ask and the higher of the two priorities. With `await_it`, returns
    /// the completion the request's outcome will land in.
    ///
    /// A full queue makes room by dropping its least important request that
    /// nobody awaits; awaited ones are never dropped, so the queue grows
    /// past its cap only by what callers are awaiting.
    pub(super) fn push(
        &mut self,
        key: K,
        priority: HydratePriority,
        now: Instant,
        await_it: bool,
    ) -> Option<Arc<Completion>> {
        self.seq += 1;
        let seq = self.seq;
        let at = match self.waiting.iter().position(|r| r.key == key) {
            Some(at) => {
                let existing = &mut self.waiting[at];
                existing.priority = existing.priority.max(priority);
                existing.seq = seq;
                existing.asked = now;
                at
            }
            None => {
                if self.waiting.len() >= self.cap
                    && let Some(least) = self.least_important()
                {
                    self.waiting
                        .swap_remove(least)
                        .finish(Err(FetchError::Expired));
                }
                self.waiting.push(Request {
                    key,
                    priority,
                    seq,
                    asked: now,
                    done: None,
                });
                self.waiting.len() - 1
            }
        };
        await_it.then(|| {
            self.waiting[at]
                .done
                .get_or_insert_with(|| Arc::new(Completion::default()))
                .clone()
        })
    }

    /// Drop what expired: not asked again within [`REQUEST_TTL`] and not
    /// awaited.
    fn expire(&mut self, now: Instant) {
        self.waiting.retain(|r| {
            let alive = now.saturating_duration_since(r.asked) < REQUEST_TTL || r.awaited();
            if !alive {
                r.finish(Err(FetchError::Expired));
            }
            alive
        });
    }

    /// Take the request to serve next, dropping any that expired.
    pub(super) fn pop(&mut self, now: Instant) -> Option<Request<K>> {
        self.expire(now);
        let next = self
            .waiting
            .iter()
            .enumerate()
            .max_by_key(|(_, r)| (r.priority, r.seq))
            .map(|(i, _)| i)?;
        Some(self.waiting.swap_remove(next))
    }

    /// Take up to `max` requests, most urgent first, stopping before the
    /// one that would take their summed `weight` past `budget` (the first is
    /// always taken).
    pub(super) fn pop_batch(
        &mut self,
        now: Instant,
        max: usize,
        budget: usize,
        weight: impl Fn(&K) -> usize,
    ) -> Vec<Request<K>> {
        self.expire(now);
        self.waiting
            .sort_unstable_by_key(|r| std::cmp::Reverse((r.priority, r.seq)));
        let mut total = 0usize;
        let mut take = 0;
        for request in self.waiting.iter().take(max) {
            total = total.saturating_add(weight(&request.key));
            if take > 0 && total > budget {
                break;
            }
            take += 1;
        }
        self.waiting.drain(..take).collect()
    }

    fn least_important(&self) -> Option<usize> {
        self.waiting
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.awaited())
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
    pub(super) inner: Arc<HydratorInner>,
}

impl Default for Hydrator {
    fn default() -> Self {
        Self::new()
    }
}

pub(super) struct HydratorInner {
    queue: std::sync::Mutex<Queue<(ComponentId, Range<Timestamp>)>>,
    backoff: std::sync::Mutex<HashMap<(ComponentId, Timestamp), Backoff>>,
    waker: WaitQueue,
    /// Range fetches and point reads, served apart from whole nodes so
    /// neither waits on the other (see [`super::range`]).
    pub(super) ranges: std::sync::Mutex<Queue<RangeKey>>,
    pub(super) range_waker: WaitQueue,
    /// When each component's manifest was last re-listed for a range read
    /// whose answer no span covered (see [`super::range`]).
    pub(super) reseeded: std::sync::Mutex<HashMap<ComponentId, Instant>>,
}

impl Hydrator {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HydratorInner {
                queue: std::sync::Mutex::new(Queue::default()),
                backoff: std::sync::Mutex::new(HashMap::new()),
                waker: WaitQueue::new(),
                ranges: std::sync::Mutex::new(Queue::with_cap(RANGE_QUEUE_CAP)),
                range_waker: WaitQueue::new(),
                reseeded: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Whether `key` is still inside its retry backoff window.
    pub(super) fn backed_off(&self, key: (ComponentId, Timestamp)) -> bool {
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
    pub(super) fn record_failure(&self, key: (ComponentId, Timestamp)) {
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
    pub(super) fn clear_backoff(&self, key: (ComponentId, Timestamp)) {
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
        self.inner.queue.lock().unwrap().push(
            (component_id, range),
            priority,
            Instant::now(),
            false,
        );
        // wake() stores a wakeup if it lands while run() is between its
        // queue pop and wait(); one stored wakeup suffices because the
        // single consumer drains the whole queue before re-waiting.
        self.inner.waker.wake();
    }

    /// Whether requests are waiting behind the one being served, whole
    /// nodes or range fetches — the signal for callers that only re-request
    /// when they repaint to repaint.
    pub fn backlogged(&self) -> bool {
        !self.inner.queue.lock().unwrap().is_empty()
            || !self.inner.ranges.lock().unwrap().is_empty()
    }

    /// Serve requests until the runtime shuts down. Spawn once per DB
    /// with the store hydration should pull from. Whole nodes and range
    /// fetches are served side by side, so a point read never waits for a
    /// node download to finish.
    pub async fn run(self, db: Arc<DB>, store: Arc<dyn NodeStore>) {
        let nodes = async {
            loop {
                let next = self.inner.queue.lock().unwrap().pop(Instant::now());
                let Some(Request {
                    key: (component_id, range),
                    ..
                }) = next
                else {
                    let _ = self.inner.waker.wait().await;
                    continue;
                };
                if let Err(err) = self.serve(&db, store.as_ref(), component_id, range).await {
                    warn!(?err, ?component_id, "hydration request failed");
                }
            }
        };
        futures_lite::future::zip(nodes, self.serve_ranges(&db, store.as_ref())).await;
    }

    pub(super) async fn serve(
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
            // Keyed by seal: a seal's pieces, and so their keys, move as
            // its coverage is re-derived, but a failing download is the
            // seal's.
            let key = (component_id, gap.seal_start);
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

    type Key = (ComponentId, Range<Timestamp>);

    fn drain(queue: &mut Queue<Key>, now: Instant) -> Vec<Key> {
        std::iter::from_fn(|| queue.pop(now).map(|r| r.key)).collect()
    }

    fn push(
        queue: &mut Queue<Key>,
        id: ComponentId,
        range: Range<Timestamp>,
        p: HydratePriority,
        now: Instant,
    ) {
        queue.push((id, range), p, now, false);
    }

    #[test]
    fn serves_most_urgent_then_newest() {
        let mut queue = Queue::default();
        let now = Instant::now();
        let id = ComponentId(1);
        push(&mut queue, id, range(0), HydratePriority::Prefetch, now);
        push(&mut queue, id, range(10), HydratePriority::Visible, now);
        push(&mut queue, id, range(20), HydratePriority::Selected, now);
        push(&mut queue, id, range(30), HydratePriority::Visible, now);
        push(&mut queue, id, range(40), HydratePriority::Selected, now);
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
        push(&mut queue, id, range(0), HydratePriority::Visible, now);
        push(&mut queue, id, range(10), HydratePriority::Visible, now);
        // Asked again: newest within its priority, and never lowered.
        push(&mut queue, id, range(0), HydratePriority::Visible, now);
        assert_eq!(queue.waiting.len(), 2);
        assert_eq!(queue.pop(now).map(|r| r.key.1.start.0), Some(0));
        push(&mut queue, id, range(20), HydratePriority::Selected, now);
        push(&mut queue, id, range(20), HydratePriority::Prefetch, now);
        push(&mut queue, id, range(30), HydratePriority::Visible, now);
        assert_eq!(queue.pop(now).map(|r| r.key.1.start.0), Some(20));
    }

    #[test]
    fn unrepeated_requests_expire() {
        let mut queue = Queue::default();
        let start = Instant::now();
        let id = ComponentId(1);
        push(&mut queue, id, range(0), HydratePriority::Selected, start);
        push(&mut queue, id, range(10), HydratePriority::Visible, start);
        let later = start + REQUEST_TTL / 2;
        push(&mut queue, id, range(10), HydratePriority::Visible, later);
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
        push(&mut queue, id, range(-10), HydratePriority::Prefetch, now);
        for i in 0..QUEUE_CAP as i64 - 1 {
            push(&mut queue, id, range(i * 10), HydratePriority::Visible, now);
        }
        push(
            &mut queue,
            id,
            range(10_000),
            HydratePriority::Selected,
            now,
        );
        assert_eq!(queue.waiting.len(), QUEUE_CAP);
        let served = drain(&mut queue, now);
        assert_eq!(served.first().map(|(_, r)| r.start.0), Some(10_000));
        assert!(served.iter().all(|(_, r)| r.start.0 != -10));
    }

    /// An awaited request outlives the TTL for as long as it is awaited,
    /// and an expired one tells whoever still holds its completion.
    #[test]
    fn awaited_requests_live_until_dropped() {
        let mut queue = Queue::default();
        let start = Instant::now();
        let id = ComponentId(1);
        let held = queue
            .push((id, range(0)), HydratePriority::Selected, start, true)
            .unwrap();
        let dropped = queue
            .push((id, range(10)), HydratePriority::Selected, start, true)
            .unwrap();
        let expired = start + REQUEST_TTL * 2;
        queue.expire(expired);
        assert_eq!(queue.waiting.len(), 2, "both still awaited");
        drop(dropped);
        queue.expire(expired);
        assert_eq!(queue.waiting.len(), 1);
        assert_eq!(drain(&mut queue, expired), [(id, range(0))]);
        drop(held);
    }

    #[test]
    fn a_request_whose_awaiter_left_can_be_pushed_out() {
        let mut queue = Queue::default();
        let now = Instant::now();
        let id = ComponentId(1);
        let least = queue
            .push((id, range(-10)), HydratePriority::Prefetch, now, true)
            .unwrap();
        drop(least);
        for i in 0..QUEUE_CAP as i64 {
            queue.push((id, range(i * 10)), HydratePriority::Visible, now, false);
        }
        assert_eq!(queue.waiting.len(), QUEUE_CAP);
        assert!(queue.waiting.iter().all(|r| r.key != (id, range(-10))));
    }

    /// Point reads someone awaits are never pushed out by more of them: the
    /// queue drops what nobody awaits first, and otherwise grows.
    #[test]
    fn awaited_requests_are_never_pushed_out() {
        let mut queue = Queue::default();
        let now = Instant::now();
        let id = ComponentId(1);
        let unawaited = (id, range(-10));
        queue.push(unawaited.clone(), HydratePriority::Selected, now, false);
        let awaited: Vec<_> = (0..QUEUE_CAP as i64 + 10)
            .map(|i| {
                queue
                    .push((id, range(i * 10)), HydratePriority::Selected, now, true)
                    .unwrap()
            })
            .collect();
        assert_eq!(queue.waiting.len(), QUEUE_CAP + 10);
        assert!(queue.waiting.iter().all(|r| r.key != unawaited));
        assert!(awaited.iter().all(|done| done.outcome().is_none()));
    }

    #[test]
    fn batches_stop_at_their_byte_budget() {
        let mut queue = Queue::default();
        let now = Instant::now();
        let id = ComponentId(1);
        for i in 0..10 {
            push(&mut queue, id, range(i * 10), HydratePriority::Visible, now);
        }
        let weight = |key: &Key| if key.1.start.0 < 50 { 10 } else { 1 };
        // Newest first: 90 down to 50 weigh 1 each, then 40 tips past 7.
        assert_eq!(queue.pop_batch(now, 64, 7, weight).len(), 5);
        // One heavier than the budget still goes, alone.
        assert_eq!(queue.pop_batch(now, 64, 5, weight).len(), 1);
        assert_eq!(queue.pop_batch(now, 2, 1000, weight).len(), 2);
    }
}
