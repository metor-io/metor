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
    #[error("{0}")]
    Failed(String),
}

/// What a range request asks, which identifies it in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    pub(super) async fn serve_ranges(&self, db: &Arc<DB>, store: &dyn NodeStore) {
        loop {
            let batch = self
                .inner
                .ranges
                .lock()
                .unwrap()
                .pop_batch(Instant::now(), MAX_BATCH);
            if batch.is_empty() {
                let _ = self.inner.range_waker.wait().await;
                continue;
            }
            let mut sent: Vec<(Request<RangeKey>, Component)> = Vec::new();
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
                if let Ask::Range(query) = ask
                    && let Some(covered) = answered_here(&component.time_series, query)
                {
                    request.finish(Ok(Outcome::Covered(covered)));
                    continue;
                }
                wire.push(wire_request(*component_id, ask, component.schema.size()));
                sent.push((request, component));
            }
            if wire.is_empty() {
                continue;
            }
            match store.time_series(&wire).await {
                Ok(replies) => {
                    for ((request, component), reply) in sent.iter().zip(replies) {
                        request.finish(land(db, request, component, reply));
                    }
                }
                Err(StoreError::Unsupported) => {
                    for (request, component) in &sent {
                        let outcome = self.fall_back(db, store, request, component).await;
                        request.finish(outcome);
                    }
                }
                Err(err) => {
                    warn!(?err, "range fetch failed");
                    for (request, _) in &sent {
                        request.finish(Err(FetchError::Failed(err.to_string())));
                    }
                }
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
        let name = db
            .with_state(|state| {
                state
                    .get_component_metadata(*component_id)
                    .map(|m| m.name.clone())
            })
            .unwrap_or_else(|| component_id.to_string());
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

/// The wire request for `ask`, its limit capped to one page.
fn wire_request(component_id: ComponentId, ask: &Ask, size: usize) -> GetTimeSeries {
    let page = (PAGE_BYTES / (size + size_of::<Timestamp>())).max(1);
    let (query, stride) = match ask {
        Ask::Range(query) => (query, None),
        Ask::Strided(query, stride) => (query, Some(*stride)),
    };
    GetTimeSeries {
        id: [0, 0],
        range: query.range.clone(),
        component_id,
        limit: Some(query.limit.map_or(page, |limit| limit.min(page))),
        from: query.from,
        stride,
    }
}

/// Install or hand back one reply.
fn land(
    db: &DB,
    request: &Request<RangeKey>,
    component: &Component,
    reply: Result<SeriesReply, StoreError>,
) -> Result<Outcome, FetchError> {
    let failed = |err: &dyn std::fmt::Display| FetchError::Failed(err.to_string());
    let reply = reply.map_err(|err| failed(&err))?;
    let timestamps = reply
        .timestamps()
        .map_err(|err| failed(&format!("{err:?}")))?;
    let data = reply.data().map_err(|err| failed(&format!("{err:?}")))?;
    match &request.key.1 {
        Ask::Range(_) => {
            component
                .time_series
                .install_fragment(
                    reply.covered.clone(),
                    component.schema.size(),
                    timestamps,
                    data,
                )
                .map_err(|err| failed(&err))?;
            if let Some(first) = timestamps.first() {
                db.earliest_timestamp.update_min(*first);
            }
            Ok(Outcome::Covered(reply.covered.clone()))
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
    /// them, as a peer predating range fetches does.
    struct Peer {
        store: PeerStore,
        ranges: bool,
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
            id: [9, 4],
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
        let peer = Arc::new(DB::create(dir.join("peer")).unwrap());
        with_a(&peer);
        for node in [0i64..1000, 1000..2000] {
            let values: Vec<(Timestamp, [u8; 8])> = node
                .step_by(10)
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
            .collect();
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
