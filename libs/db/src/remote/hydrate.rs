use std::{collections::VecDeque, ops::Range, sync::Arc};

use metor_proto::{
    schema::Schema,
    types::{ComponentId, Timestamp},
};
use stellarator::sync::WaitQueue;
use tracing::warn;

use crate::{
    AtomicTimestampExt, DB, Error,
    manifest::{GapVec, SpanSource, SpanState},
    store::{NodeKey, NodeStaging, NodeStore, StoreError},
    time_series_2::TimeSeries,
};

/// Most recent gap requests we keep; render loops re-request visible gaps
/// every frame, so dropped entries are re-discovered immediately.
const QUEUE_CAP: usize = 64;

/// Fetch one remote-only span into residency. Returns `Ok(false)` when
/// there is nothing to do — the span is unknown, already resident, or
/// another fetch holds the claim.
pub async fn hydrate_span(
    time_series: &TimeSeries,
    store: &dyn NodeStore,
    component_id: ComponentId,
    component_name: &str,
    schema: &Schema<Vec<u64>>,
    start_ts: Timestamp,
) -> Result<bool, Error> {
    let Some(seal) = time_series.begin_fetch(start_ts) else {
        return Ok(false);
    };
    let result: Result<(), Error> = async {
        let staging = NodeStaging::create(time_series.path(), &seal)?;
        let key = NodeKey {
            component_id,
            component_name,
            schema,
            start_ts,
            checksum: seal.checksum,
        };
        let fetched = store.get(key, &staging).await?;
        if fetched.checksum != seal.checksum {
            return Err(StoreError::ChecksumMismatch.into());
        }
        time_series.install_node(staging, &seal, SpanSource::RemoteFetch)
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
    waker: WaitQueue,
}

impl Hydrator {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HydratorInner {
                queue: std::sync::Mutex::new(VecDeque::new()),
                waker: WaitQueue::new(),
            }),
        }
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
        let schema = component.schema.to_schema();
        let mut gaps = GapVec::new();
        component.time_series.coverage(range, &mut gaps);
        for gap in gaps {
            if gap.state != SpanState::RemoteOnly {
                continue;
            }
            if hydrate_span(
                &component.time_series,
                store,
                component_id,
                &name,
                &schema,
                gap.start_ts,
            )
            .await?
            {
                db.earliest_timestamp.update_min(gap.start_ts);
            }
        }
        Ok(())
    }
}
