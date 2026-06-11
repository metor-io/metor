//! Demand-driven envelope fetching, the wide-view sibling of
//! [`super::Hydrator`]: render loops call [`Envelopes::request`] every
//! frame — non-blocking, cached, newest-wins — and the [`Envelopes::run`]
//! task asks an [`EnvelopeSource`] for grid-quantized ranges. A year-wide
//! envelope is a few kilobytes, so the cache keeps exactly one entry per
//! (component, element) and refetching on view changes is cheap.

use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
    sync::Arc,
    time::Duration,
};

use metor_proto::types::{ComponentId, Timestamp};
pub use metor_proto_wkt::EnvelopeBin;
use stellarator::sync::WaitQueue;
use tracing::warn;

use crate::store::{EnvelopeSource, StoreError};

/// Pending fetches kept; like the hydrator queue, render loops re-request
/// visible ranges every frame, so dropped entries reappear immediately.
const QUEUE_CAP: usize = 32;
/// Pause after a failed fetch so per-frame re-requests cannot hammer a
/// down peer.
const RETRY_BACKOFF: Duration = Duration::from_millis(500);
/// Requested ranges snap outward to an eighth of their span, so per-pixel
/// panning reuses the cached envelope instead of refetching.
const GRID_DIVISIONS: i64 = 8;

/// One fetched envelope: the (quantized) range it answers and its bins.
#[derive(Debug)]
pub struct EnvelopeData {
    pub range: Range<Timestamp>,
    pub bins: Arc<[EnvelopeBin]>,
}

#[derive(Clone)]
pub struct Envelopes {
    inner: Arc<Inner>,
}

impl Default for Envelopes {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: std::sync::Mutex::new(State::default()),
                waker: WaitQueue::new(),
                repaint: Arc::new(WaitQueue::new()),
            }),
        }
    }
}

struct Inner {
    state: std::sync::Mutex<State>,
    waker: WaitQueue,
    /// Woken when new envelope data lands, so static plots repaint
    /// without waiting for their next live sample.
    repaint: Arc<WaitQueue>,
}

#[derive(Default)]
struct State {
    cache: HashMap<(ComponentId, u32), Entry>,
    queue: VecDeque<Pending>,
    /// The source answered [`StoreError::Unsupported`]; stop asking for
    /// the rest of the session.
    unsupported: bool,
}

#[derive(Default)]
struct Entry {
    data: Option<Arc<EnvelopeData>>,
    /// The most recently queued/fetched quantized range, deduping
    /// per-frame re-requests.
    requested: Option<Range<Timestamp>>,
}

struct Pending {
    component_id: ComponentId,
    element_index: u32,
    range: Range<Timestamp>,
    max_bins: u32,
}

impl Envelopes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Freshest cached envelope for the trace, queueing a refetch when
    /// the (grid-quantized) request doesn't match what the cache answers.
    /// Stale-while-revalidate: the old envelope keeps drawing until the
    /// new one lands. Never blocks.
    pub fn request(
        &self,
        component_id: ComponentId,
        element_index: u32,
        range: Range<Timestamp>,
        max_bins: u32,
    ) -> Option<Arc<EnvelopeData>> {
        let quantized = quantize(&range);
        let mut state = self.inner.state.lock().unwrap();
        if state.unsupported {
            return None;
        }
        let key = (component_id, element_index);
        let entry = state.cache.entry(key).or_default();
        let data = entry.data.clone();
        if entry.requested.as_ref() == Some(&quantized) {
            return data;
        }
        entry.requested = Some(quantized.clone());
        state
            .queue
            .retain(|p| (p.component_id, p.element_index) != key);
        if state.queue.len() >= QUEUE_CAP {
            state.queue.pop_front();
        }
        state.queue.push_back(Pending {
            component_id,
            element_index,
            range: quantized,
            max_bins,
        });
        drop(state);
        // wake() stores a wakeup for the consumer's check-then-wait gap;
        // see Hydrator::request.
        self.inner.waker.wake();
        data
    }

    /// Woken whenever a fetch lands; hook a repaint to it so envelope
    /// arrival is visible on plots with no live data flowing.
    pub fn waiter(&self) -> Arc<WaitQueue> {
        self.inner.repaint.clone()
    }

    /// Serve requests until the runtime shuts down. Spawn once per
    /// remote alongside the hydrator.
    pub async fn run(self, source: Arc<dyn EnvelopeSource>) {
        loop {
            let next = self.inner.state.lock().unwrap().queue.pop_front();
            let Some(pending) = next else {
                let _ = self.inner.waker.wait().await;
                continue;
            };
            let result = source
                .get_envelope(
                    pending.component_id,
                    pending.element_index,
                    pending.range.clone(),
                    pending.max_bins,
                )
                .await;
            let key = (pending.component_id, pending.element_index);
            let mut state = self.inner.state.lock().unwrap();
            match result {
                Ok(bins) => {
                    let entry = state.cache.entry(key).or_default();
                    entry.data = Some(Arc::new(EnvelopeData {
                        range: pending.range,
                        bins: bins.into(),
                    }));
                    drop(state);
                    self.inner.repaint.wake_all();
                }
                Err(StoreError::Unsupported) => {
                    state.unsupported = true;
                    state.queue.clear();
                }
                Err(err) => {
                    warn!(?err, component_id = ?pending.component_id, "envelope fetch failed");
                    // Forget the request so the next frame retries, but
                    // only after a pause — render loops re-request every
                    // frame and must not hammer a down peer.
                    if let Some(entry) = state.cache.get_mut(&key) {
                        entry.requested = None;
                    }
                    drop(state);
                    stellarator::sleep(RETRY_BACKOFF).await;
                }
            }
        }
    }
}

fn quantize(range: &Range<Timestamp>) -> Range<Timestamp> {
    let span = (range.end.0 - range.start.0).max(1);
    let step = (span / GRID_DIVISIONS).max(1);
    let start = range.start.0.div_euclid(step) * step;
    let end = range.end.0.div_euclid(step) * step + step;
    Timestamp(start)..Timestamp(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantize_is_stable_under_small_pans() {
        let a = quantize(&(Timestamp(1_000)..Timestamp(9_000)));
        let b = quantize(&(Timestamp(1_400)..Timestamp(9_400)));
        assert_eq!(a, b);
        let c = quantize(&(Timestamp(3_000)..Timestamp(11_000)));
        assert_ne!(a, c);
    }

    #[test]
    fn quantize_covers_the_request() {
        let q = quantize(&(Timestamp(-5_500)..Timestamp(2_500)));
        assert!(q.start.0 <= -5_500 && q.end.0 >= 2_500);
    }
}
