//! Range fetches and point reads through the [`Hydrator`]: the samples a
//! view needs instead of the whole sealed nodes holding them.
//!
//! Requests queue like whole-node ones, with the same priorities and
//! [`REQUEST_TTL`](super::REQUEST_TTL) expiry, plus an awaitable outcome.
//! A worker takes the most urgent batch at once and sends it down the
//! store's range connection in one round trip ([`NodeStore::time_series`]),
//! so a dashboard scrubbed to a new instant costs one round trip for all
//! its instruments. Replies install as fragments
//! ([`TimeSeries::install_fragment`]), which every local read sees.
//!
//! What is already answered locally never goes out. A peer too old to say
//! what a reply covers gets whole-node hydration instead, of the spans
//! nearest the requested end.

use std::{
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use metor_proto::types::{ComponentId, Timestamp};
use metor_proto_wkt::{GetTimeSeries, SeriesEnd};
use stellarator::sync::WaitQueue;
use tracing::warn;

use crate::{
    AtomicTimestampExt, Component, DB,
    manifest::{GapVec, SpanState},
    series::{Flat, held},
    store::{MAX_BATCH, NodeStore, SeriesReply, StoreError},
    time_series::TimeSeries,
};

use super::{
    HydratePriority, Hydrator,
    hydrate::{Request, hydrate_span},
};

/// Most bytes one range fetch asks for: a limit sized to a fragment. A
/// wider range is paged by asking again for what is still missing.
pub const PAGE_BYTES: usize = 4 * 1024 * 1024;

/// About how many reply bytes one batch asks for, so a batch of wide range
/// reads stays a few pages while point reads still go out together.
const BATCH_BYTES: usize = 4 * PAGE_BYTES;

/// Sample size a limited read is assumed to have when weighing a batch.
const ESTIMATED_SAMPLE: usize = 64;

/// First wait before redialing a peer the range connection could not
/// reach; doubles per failed batch up to [`DIAL_BACKOFF_CAP`].
const DIAL_BACKOFF_BASE: Duration = Duration::from_millis(250);
const DIAL_BACKOFF_CAP: Duration = Duration::from_secs(5);

/// Least time between re-listing one component's remote spans for reads
/// whose answer no span covered.
const RESEED_AFTER: Duration = Duration::from_secs(5);
/// Soft bound on the re-listing memo; pruned past this.
const RESEED_MEMO_CAP: usize = 256;

/// Whole-node hydrations a fallback read tries before giving up: each
/// lands the span nearest the requested end, and an empty one moves on to
/// the next.
const FALLBACK_SPANS: usize = 4;

/// A stretch of one component's history, as a range fetch asks for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RangeQuery {
    /// End exclusive.
    pub range: Range<Timestamp>,
    pub from: SeriesEnd,
    /// At most this many samples from `from`'s end. One fetch also stops
    /// at [`PAGE_BYTES`].
    pub limit: Option<usize>,
}

impl RangeQuery {
    /// The oldest samples of `range`.
    pub fn range(range: Range<Timestamp>) -> Self {
        Self {
            range,
            from: SeriesEnd::Oldest,
            limit: None,
        }
    }

    /// The sample a reader shows at `t`: the newest at or before it.
    pub fn instant(t: Timestamp) -> Self {
        Self {
            range: Timestamp(i64::MIN)..Timestamp(t.0.saturating_add(1)),
            from: SeriesEnd::Newest,
            limit: Some(1),
        }
    }

    pub fn newest(self) -> Self {
        Self {
            from: SeriesEnd::Newest,
            ..self
        }
    }

    pub fn limit(self, limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..self
        }
    }
}

/// A decimated read: the last sample of each stride cell, which the peer
/// vouches for over `covered`. Never installed, since it is not the
/// component's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strided {
    pub covered: Range<Timestamp>,
    pub timestamps: Vec<Timestamp>,
    /// One sample per timestamp, of the component's element size.
    pub data: Vec<u8>,
}

/// Why a range fetch has no outcome.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// Dropped unserved: nobody asked again within the request TTL, or a
    /// full queue made room for more urgent work.
    #[error("the request expired before it was served")]
    Expired,
    /// The peer cannot answer this kind of read (a strided one, from a
    /// peer that predates range fetches).
    #[error("the peer cannot answer this read")]
    Unsupported,
    /// The peer answered, but nothing of the answer could be held here:
    /// no remote span covers it, as for history the peer sealed, or its
    /// head grew over, since the manifest was last seeded (re-listed on the
    /// spot, so asking again shortly works), or the unsealed head of a peer
    /// too old to report it.
    #[error("the peer's answer lies where no remote span here covers it")]
    NotHeld,
    #[error("{0}")]
    Failed(String),
}

/// What a range request asks, which identifies it in the queue.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum Ask {
    Range(RangeQuery),
    Strided(RangeQuery, Duration),
}

pub(super) type RangeKey = (ComponentId, Ask);

#[derive(Debug, Clone)]
pub(super) enum Outcome {
    Covered(Range<Timestamp>),
    Strided(Arc<Strided>),
}

/// Where a request's outcome lands, for whoever awaits it.
pub(super) struct Completion {
    outcome: Mutex<Option<Result<Outcome, FetchError>>>,
    done: WaitQueue,
}

impl Default for Completion {
    fn default() -> Self {
        Self {
            outcome: Mutex::new(None),
            done: WaitQueue::new(),
        }
    }
}

impl Completion {
    pub(super) fn finish(&self, outcome: Result<Outcome, FetchError>) {
        let mut slot = self.outcome.lock().unwrap();
        if slot.is_none() {
            *slot = Some(outcome);
        }
        drop(slot);
        self.done.wake_all();
    }

    #[cfg(test)]
    pub(super) fn outcome(&self) -> Option<Result<Outcome, FetchError>> {
        self.outcome.lock().unwrap().clone()
    }

    async fn wait(&self) -> Result<Outcome, FetchError> {
        let _ = self
            .done
            .wait_for(|| self.outcome.lock().unwrap().is_some())
            .await;
        self.outcome
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Err(FetchError::Expired))
    }
}

impl Hydrator {
    /// Ask for `query` of `component_id` to become locally answerable, as
    /// fragments. Never blocks; like [`Self::request`], a request lives for
    /// [`REQUEST_TTL`](super::REQUEST_TTL) unless asked again, so frame
    /// loops repeat it while they still need it, and a wide range is paged
    /// in over repeated asks.
    pub fn request_range(
        &self,
        component_id: ComponentId,
        query: RangeQuery,
        priority: HydratePriority,
    ) {
        self.ask(component_id, Ask::Range(query), priority, false);
    }

    /// [`Self::request_range`] for the sample a reader shows at `t`.
    pub fn request_point(
        &self,
        component_id: ComponentId,
        t: Timestamp,
        priority: HydratePriority,
    ) {
        self.request_range(component_id, RangeQuery::instant(t), priority);
    }

    /// [`Self::request_range`] for several reads at once, queued under one
    /// lock so the worker takes them as one batch: what a scrub asks of
    /// every reader on screen goes out in one round trip, rather than as
    /// many as the worker happens to wake between asks.
    pub fn request_ranges(
        &self,
        asks: impl IntoIterator<Item = (ComponentId, RangeQuery)>,
        priority: HydratePriority,
    ) {
        let now = Instant::now();
        let mut queue = self.inner.ranges.lock().unwrap();
        for (component_id, query) in asks {
            queue.push((component_id, Ask::Range(query)), priority, now, false);
        }
        drop(queue);
        self.inner.range_waker.wake();
    }

    /// Fetch `query` of `component_id` and install it as fragments,
    /// returning the range the peer vouched for: local reads answer it from
    /// then on, until eviction or a whole node replaces the fragments. When
    /// `query` is already answered locally, returns at once with what is
    /// held, without asking the peer. One fetch stops at [`PAGE_BYTES`]; ask
    /// again past `covered` for the rest.
    ///
    /// The request stays queued while this is awaited; dropping the future
    /// lets it expire like an unrepeated [`Self::request_range`].
    pub async fn fetch_range(
        &self,
        component_id: ComponentId,
        query: RangeQuery,
        priority: HydratePriority,
    ) -> Result<Range<Timestamp>, FetchError> {
        let done = self.ask(component_id, Ask::Range(query), priority, true);
        match done.expect("awaited asks complete").wait().await? {
            Outcome::Covered(covered) => Ok(covered),
            Outcome::Strided(_) => Err(FetchError::Failed("strided outcome".into())),
        }
    }

    /// [`Self::fetch_range`] for the sample a reader shows at `t`: once it
    /// returns, the newest sample at or before `t` is local (or the peer
    /// holds none), and the returned range is how far that answer holds.
    pub async fn point(
        &self,
        component_id: ComponentId,
        t: Timestamp,
        priority: HydratePriority,
    ) -> Result<Range<Timestamp>, FetchError> {
        self.fetch_range(component_id, RangeQuery::instant(t), priority)
            .await
    }

    /// The last sample of each epoch-aligned `stride` cell of `query`,
    /// returned rather than installed. Peers that predate range fetches
    /// answer [`FetchError::Unsupported`].
    pub async fn fetch_strided(
        &self,
        component_id: ComponentId,
        query: RangeQuery,
        stride: Duration,
        priority: HydratePriority,
    ) -> Result<Arc<Strided>, FetchError> {
        let done = self.ask(component_id, Ask::Strided(query, stride), priority, true);
        match done.expect("awaited asks complete").wait().await? {
            Outcome::Strided(strided) => Ok(strided),
            Outcome::Covered(_) => Err(FetchError::Failed("unstrided outcome".into())),
        }
    }

    fn ask(
        &self,
        component_id: ComponentId,
        ask: Ask,
        priority: HydratePriority,
        await_it: bool,
    ) -> Option<Arc<Completion>> {
        let done = self.inner.ranges.lock().unwrap().push(
            (component_id, ask),
            priority,
            Instant::now(),
            await_it,
        );
        self.inner.range_waker.wake();
        done
    }

    /// Serve range requests in batches until the runtime shuts down.
    ///
    /// A batch is at most [`MAX_BATCH`] requests and about [`BATCH_BYTES`]
    /// of replies. When the peer is unreachable, the worker waits out a
    /// growing backoff before the next batch rather than redialing for
    /// every frame's asks.
    pub(super) async fn serve_ranges(&self, db: &Arc<DB>, store: &dyn NodeStore) {
        let mut failures = 0u32;
        let mut retry_at: Option<Instant> = None;
        loop {
            if let Some(at) = retry_at.take() {
                let now = Instant::now();
                if at > now {
                    stellarator::sleep(at - now).await;
                }
            }
            let batch = self.inner.ranges.lock().unwrap().pop_batch(
                Instant::now(),
                MAX_BATCH,
                BATCH_BYTES,
                |(_, ask)| ask_bytes(ask),
            );
            if batch.is_empty() {
                let _ = self.inner.range_waker.wait().await;
                continue;
            }
            let mut sent: Vec<Sent> = Vec::new();
            let mut wire: Vec<GetTimeSeries> = Vec::new();
            for request in batch {
                let (component_id, ask) = &request.key;
                let Some(component) =
                    db.with_state(|state| state.components.get(component_id).cloned())
                else {
                    request.finish(Err(FetchError::Failed(format!(
                        "{component_id:?} is not known here"
                    ))));
                    continue;
                };
                let before = match ask {
                    Ask::Range(query) => {
                        if let Some(covered) = answered_here(&component.time_series, query) {
                            request.finish(Ok(Outcome::Covered(covered)));
                            continue;
                        }
                        held(&component.time_series, &query.range, query.from)
                    }
                    Ask::Strided(query, _) => query.range.start..query.range.start,
                };
                wire.push(wire_request(
                    *component_id,
                    ask,
                    component.schema.size(),
                    &before,
                ));
                sent.push(Sent {
                    request,
                    component,
                    before,
                });
            }
            if wire.is_empty() {
                continue;
            }
            match store.time_series(&wire).await {
                Ok(replies) => {
                    failures = 0;
                    for (sent, reply) in sent.iter().zip(replies) {
                        let outcome = match reply {
                            Ok(reply) => self.land_or_reseed(db, store, sent, &reply).await,
                            Err(err) => Err(FetchError::Failed(err.to_string())),
                        };
                        sent.request.finish(outcome);
                    }
                }
                Err(StoreError::Unsupported) => {
                    failures = 0;
                    for sent in &sent {
                        let outcome = self
                            .fall_back(db, store, &sent.request, &sent.component)
                            .await;
                        sent.request.finish(outcome);
                    }
                }
                Err(err) => {
                    warn!(?err, "range fetch failed");
                    if matches!(err, StoreError::Unavailable(_)) {
                        failures = failures.saturating_add(1);
                        retry_at = Some(Instant::now() + dial_backoff(failures));
                    }
                    for sent in &sent {
                        sent.request
                            .finish(Err(FetchError::Failed(err.to_string())));
                    }
                }
            }
        }
    }

    /// [`land`] a reply, and when no span here covers its answer (the peer
    /// sealed it, or its head grew over it, after the manifest was last
    /// seeded), re-list the component's remote spans and land it again.
    /// Re-listing waits [`RESEED_AFTER`] per component, so a read whose
    /// answer stays unspanned (a peer too old to report its head) costs a
    /// listing at most that often.
    async fn land_or_reseed(
        &self,
        db: &Arc<DB>,
        store: &dyn NodeStore,
        sent: &Sent,
        reply: &SeriesReply,
    ) -> Result<Outcome, FetchError> {
        let outcome = land(db, sent, reply);
        if !matches!(outcome, Err(FetchError::NotHeld)) || reply.covered.is_empty() {
            return outcome;
        }
        match self
            .reseed(db, store, sent.request.key.0, &sent.component)
            .await
        {
            true => land(db, sent, reply),
            false => outcome,
        }
    }

    /// Re-list `component`'s remote spans from `store`, unless that was
    /// done within [`RESEED_AFTER`]. Returns whether the listing recorded
    /// any seal not known before, or a head.
    async fn reseed(
        &self,
        db: &Arc<DB>,
        store: &dyn NodeStore,
        component_id: ComponentId,
        component: &Component,
    ) -> bool {
        {
            let now = Instant::now();
            let mut reseeded = self.inner.reseeded.lock().unwrap();
            if reseeded
                .get(&component_id)
                .is_some_and(|at| now.saturating_duration_since(*at) < RESEED_AFTER)
            {
                return false;
            }
            reseeded.insert(component_id, now);
            if reseeded.len() > RESEED_MEMO_CAP {
                reseeded.retain(|_, at| now.saturating_duration_since(*at) < RESEED_AFTER);
            }
        }
        let name = component_name(db, component_id);
        let listing = match store.listing(component_id, &name).await {
            Ok(listing) => listing,
            Err(err) => {
                warn!(?err, ?component_id, "failed to re-list remote spans");
                return false;
            }
        };
        let head = listing.head.is_some();
        match component
            .time_series
            .sync_remote(listing.nodes, listing.head)
        {
            // The head may have grown over the answer, too.
            Ok(added) => added > 0 || head,
            Err(err) => {
                warn!(?err, ?component_id, "failed to merge re-listed spans");
                false
            }
        }
    }

    /// Answer `request` with whole-node hydration, for a peer that can't
    /// answer range fetches: land the remote span nearest the requested
    /// end until the query is answered locally.
    async fn fall_back(
        &self,
        db: &Arc<DB>,
        store: &dyn NodeStore,
        request: &Request<RangeKey>,
        component: &Component,
    ) -> Result<Outcome, FetchError> {
        let (component_id, ask) = &request.key;
        let Ask::Range(query) = ask else {
            return Err(FetchError::Unsupported);
        };
        let series = &component.time_series;
        let name = component_name(db, *component_id);
        for _ in 0..FALLBACK_SPANS {
            if let Some(covered) = answered_here(series, query) {
                return Ok(Outcome::Covered(covered));
            }
            let mut gaps = GapVec::new();
            series.coverage(query.range.clone(), &mut gaps);
            gaps.retain(|g| g.state == SpanState::RemoteOnly);
            let gap = match query.from {
                SeriesEnd::Oldest => gaps.iter().min_by_key(|g| g.range.start),
                SeriesEnd::Newest => gaps.iter().max_by_key(|g| g.range.end),
            };
            let Some(gap) = gap.cloned() else {
                // Only fetches already in flight are left; what is held
                // now is all this read can vouch for.
                return Ok(Outcome::Covered(held(series, &query.range, query.from)));
            };
            let key = (*component_id, gap.seal_start);
            if self.backed_off(key) {
                return Err(FetchError::Failed("the span is backing off".into()));
            }
            match hydrate_span(
                series,
                store,
                *component_id,
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
                    return Err(FetchError::Failed(err.to_string()));
                }
            }
        }
        answered_here(series, query)
            .map(Outcome::Covered)
            .ok_or_else(|| FetchError::Failed("no span held the answer".into()))
    }
}

/// A request sent down the wire, with what the local series held of it
/// from the requested end before the reply.
pub(super) struct Sent {
    request: Request<RangeKey>,
    component: Component,
    before: Range<Timestamp>,
}

fn component_name(db: &DB, component_id: ComponentId) -> String {
    db.with_state(|state| {
        state
            .get_component_metadata(component_id)
            .map(|m| m.name.clone())
    })
    .unwrap_or_else(|| component_id.to_string())
}

/// The reply bytes a batch may expect for `ask`, for [`BATCH_BYTES`]: a page
/// for an unlimited read, else its limit at [`ESTIMATED_SAMPLE`] bytes.
fn ask_bytes(ask: &Ask) -> usize {
    let (Ask::Range(query) | Ask::Strided(query, _)) = ask;
    query.limit.map_or(PAGE_BYTES, |limit| {
        limit.saturating_mul(ESTIMATED_SAMPLE).min(PAGE_BYTES)
    })
}

/// The wait before the range connection is dialed again after `failures`
/// batches in a row found the peer unreachable.
fn dial_backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(16);
    DIAL_BACKOFF_BASE
        .saturating_mul(1u32 << shift)
        .min(DIAL_BACKOFF_CAP)
}

/// The wire request for `ask`, its limit capped to one page. An unlimited
/// range asks only for what is still missing past `held`, the stretch the
/// local series already answers from the requested end, so repeated asks
/// page through it instead of fetching its first page again.
fn wire_request(
    component_id: ComponentId,
    ask: &Ask,
    size: usize,
    held: &Range<Timestamp>,
) -> GetTimeSeries {
    let page = (PAGE_BYTES / (size + size_of::<Timestamp>())).max(1);
    let (query, stride) = match ask {
        Ask::Range(query) => (query, None),
        Ask::Strided(query, stride) => (query, Some(*stride)),
    };
    let range = match (stride, query.limit, query.from) {
        (None, None, SeriesEnd::Oldest) => held.end.max(query.range.start)..query.range.end,
        (None, None, SeriesEnd::Newest) => query.range.start..held.start.min(query.range.end),
        _ => query.range.clone(),
    };
    GetTimeSeries {
        id: [0; 8],
        range,
        component_id,
        limit: Some(query.limit.map_or(page, |limit| limit.min(page))),
        from: query.from,
        stride,
    }
}

/// Install or hand back one reply. Peer bytes are checked, never trusted:
/// a malformed reply fails the request.
///
/// A range reply reports what the local series answers now, which is the
/// union of what it held and what the reply covered where that could be
/// installed: the whole query once it is answered here (narrowed to what
/// the peer vouched for when the query has a limit, as a point read's
/// answer holds only that far), or, for an unlimited range, the stretch
/// held from the requested end when the reply extended it.
/// [`FetchError::NotHeld`] when the reply changed nothing, as when its
/// answer lies where no remote span here covers it.
fn land(db: &DB, sent: &Sent, reply: &SeriesReply) -> Result<Outcome, FetchError> {
    let failed = |err: &dyn std::fmt::Display| FetchError::Failed(err.to_string());
    let timestamps = reply
        .timestamps()
        .map_err(|err| failed(&format!("{err:?}")))?;
    let data = reply.data().map_err(|err| failed(&format!("{err:?}")))?;
    let size = sent.component.schema.size();
    if timestamps.len().checked_mul(size) != Some(data.len()) {
        return Err(failed(&"the reply's samples are not the component's size"));
    }
    let series = &sent.component.time_series;
    match &sent.request.key.1 {
        Ask::Range(query) => {
            let claimed = series
                .install_fragment(reply.covered.clone(), size, timestamps, data)
                .map_err(|err| failed(&err))?;
            if !claimed.is_empty()
                && let Some(first) = timestamps.first()
            {
                db.earliest_timestamp.update_min(*first);
            }
            let after = held(series, &query.range, query.from);
            if answered_here(series, query).is_some() {
                let vouched =
                    reply.covered.start.max(after.start)..reply.covered.end.min(after.end);
                return Ok(Outcome::Covered(match query.limit {
                    Some(_) if vouched.start < vouched.end => vouched,
                    _ => after,
                }));
            }
            let grew = |r: &Range<Timestamp>| (r.end.0 as i128) - (r.start.0 as i128);
            match query.limit.is_none() && grew(&after) > grew(&sent.before) {
                true => Ok(Outcome::Covered(after)),
                false => Err(FetchError::NotHeld),
            }
        }
        Ask::Strided(..) => Ok(Outcome::Strided(Arc::new(Strided {
            covered: reply.covered.clone(),
            timestamps: timestamps.to_vec(),
            data: data.to_vec(),
        }))),
    }
}

/// What of `query` the local series answers without asking the peer: the
/// stretch from the requested end to the first gap, when that is the whole
/// range or holds the samples the limit asks for.
pub fn answered_here(series: &TimeSeries, query: &RangeQuery) -> Option<Range<Timestamp>> {
    let held = held(series, &query.range, query.from);
    if held == query.range {
        return Some(held);
    }
    let limit = query.limit?;
    (Flat::new(series, &held).len() >= limit).then_some(held)
}

#[cfg(test)]
mod tests {
    use metor_proto::types::short_id;
    use std::{
        net::SocketAddr,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use metor_proto::types::PrimType;
    use metor_proto::vtable::builder::{
        component, raw_field, raw_table, schema, timestamp, vtable,
    };

    use super::*;
    use crate::{
        manifest::SpanSource,
        seal::SealRecord,
        store::{BoxFuture, NodeKey, NodeStaging, PeerStore, SealedNode},
    };

    const A: &str = "a";

    /// A peer store that counts range-fetch batches, or answers none of
    /// them, as a peer predating range fetches does; and that reports its
    /// unsealed head when listed, or not, as one predating heads does.
    struct Peer {
        store: PeerStore,
        ranges: bool,
        heads: std::sync::atomic::AtomicBool,
        batches: Mutex<Vec<usize>>,
        fetched_nodes: AtomicUsize,
    }

    impl NodeStore for Peer {
        fn put<'a>(
            &'a self,
            key: NodeKey<'a>,
            node: SealedNode<'a>,
        ) -> BoxFuture<'a, Result<(), StoreError>> {
            self.store.put(key, node)
        }

        fn get<'a>(
            &'a self,
            key: NodeKey<'a>,
            dst: &'a NodeStaging,
        ) -> BoxFuture<'a, Result<SealRecord, StoreError>> {
            self.fetched_nodes.fetch_add(1, Ordering::Relaxed);
            self.store.get(key, dst)
        }

        fn list<'a>(
            &'a self,
            component_id: ComponentId,
            component_name: &'a str,
        ) -> BoxFuture<'a, Result<Vec<SealRecord>, StoreError>> {
            self.store.list(component_id, component_name)
        }

        fn listing<'a>(
            &'a self,
            component_id: ComponentId,
            component_name: &'a str,
        ) -> BoxFuture<'a, Result<crate::store::Listing, StoreError>> {
            Box::pin(async move {
                let mut listing = self.store.listing(component_id, component_name).await?;
                if !self.heads.load(Ordering::Relaxed) {
                    listing.head = None;
                }
                Ok(listing)
            })
        }

        fn contains<'a>(&'a self, key: NodeKey<'a>) -> BoxFuture<'a, Result<bool, StoreError>> {
            self.store.contains(key)
        }

        fn time_series<'a>(
            &'a self,
            requests: &'a [GetTimeSeries],
        ) -> BoxFuture<'a, Result<Vec<Result<SeriesReply, StoreError>>, StoreError>> {
            if !self.ranges {
                return Box::pin(async { Err(StoreError::Unsupported) });
            }
            self.batches.lock().unwrap().push(requests.len());
            self.store.time_series(requests)
        }
    }

    fn with_a(db: &DB) {
        db.insert_vtable(metor_proto_wkt::VTableMsg {
            id: short_id([9, 4]),
            vtable: vtable([raw_field(
                8,
                8,
                timestamp(raw_table(0, 8), schema(PrimType::F64, &[1], component(A))),
            )]),
        })
        .unwrap();
    }

    fn series(db: &DB) -> TimeSeries {
        db.with_state(|s| {
            s.get_component(ComponentId::new(A))
                .unwrap()
                .time_series
                .clone()
        })
    }

    /// A peer holding samples every 10 over 0..2000, sealed as two nodes,
    /// and a mirror that knows them only as remote spans, hydrating from
    /// the peer with range fetches or without.
    fn setup(dir: &std::path::Path, ranges: bool) -> (Arc<DB>, Hydrator, Arc<Peer>) {
        setup_with(dir, ranges, &[(0, 1000), (1000, 2000)], 10, 2)
    }

    /// A peer holding samples every `step` over `nodes`, each sealed as a
    /// node of its own, and a mirror that knows the first `seeded` of them
    /// as remote spans.
    fn setup_with(
        dir: &std::path::Path,
        ranges: bool,
        nodes: &[(i64, i64)],
        step: usize,
        seeded: usize,
    ) -> (Arc<DB>, Hydrator, Arc<Peer>) {
        setup_live(dir, ranges, nodes, step, seeded, 0..0)
    }

    /// [`setup_with`], the peer also holding samples every `step` over
    /// `head` in its unsealed head, which the mirror hasn't listed yet.
    fn setup_live(
        dir: &std::path::Path,
        ranges: bool,
        nodes: &[(i64, i64)],
        step: usize,
        seeded: usize,
        head: Range<i64>,
    ) -> (Arc<DB>, Hydrator, Arc<Peer>) {
        let peer = Arc::new(DB::create(dir.join("peer")).unwrap());
        with_a(&peer);
        for node in nodes {
            let values: Vec<(Timestamp, [u8; 8])> = (node.0..node.1)
                .step_by(step)
                .map(|t| (Timestamp(t), (t as f64).to_le_bytes()))
                .collect();
            series(&peer)
                .install_samples(
                    8,
                    values.iter().map(|(t, v)| (*t, &v[..])),
                    SpanSource::LocalIngest,
                )
                .unwrap();
        }
        let seals: Vec<SealRecord> = series(&peer)
            .manifest()
            .spans
            .iter()
            .map(|s| s.seal)
            .take(seeded)
            .collect();
        let mut writer = series(&peer).writer().unwrap();
        for t in head.step_by(step) {
            writer
                .push_buf(Timestamp(t), &(t as f64).to_le_bytes())
                .unwrap();
        }
        drop(writer);
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let server = crate::Server { listener, db: peer };
        stellarator::struc_con::stellar(move || server.run());

        let mirror = Arc::new(DB::create(dir.join("mirror")).unwrap());
        with_a(&mirror);
        series(&mirror).merge_remote_spans(seals).unwrap();
        let store = Arc::new(Peer {
            store: PeerStore::new(addr),
            ranges,
            heads: true.into(),
            batches: Mutex::new(Vec::new()),
            fetched_nodes: AtomicUsize::new(0),
        });
        let hydrator = Hydrator::new();
        stellarator::spawn(hydrator.clone().run(mirror.clone(), store.clone()));
        (mirror, hydrator, store)
    }

    fn value(series: &TimeSeries, t: i64) -> Option<f64> {
        let sample = series.get(Timestamp(t))?;
        Some(f64::from_le_bytes(sample.data().try_into().unwrap()))
    }

    const SELECTED: HydratePriority = HydratePriority::Selected;

    #[stellarator::test]
    async fn a_point_read_fetches_the_sample_not_the_node() {
        let dir = tempfile::tempdir().unwrap();
        let (mirror, hydrator, peer) = setup(dir.path(), true);
        let a = ComponentId::new(A);
        let covered = hydrator.point(a, Timestamp(1234), SELECTED).await.unwrap();
        assert_eq!(covered, Timestamp(1221)..Timestamp(1235));
        let series = series(&mirror);
        assert_eq!(value(&series, 1230), Some(1230.0));
        assert_eq!(series.manifest().fragments.len(), 1);
        assert_eq!(
            series.manifest().span(Timestamp(1000)).unwrap().state,
            SpanState::RemoteOnly
        );
        assert_eq!(peer.fetched_nodes.load(Ordering::Relaxed), 0);
        // An instant whose predecessor it holds is answered here, without
        // asking again; one before it is not.
        let again = hydrator.point(a, Timestamp(1232), SELECTED).await.unwrap();
        assert_eq!(again, Timestamp(1221)..Timestamp(1233));
        assert_eq!(*peer.batches.lock().unwrap(), [1]);
        let earlier = hydrator.point(a, Timestamp(1225), SELECTED).await.unwrap();
        assert_eq!(earlier, Timestamp(1211)..Timestamp(1226));
        assert_eq!(*peer.batches.lock().unwrap(), [1, 1]);
        assert_eq!(value(&series, 1220), Some(1220.0));
    }

    /// A dashboard scrub: every instrument's point read goes out together.
    #[stellarator::test]
    async fn reads_asked_together_go_out_in_one_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (mirror, hydrator, peer) = setup(dir.path(), true);
        let a = ComponentId::new(A);
        let instants: Vec<i64> = (0..10).map(|i| 105 + i * 190).collect();
        for t in &instants {
            hydrator.request_point(a, Timestamp(*t), SELECTED);
        }
        for t in &instants {
            hydrator.point(a, Timestamp(*t), SELECTED).await.unwrap();
        }
        assert_eq!(*peer.batches.lock().unwrap(), [10]);
        let series = series(&mirror);
        for t in instants {
            let before = t - t % 10;
            assert_eq!(value(&series, before), Some(before as f64));
        }
    }

    #[stellarator::test]
    async fn a_range_fetch_installs_its_page() {
        let dir = tempfile::tempdir().unwrap();
        let (mirror, hydrator, _) = setup(dir.path(), true);
        let a = ComponentId::new(A);
        let query = RangeQuery::range(Timestamp(500)..Timestamp(1500));
        let covered = hydrator
            .fetch_range(a, query.clone(), SELECTED)
            .await
            .unwrap();
        assert_eq!(covered, Timestamp(500)..Timestamp(1500));
        let series = series(&mirror);
        let mut gaps = GapVec::new();
        series.coverage(Timestamp(500)..Timestamp(1500), &mut gaps);
        assert!(gaps.is_empty());
        assert_eq!(answered_here(&series, &query), Some(covered));
        // A limit pages: the next page starts where this one's cover ends.
        let page = RangeQuery::range(Timestamp(1500)..Timestamp(2000)).limit(3);
        let covered = hydrator.fetch_range(a, page, SELECTED).await.unwrap();
        assert_eq!(covered, Timestamp(1500)..Timestamp(1530));
    }

    #[stellarator::test]
    async fn strided_reads_hand_back_the_last_sample_per_cell() {
        let dir = tempfile::tempdir().unwrap();
        let (mirror, hydrator, _) = setup(dir.path(), true);
        let a = ComponentId::new(A);
        let query = RangeQuery::range(Timestamp(0)..Timestamp(1000));
        let strided = hydrator
            .fetch_strided(a, query, Duration::from_micros(250), SELECTED)
            .await
            .unwrap();
        assert_eq!(strided.covered, Timestamp(0)..Timestamp(1000));
        let stamps: Vec<i64> = strided.timestamps.iter().map(|t| t.0).collect();
        assert_eq!(stamps, [240, 490, 740, 990]);
        assert_eq!(strided.data.len(), 4 * 8);
        assert!(series(&mirror).manifest().fragments.is_empty());
    }

    /// A peer that can't answer range fetches still answers a point read,
    /// with the whole node holding it.
    #[stellarator::test]
    async fn older_peers_fall_back_to_whole_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let (mirror, hydrator, peer) = setup(dir.path(), false);
        let a = ComponentId::new(A);
        let covered = hydrator.point(a, Timestamp(1234), SELECTED).await.unwrap();
        assert!(covered.contains(&Timestamp(1234)));
        let series = series(&mirror);
        assert_eq!(value(&series, 1230), Some(1230.0));
        assert_eq!(
            series.manifest().span(Timestamp(1000)).unwrap().state,
            SpanState::Resident
        );
        assert_eq!(peer.fetched_nodes.load(Ordering::Relaxed), 1);
        let strided = hydrator
            .fetch_strided(
                a,
                RangeQuery::range(Timestamp(0)..Timestamp(10)),
                Duration::from_micros(5),
                SELECTED,
            )
            .await;
        assert_eq!(strided, Err(FetchError::Unsupported));
    }

    /// An unlimited range wider than a page pages in over repeated asks,
    /// each asking only for what is still missing.
    #[stellarator::test]
    async fn wide_ranges_page_in_over_repeated_asks() {
        let dir = tempfile::tempdir().unwrap();
        let (mirror, hydrator, peer) = setup_with(dir.path(), true, &[(0, 300_000)], 1, 1);
        let a = ComponentId::new(A);
        let query = RangeQuery::range(Timestamp(0)..Timestamp(300_000));
        let page = (PAGE_BYTES / 16) as i64;
        let first = hydrator
            .fetch_range(a, query.clone(), SELECTED)
            .await
            .unwrap();
        assert_eq!(first, Timestamp(0)..Timestamp(page));
        let second = hydrator
            .fetch_range(a, query.clone(), SELECTED)
            .await
            .unwrap();
        assert_eq!(second, Timestamp(0)..Timestamp(300_000));
        let third = hydrator.fetch_range(a, query, SELECTED).await.unwrap();
        assert_eq!(third, Timestamp(0)..Timestamp(300_000));
        assert_eq!(*peer.batches.lock().unwrap(), [1, 1]);
        let series = series(&mirror);
        let mut gaps = GapVec::new();
        series.coverage(Timestamp(0)..Timestamp(300_000), &mut gaps);
        assert!(gaps.is_empty());
        assert_eq!(value(&series, 299_999), Some(299_999.0));
        assert_eq!(peer.fetched_nodes.load(Ordering::Relaxed), 0);
    }

    /// The newest end pages backwards the same way.
    #[stellarator::test]
    async fn wide_ranges_page_from_the_newest_end_too() {
        let dir = tempfile::tempdir().unwrap();
        let (mirror, hydrator, _) = setup_with(dir.path(), true, &[(0, 300_000)], 1, 1);
        let a = ComponentId::new(A);
        let query = RangeQuery::range(Timestamp(0)..Timestamp(300_000)).newest();
        let first = hydrator
            .fetch_range(a, query.clone(), SELECTED)
            .await
            .unwrap();
        assert_eq!(first.end, Timestamp(300_000));
        assert!(first.start > Timestamp(0));
        let second = hydrator.fetch_range(a, query, SELECTED).await.unwrap();
        assert_eq!(second, Timestamp(0)..Timestamp(300_000));
        assert_eq!(value(&series(&mirror), 0), Some(0.0));
    }

    /// A read whose answer lies in a node the peer sealed after the mirror
    /// last listed its spans re-lists them and lands, rather than reporting
    /// an answer it never installed.
    #[stellarator::test]
    async fn a_read_past_the_seeded_spans_relists_them() {
        let dir = tempfile::tempdir().unwrap();
        let (mirror, hydrator, peer) =
            setup_with(dir.path(), true, &[(0, 1000), (1000, 2000)], 10, 1);
        let a = ComponentId::new(A);
        let covered = hydrator.point(a, Timestamp(1500), SELECTED).await.unwrap();
        assert_eq!(covered, Timestamp(1491)..Timestamp(1501));
        let series = series(&mirror);
        assert_eq!(value(&series, 1500), Some(1500.0));
        assert!(series.manifest().span(Timestamp(1000)).is_some());
        // Held now: asking again needs no round trip.
        hydrator.point(a, Timestamp(1500), SELECTED).await.unwrap();
        assert_eq!(*peer.batches.lock().unwrap(), [1]);
    }

    /// An answer in the unsealed head of a peer too old to report it can't
    /// land, since no span here covers it and re-listing finds none: the
    /// read says so instead of claiming success.
    #[stellarator::test]
    async fn an_answer_no_span_covers_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        let (mirror, hydrator, peer) =
            setup_live(dir.path(), true, &[(0, 1000)], 10, 1, 2000..3000);
        peer.heads.store(false, Ordering::Relaxed);
        let a = ComponentId::new(A);
        let err = hydrator.point(a, Timestamp(2505), SELECTED).await;
        assert_eq!(err, Err(FetchError::NotHeld));
        assert!(series(&mirror).get(Timestamp(2500)).is_none());
        assert!(series(&mirror).manifest().fragments.is_empty());
        // Re-listing waits a while before it is tried again.
        let again = hydrator.point(a, Timestamp(2505), SELECTED).await;
        assert_eq!(again, Err(FetchError::NotHeld));
        assert_eq!(*peer.batches.lock().unwrap(), [1, 1]);
    }

    /// A peer that has sealed nothing yet: its whole history is its head,
    /// which a mirror reads by range. A fragment of it is evictable (the
    /// peer still holds it), and survives the peer sealing that stretch,
    /// after which it is cache over an ordinary sealed span.
    #[stellarator::test]
    async fn a_peers_head_is_read_by_range_and_outlives_its_seal() {
        let dir = tempfile::tempdir().unwrap();
        let peer = Arc::new(DB::create(dir.path().join("peer")).unwrap());
        with_a(&peer);
        series(&peer).set_max_node_age(Duration::from_micros(1000));
        let mut writer = series(&peer).writer().unwrap();
        for t in (0..1000).step_by(10) {
            writer
                .push_buf(Timestamp(t), &(t as f64).to_le_bytes())
                .unwrap();
        }
        let listener = stellarator::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let server = crate::Server {
            listener,
            db: peer.clone(),
        };
        stellarator::struc_con::stellar(move || server.run());
        let mirror = Arc::new(DB::create(dir.path().join("mirror")).unwrap());
        with_a(&mirror);
        let store = Arc::new(Peer {
            store: PeerStore::new(addr),
            ranges: true,
            heads: true.into(),
            batches: Mutex::new(Vec::new()),
            fetched_nodes: AtomicUsize::new(0),
        });
        let hydrator = Hydrator::new();
        stellarator::spawn(hydrator.clone().run(mirror.clone(), store.clone()));
        let a = ComponentId::new(A);
        let sync = || async {
            let listing = store.listing(a, A).await.unwrap();
            series(&mirror)
                .sync_remote(listing.nodes, listing.head)
                .unwrap();
        };
        let gaps = |range: Range<i64>| {
            let mut gaps = GapVec::new();
            series(&mirror).coverage(Timestamp(range.start)..Timestamp(range.end), &mut gaps);
            gaps.iter()
                .map(|g| (g.range.start.0, g.range.end.0))
                .collect::<Vec<_>>()
        };

        sync().await;
        let manifest = series(&mirror).manifest();
        let [head] = &manifest.spans[..] else {
            panic!("one head span: {:?}", manifest.spans);
        };
        assert_eq!(
            (
                head.source,
                head.state,
                head.cover_start.0,
                head.cover_end.0
            ),
            (SpanSource::PeerHead, SpanState::RemoteOnly, 0, 990)
        );
        assert_eq!(head.seal.count, 100);
        assert!(series(&mirror).begin_fetch(Timestamp(0)).is_none());
        assert_eq!(gaps(0..1000), [(0, 991)]);

        let query = RangeQuery::range(Timestamp(0)..Timestamp(991));
        let covered = hydrator.fetch_range(a, query.clone(), SELECTED).await;
        assert_eq!(covered, Ok(Timestamp(0)..Timestamp(991)));
        assert_eq!(value(&series(&mirror), 500), Some(500.0));
        assert!(gaps(0..1000).is_empty());
        // The peer still holds it, so the fragment may go, and the head
        // reads as a gap again.
        assert!(series(&mirror).purge_fragment(Timestamp(0)).unwrap());
        assert_eq!(gaps(0..1000), [(0, 991)]);
        assert_eq!(value(&series(&mirror), 500), None);
        hydrator.fetch_range(a, query, SELECTED).await.unwrap();
        assert_eq!(value(&series(&mirror), 500), Some(500.0));

        // The peer rolls its head and seals what it held.
        writer
            .push_buf(Timestamp(1500), &1500f64.to_le_bytes())
            .unwrap();
        drop(writer);
        assert!(series(&peer).seal_rolled_nodes().unwrap());
        sync().await;
        let manifest = series(&mirror).manifest();
        let sealed = manifest.span(Timestamp(0)).unwrap();
        assert_eq!(
            (sealed.source, sealed.state, sealed.seal.end_ts.0),
            (SpanSource::RemoteFetch, SpanState::RemoteOnly, 990)
        );
        let head = manifest.span(Timestamp(1500)).unwrap();
        assert_eq!(head.source, SpanSource::PeerHead);
        assert_eq!(manifest.fragments.len(), 1);
        assert_eq!(value(&series(&mirror), 500), Some(500.0));
        assert_eq!(gaps(0..1600), [(1500, 1501)]);
        // Dropping the fragment hands its stretch to the sealed span, which
        // a whole node can now fill.
        assert!(series(&mirror).purge_fragment(Timestamp(0)).unwrap());
        assert_eq!(gaps(0..1000), [(0, 991)]);
        assert!(series(&mirror).begin_fetch(Timestamp(0)).is_some());
        assert_eq!(store.fetched_nodes.load(Ordering::Relaxed), 0);
    }

    /// A peer found too old for range fetches is asked again once the
    /// verdict is stale, so one upgraded in place is noticed.
    #[stellarator::test]
    async fn a_peer_found_unsupported_is_probed_again() {
        let dir = tempfile::tempdir().unwrap();
        let (_mirror, _hydrator, peer) = setup(dir.path(), true);
        let ask = [GetTimeSeries::instant(ComponentId::new(A), Timestamp(10))];
        assert!(peer.store.time_series(&ask).await.is_ok());
        peer.store.pretend_unsupported(Duration::ZERO).await;
        assert!(matches!(
            peer.store.time_series(&ask).await,
            Err(StoreError::Unsupported)
        ));
        peer.store
            .pretend_unsupported(Duration::from_secs(3600))
            .await;
        assert!(peer.store.time_series(&ask).await.is_ok());
    }

    /// With the peer down, the worker backs off between dials instead of
    /// redialing for every ask.
    #[stellarator::test]
    async fn an_unreachable_peer_is_redialed_after_a_backoff() {
        let dir = tempfile::tempdir().unwrap();
        let dead = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let mirror = Arc::new(DB::create(dir.path().join("mirror")).unwrap());
        with_a(&mirror);
        series(&mirror)
            .merge_remote_spans([SealRecord {
                start_ts: Timestamp(0),
                end_ts: Timestamp(100),
                count: 2,
                index_len: 16,
                data_len: 16,
                checksum: 1,
                element_size: 8,
            }])
            .unwrap();
        let store = Arc::new(Peer {
            store: PeerStore::new(dead),
            ranges: true,
            heads: true.into(),
            batches: Mutex::new(Vec::new()),
            fetched_nodes: AtomicUsize::new(0),
        });
        let hydrator = Hydrator::new();
        stellarator::spawn(hydrator.clone().run(mirror.clone(), store.clone()));
        let a = ComponentId::new(A);
        let first = hydrator.point(a, Timestamp(50), SELECTED).await;
        assert!(matches!(first, Err(FetchError::Failed(_))));
        let start = Instant::now();
        let second = hydrator.point(a, Timestamp(50), SELECTED).await;
        assert!(matches!(second, Err(FetchError::Failed(_))));
        assert!(start.elapsed() >= DIAL_BACKOFF_BASE);
        assert_eq!(store.batches.lock().unwrap().len(), 2);
    }

    #[stellarator::test]
    async fn an_unknown_component_fails_rather_than_waits() {
        let dir = tempfile::tempdir().unwrap();
        let (_mirror, hydrator, _) = setup(dir.path(), true);
        let err = hydrator
            .point(ComponentId::new("nope"), Timestamp(5), SELECTED)
            .await;
        assert!(matches!(err, Err(FetchError::Failed(_))));
    }
}
