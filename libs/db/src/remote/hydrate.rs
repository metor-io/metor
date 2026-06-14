use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    sync::Arc,
    time::{Duration, Instant},
};

use metor_proto::types::{ComponentId, Timestamp};
use stellarator::sync::WaitQueue;
use tracing::warn;

use crate::{
    AtomicTimestampExt, ComponentSchema, DB, Error,
    manifest::{GapVec, SpanSource, SpanState},
    store::{NodeKey, NodeStaging, NodeStore, StoreError},
    time_series_2::TimeSeries,
};

/// Most recent gap requests we keep; render loops re-request visible gaps
/// every frame, so dropped entries are re-discovered immediately.
const QUEUE_CAP: usize = 64;

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

/// Fetch one remote-only span into residency. Returns `Ok(false)` when
/// there is nothing to do — the span is unknown, already resident, or
/// another fetch holds the claim.
pub async fn hydrate_span(
    time_series: &TimeSeries,
    store: &dyn NodeStore,
    component_id: ComponentId,
    component_name: &str,
    schema: &ComponentSchema,
    start_ts: Timestamp,
) -> Result<bool, Error> {
    let Some(span) = time_series.begin_fetch(start_ts) else {
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
            start_ts,
            checksum: seal.checksum,
        };
        let fetched = store.get(key, &staging).await?;
        if fetched.checksum != seal.checksum {
            return Err(StoreError::ChecksumMismatch.into());
        }
        // A coverage-trimmed span only installs the prefix this side is
        // missing; the rest of the node already exists as resident data.
        let install_seal = if span.cover_end.0 < seal.end_ts.0 {
            staging.trim_to(span.cover_end)?
        } else {
            seal
        };
        time_series.install_node(staging, &install_seal, SpanSource::RemoteFetch)
    }
    .await;
    match result {
        Ok(()) => Ok(true),
        Err(err) => {
            time_series.abort_fetch(start_ts);
            Err(err)
        }
    }
}

/// Demand-driven fetcher for remote-only history. Frame loops call
/// [`Self::request`] — non-blocking, lossy, newest-wins — and the
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
    queue: std::sync::Mutex<VecDeque<(ComponentId, Range<Timestamp>)>>,
    backoff: std::sync::Mutex<HashMap<(ComponentId, Timestamp), Backoff>>,
    waker: WaitQueue,
}

impl Hydrator {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HydratorInner {
                queue: std::sync::Mutex::new(VecDeque::new()),
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
        let delay = BACKOFF_BASE
            .saturating_mul(1u32 << shift)
            .min(BACKOFF_CAP);
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
    /// and never allocates beyond the bounded queue; when the queue is
    /// full the oldest request is dropped — the newest view wins.
    pub fn request(&self, component_id: ComponentId, range: Range<Timestamp>) {
        let mut queue = self.inner.queue.lock().unwrap();
        if queue.iter().any(|(id, r)| *id == component_id && *r == range) {
            return;
        }
        if queue.len() >= QUEUE_CAP {
            queue.pop_front();
        }
        queue.push_back((component_id, range));
        drop(queue);
        // wake() stores a wakeup if it lands while run() is between its
        // queue pop and wait(); one stored wakeup suffices because the
        // single consumer drains the whole queue before re-waiting.
        self.inner.waker.wake();
    }

    /// Serve requests until the runtime shuts down. Spawn once per DB
    /// with the store hydration should pull from.
    pub async fn run(self, db: Arc<DB>, store: Arc<dyn NodeStore>) {
        loop {
            let next = self.inner.queue.lock().unwrap().pop_front();
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
