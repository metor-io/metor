//! Working-set eviction: drop local copies nobody has read lately, as long
//! as something else can give them back.
//!
//! Readers mark what they use with [`TimeSeries::touch`]; this module purges
//! the sealed spans no touch has reached within a grace period ([`sweep`]),
//! or, under a byte budget, the least recently touched spans until the rest
//! fits ([`budget`]). Whether a
//! span *can* go is [`TimeSeries::purge_span`]'s call — an acked span (a
//! store or the peer it was hydrated from holds this exact node), or a
//! mirrored span whose peer seals cover every sample — so eviction never
//! loses data the DB itself knows to be unrecoverable. What it cannot know,
//! such as whether that peer is reachable right now, is the caller's
//! predicate, and so is when to run at all: this is a mechanism, the policy
//! (cadence, pausing, exemptions) lives with whoever drives it.
//!
//! Locally produced spans ([`SpanSource::LocalIngest`]) are never evicted
//! here, acked or not: they are the system of record, or panel-computed
//! data whose recomputation nothing schedules yet.

use std::{
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use metor_proto::types::{ComponentId, Timestamp};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use tracing::warn;

use crate::{
    Component, DB, Error,
    manifest::{NodeSpan, SpanSource, SpanState},
    time_series::TimeSeries,
};

/// Whether eviction can run on this platform. Purging unlinks a node's
/// files while readers may still hold slices of its mapping; unix keeps an
/// unlinked mapped file readable, but Windows refuses to delete a mapped
/// file at all, and `purge_span` would leave a half-purged node behind.
/// Until purges there wait for the last slice to drop, eviction is off.
pub const SUPPORTED: bool = cfg!(unix);

/// How long history may go unread before working-set eviction takes it.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// What a mirroring client does with history it holds but nobody is
/// reading. Only data something else can give back is ever evicted, under
/// any policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eviction {
    /// Keep only what readers touched within `grace` ([`sweep`]). The
    /// default: a mirror holds the live head and what is on screen.
    WorkingSet { grace: Duration },
    /// Keep the most recently touched history up to `max_bytes` ([`budget`]),
    /// for a cache that is reopened day after day and is worth more than
    /// the disk it takes.
    Budget { max_bytes: u64 },
    /// Keep everything that arrives.
    Off,
}

impl Default for Eviction {
    fn default() -> Self {
        Self::WorkingSet {
            grace: DEFAULT_GRACE,
        }
    }
}

/// What one pass purged.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Evicted {
    pub spans: usize,
    pub bytes: u64,
}

impl std::ops::AddAssign for Evicted {
    fn add_assign(&mut self, rhs: Self) {
        self.spans += rhs.spans;
        self.bytes += rhs.bytes;
    }
}

/// A node's last read, as microseconds past a process-wide epoch so it fits
/// one atomic. Starts at the node's creation, so a span just hydrated or
/// sealed gets a full grace period before anyone must have read it.
#[derive(Debug)]
pub(crate) struct LastAccess(AtomicU64);

impl LastAccess {
    pub(crate) fn now() -> Self {
        Self(AtomicU64::new(Self::stamp(Instant::now())))
    }

    pub(crate) fn stamp(at: Instant) -> u64 {
        static EPOCH: OnceLock<Instant> = OnceLock::new();
        let epoch = *EPOCH.get_or_init(Instant::now);
        at.saturating_duration_since(epoch).as_micros() as u64
    }

    /// Readers on many threads race to touch; any of their stamps is close
    /// enough, so this skips the store (and the cache-line write) when the
    /// node is already as fresh.
    pub(crate) fn touch(&self, stamp: u64) {
        if self.0.load(Ordering::Relaxed) < stamp {
            self.0.store(stamp, Ordering::Relaxed);
        }
    }

    fn last(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// What eviction can take: a sealed span, keyed by its start, or a
/// range-fetch fragment, keyed by its claim's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Victim {
    start: Timestamp,
    bytes: u64,
    fragment: bool,
}

impl Victim {
    fn of(span: &NodeSpan, fragment: bool) -> Self {
        Self {
            start: span.cover_start,
            bytes: span.bytes(),
            fragment,
        }
    }

    fn purge(&self, series: &TimeSeries) -> Result<bool, Error> {
        match self.fragment {
            true => series.purge_fragment(self.start),
            false => series.purge_span(self.start),
        }
    }
}

type Victims = SmallVec<[Victim; 8]>;

/// Call `found` with each span of `series` eviction may take, and its
/// node's last access: resident, sealed, not locally produced, and accepted
/// by `evictable`. Range-fetch fragments come as the spans they stand for
/// ([`crate::manifest::Fragment::as_span`]). Whether a purge agrees is only
/// known when it runs.
fn evictable_spans(
    series: &TimeSeries,
    evictable: &mut impl FnMut(&NodeSpan) -> bool,
    mut found: impl FnMut(u64, Victim),
) {
    let manifest = series.manifest();
    for node in series.list.iter() {
        if node.fragment {
            continue;
        }
        let Some(start) = node.timestamps().first().copied() else {
            continue;
        };
        // An unsealed node has no span: the live head, or one the
        // lifecycle task hasn't sealed yet.
        let Some(span) = manifest.span(start) else {
            continue;
        };
        if span.state == SpanState::Resident
            && span.source != SpanSource::LocalIngest
            && evictable(span)
        {
            found(node.last_access.last(), Victim::of(span, false));
        }
    }
    for fragment in manifest.fragments.iter() {
        let span = fragment.as_span();
        if evictable(&span) {
            found(fragment.access.last(), Victim::of(&span, true));
        }
    }
}

/// The access stamp below which a node has gone `grace` unread at `now`.
fn idle_cutoff(now: Instant, grace: Duration) -> u64 {
    LastAccess::stamp(now).saturating_sub(grace.as_micros() as u64)
}

/// Append the keys (and sizes) of `series`' evictable spans untouched since
/// `now - grace`.
fn idle_spans(
    series: &TimeSeries,
    now: Instant,
    grace: Duration,
    evictable: &mut impl FnMut(&NodeSpan) -> bool,
    out: &mut Victims,
) {
    let cutoff = idle_cutoff(now, grace);
    evictable_spans(series, evictable, |last, victim| {
        if last < cutoff {
            out.push(victim);
        }
    });
}

/// Purge `series`' spans nobody touched within `grace` of `now` that
/// `evictable` accepts and [`TimeSeries::purge_span`] can give back.
pub fn evict(
    series: &TimeSeries,
    now: Instant,
    grace: Duration,
    mut evictable: impl FnMut(&NodeSpan) -> bool,
) -> Result<Evicted, Error> {
    let mut evicted = Evicted::default();
    if !SUPPORTED {
        return Ok(evicted);
    }
    let mut victims = Victims::new();
    idle_spans(series, now, grace, &mut evictable, &mut victims);
    for victim in victims {
        if victim.purge(series)? {
            evicted.spans += 1;
            evicted.bytes += victim.bytes;
        }
    }
    Ok(evicted)
}

/// Whether any component of `db` has a span [`sweep`] would try to purge.
/// Lets a driver do something before evicting — such as asking every
/// visible reader to touch what it shows — only when there is something
/// to evict.
pub fn any_idle(
    db: &DB,
    now: Instant,
    grace: Duration,
    mut evictable: impl FnMut(ComponentId, &NodeSpan) -> bool,
) -> bool {
    if !SUPPORTED {
        return false;
    }
    let mut victims = Victims::new();
    components(db).iter().any(|component| {
        idle_spans(
            &component.time_series,
            now,
            grace,
            &mut |span| evictable(component.component_id, span),
            &mut victims,
        );
        !victims.is_empty()
    })
}

/// [`evict`] across every component of `db`. A component that fails to
/// purge is logged and skipped, so one bad directory doesn't stall the rest.
pub fn sweep(
    db: &DB,
    now: Instant,
    grace: Duration,
    mut evictable: impl FnMut(ComponentId, &NodeSpan) -> bool,
) -> Evicted {
    let mut total = Evicted::default();
    if !SUPPORTED {
        return total;
    }
    for component in components(db) {
        let id = component.component_id;
        match evict(&component.time_series, now, grace, |span| {
            evictable(id, span)
        }) {
            Ok(evicted) => total += evicted,
            Err(err) => warn!(?err, ?id, "eviction failed"),
        }
    }
    total
}

/// Whether the bytes [`budget`] counts exceed `max_bytes`, so a driver can
/// ask readers to touch what they show before a pass, as with [`any_idle`],
/// only when one would purge.
pub fn over_budget(
    db: &DB,
    max_bytes: u64,
    mut evictable: impl FnMut(ComponentId, &NodeSpan) -> bool,
) -> bool {
    if !SUPPORTED {
        return false;
    }
    let mut total = 0u64;
    for component in components(db) {
        let id = component.component_id;
        evictable_spans(
            &component.time_series,
            &mut |span| evictable(id, span),
            |_, victim| total += victim.bytes,
        );
        if total > max_bytes {
            return true;
        }
    }
    false
}

/// Purge `db`'s least recently touched evictable spans until the evictable
/// bytes left fit in `max_bytes`.
///
/// Only what [`sweep`] could take counts toward the budget or is purged, so
/// the live head, unsealed spans and locally produced data neither count
/// nor go. Spans touched within `grace` of `now` stay even over budget: a
/// budget smaller than what views are reading would otherwise purge and
/// refetch it on every pass. Sizes are a span's sealed index and data
/// bytes, which is what its sparse node files hold on disk.
pub fn budget(
    db: &DB,
    now: Instant,
    max_bytes: u64,
    grace: Duration,
    mut evictable: impl FnMut(ComponentId, &NodeSpan) -> bool,
) -> Evicted {
    let mut evicted = Evicted::default();
    if !SUPPORTED {
        return evicted;
    }
    let components = components(db);
    let mut candidates = Vec::new();
    let mut total = 0u64;
    for (index, component) in components.iter().enumerate() {
        let id = component.component_id;
        evictable_spans(
            &component.time_series,
            &mut |span| evictable(id, span),
            |last, victim| {
                total += victim.bytes;
                candidates.push((last, index, victim));
            },
        );
    }
    if total <= max_bytes {
        return evicted;
    }
    let cutoff = idle_cutoff(now, grace);
    candidates.sort_unstable_by_key(|(last, ..)| *last);
    for (last, index, victim) in candidates {
        if total <= max_bytes || last >= cutoff {
            break;
        }
        let component = &components[index];
        match victim.purge(&component.time_series) {
            Ok(true) => {
                total -= victim.bytes;
                evicted.spans += 1;
                evicted.bytes += victim.bytes;
            }
            Ok(false) => {}
            Err(err) => warn!(?err, id = ?component.component_id, "eviction failed"),
        }
    }
    evicted
}

fn components(db: &DB) -> Vec<Component> {
    db.with_state(|state| state.components.values().cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        manifest::SpanSource,
        seal::SealRecord,
        store::NodeStaging,
        time_series::{MirrorRun, TimeSeries},
    };

    const GRACE: Duration = Duration::from_secs(5);

    fn staged(dir: &std::path::Path, stamps: std::ops::Range<i64>) -> (NodeStaging, SealRecord) {
        let staging =
            NodeStaging::with_capacity(dir, Timestamp(stamps.start), 8, stamps.clone().count())
                .unwrap();
        for ts in stamps {
            staging.push(Timestamp(ts), &ts.to_le_bytes()).unwrap();
        }
        let seal = staging.seal().unwrap();
        (staging, seal)
    }

    /// A span hydrated from a peer, which installs acked.
    fn hydrated(series: &TimeSeries, dir: &std::path::Path, stamps: std::ops::Range<i64>) {
        let (staging, seal) = staged(dir, stamps);
        series
            .install_node(staging, &seal, SpanSource::RemoteFetch)
            .unwrap();
    }

    fn state(series: &TimeSeries, start: i64) -> Option<SpanState> {
        series.manifest().span(Timestamp(start)).map(|s| s.state)
    }

    fn later(by: Duration) -> Instant {
        Instant::now() + by
    }

    #[test]
    fn untouched_acked_span_is_purged_after_grace() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        hydrated(&series, dir.path(), 0..10);

        let within = evict(&series, later(GRACE / 2), GRACE, |_| true).unwrap();
        assert_eq!(within.spans, 0, "a fresh span gets a full grace period");

        let evicted = evict(&series, later(GRACE * 2), GRACE, |_| true).unwrap();
        assert_eq!(evicted.spans, 1);
        assert!(evicted.bytes > 0);
        assert_eq!(state(&series, 0), Some(SpanState::RemoteOnly));
        assert!(series.get(Timestamp(5)).is_none());
    }

    #[test]
    fn touched_span_is_kept_and_untouched_neighbour_goes() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        hydrated(&series, dir.path(), 0..10);
        hydrated(&series, dir.path(), 100..110);
        hydrated(&series, dir.path(), 200..210);

        let now = later(GRACE * 2);
        // A window inside the middle span; the predecessor rule reaches no
        // further back than the span holding the window's start.
        series.touch(Timestamp(102)..Timestamp(105), now);
        let evicted = evict(&series, now + Duration::from_secs(1), GRACE, |_| true).unwrap();
        assert_eq!(evicted.spans, 2);
        assert_eq!(state(&series, 0), Some(SpanState::RemoteOnly));
        assert_eq!(state(&series, 100), Some(SpanState::Resident));
        assert_eq!(state(&series, 200), Some(SpanState::RemoteOnly));
    }

    #[test]
    fn touch_reaches_the_predecessor_span() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        hydrated(&series, dir.path(), 0..10);
        hydrated(&series, dir.path(), 100..110);

        let now = later(GRACE * 2);
        // An instant in the hole between spans reads its predecessor.
        series.touch(Timestamp(50)..Timestamp(51), now);
        evict(&series, now, GRACE, |_| true).unwrap();
        assert_eq!(state(&series, 0), Some(SpanState::Resident));
        assert_eq!(state(&series, 100), Some(SpanState::RemoteOnly));
    }

    #[test]
    fn predicate_and_unrecoverable_spans_are_respected() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        hydrated(&series, dir.path(), 0..10);
        // Mirrored, unacked, and no peer seal around it: nothing else holds it.
        let (staging, seal) = staged(dir.path(), 100..110);
        series
            .install_node(staging, &seal, SpanSource::Mirrored)
            .unwrap();

        let now = later(GRACE * 2);
        assert_eq!(evict(&series, now, GRACE, |_| false).unwrap().spans, 0);
        assert_eq!(state(&series, 0), Some(SpanState::Resident));
        assert_eq!(evict(&series, now, GRACE, |_| true).unwrap().spans, 1);
        assert_eq!(state(&series, 0), Some(SpanState::RemoteOnly));
        assert_eq!(state(&series, 100), Some(SpanState::Resident));
    }

    #[test]
    fn a_span_the_peer_stopped_listing_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let (staging, seal) = staged(dir.path(), 0..10);
        series
            .install_node(staging, &seal, SpanSource::RemoteFetch)
            .unwrap();
        let (_, other) = staged(&dir.path().join("peer"), 100..110);

        // The peer's listing no longer has the hydrated node: this side's
        // copy is the only one left.
        series.sync_remote(vec![other], None).unwrap();
        let now = later(GRACE * 2);
        assert_eq!(evict(&series, now, GRACE, |_| true).unwrap().spans, 0);
        assert_eq!(state(&series, 0), Some(SpanState::Resident));
        // An incremental merge doesn't speak for what the peer dropped.
        series.merge_remote_spans([other]).unwrap();
        assert!(!series.manifest().span(Timestamp(0)).unwrap().acked);

        // Listed again, the peer can give it back.
        series.sync_remote(vec![seal, other], None).unwrap();
        assert!(series.manifest().span(Timestamp(0)).unwrap().acked);
        assert_eq!(evict(&series, now, GRACE, |_| true).unwrap().spans, 1);
        assert_eq!(state(&series, 0), Some(SpanState::RemoteOnly));
    }

    #[test]
    fn a_withdrawn_ack_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        {
            let series = TimeSeries::create(dir.path()).unwrap();
            hydrated(&series, dir.path(), 0..10);
            series.sync_remote(vec![], None).unwrap();
        }
        let series = TimeSeries::open(dir.path()).unwrap();
        assert!(!series.manifest().span(Timestamp(0)).unwrap().acked);
        let evicted = evict(&series, later(GRACE * 2), GRACE, |_| true).unwrap();
        assert_eq!(evicted.spans, 0);
        assert!(series.get(Timestamp(5)).is_some());
    }

    #[test]
    fn live_head_and_local_ingest_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        let mut writer = series.writer().unwrap();
        for ts in 0..10i64 {
            writer.push_buf(Timestamp(ts), &ts.to_le_bytes()).unwrap();
        }
        series.set_max_node_age(Duration::from_micros(5));
        for ts in 10..20i64 {
            writer.push_buf(Timestamp(ts), &ts.to_le_bytes()).unwrap();
        }
        series.seal_rolled_nodes().unwrap();
        let sealed = series.manifest().spans.to_vec();
        assert!(!sealed.is_empty(), "rolled nodes sealed");
        for span in &sealed {
            assert_eq!(span.source, SpanSource::LocalIngest);
            // Even an ack (a tiering store's copy) doesn't make locally
            // produced data part of the working set.
            series.mark_acked(span.cover_start).unwrap();
        }

        let evicted = evict(&series, later(GRACE * 2), GRACE, |_| true).unwrap();
        assert_eq!(evicted.spans, 0);
        assert_eq!(series.latest().unwrap().timestamp(), Timestamp(19));
        assert!(series.get(Timestamp(0)).is_some());
    }

    #[test]
    fn mirrored_live_head_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        series.enter_mirror_run(MirrorRun::next());
        let mut writer = series.writer().unwrap();
        for ts in 0..10i64 {
            writer.push_buf(Timestamp(ts), &ts.to_le_bytes()).unwrap();
        }
        // The peer sealed past everything the head holds.
        let (_, seal) = staged(&dir.path().join("peer"), 0..20);
        series.merge_remote_spans([seal]).unwrap();

        let evicted = evict(&series, later(GRACE * 2), GRACE, |_| true).unwrap();
        assert_eq!(evicted.spans, 0);
        assert_eq!(series.latest().unwrap().timestamp(), Timestamp(9));
    }

    #[test]
    fn peer_backed_mirrored_span_is_purged_to_remote_only() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        series.enter_mirror_run(MirrorRun::next());
        let mut writer = series.writer().unwrap();
        for ts in 0..10i64 {
            writer.push_buf(Timestamp(ts), &ts.to_le_bytes()).unwrap();
        }
        // A new run rolls the head, so the first node seals as mirrored.
        series.enter_mirror_run(MirrorRun::next());
        writer
            .push_buf(Timestamp(50), &50i64.to_le_bytes())
            .unwrap();
        series.seal_rolled_nodes().unwrap();
        let mirrored = *series.manifest().span(Timestamp(0)).unwrap();
        assert_eq!(mirrored.source, SpanSource::Mirrored);

        let now = later(GRACE * 2);
        assert_eq!(
            evict(&series, now, GRACE, |_| true).unwrap().spans,
            0,
            "no peer seal covers it yet"
        );

        let (_, seal) = staged(&dir.path().join("peer"), 0..20);
        series.merge_remote_spans([seal]).unwrap();
        let evicted = evict(&series, now, GRACE, |_| true).unwrap();
        assert_eq!(evicted.spans, 1);
        assert!(series.get(Timestamp(5)).is_none());
        let mut gaps = crate::manifest::GapVec::new();
        series.coverage(Timestamp(0)..Timestamp(10), &mut gaps);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].state, SpanState::RemoteOnly);
        assert_eq!(gaps[0].range, Timestamp(0)..Timestamp(10));
        assert_eq!(series.latest().unwrap().timestamp(), Timestamp(50));
    }

    #[test]
    fn touch_is_ignored_by_internal_reads() {
        let dir = tempfile::tempdir().unwrap();
        let series = TimeSeries::create(dir.path()).unwrap();
        hydrated(&series, dir.path(), 0..10);
        let mut gaps = crate::manifest::GapVec::new();
        series.coverage(Timestamp(0)..Timestamp(10), &mut gaps);
        assert!(series.get(Timestamp(3)).is_some());
        assert!(series.get_range(Timestamp(0)..Timestamp(10)).is_some());
        assert_eq!(
            evict(&series, later(GRACE * 2), GRACE, |_| true)
                .unwrap()
                .spans,
            1
        );
    }

    /// A db with one component holding `spans` hydrated spans of ten
    /// samples each, returning the db and the component's series.
    fn hydrated_db(dir: &std::path::Path, spans: i64) -> (DB, TimeSeries) {
        let db = DB::create(dir.join("db")).unwrap();
        let id = ComponentId::new("a");
        db.with_state_mut(|state| {
            state.insert_component(
                id,
                crate::ComponentSchema::new(
                    metor_proto::types::PrimType::I64,
                    SmallVec::<[usize; 4]>::new(),
                ),
                &db.path,
            )
        })
        .unwrap();
        let series = db.with_state(|state| state.get_component(id).unwrap().time_series.clone());
        let component_dir = db.path.join(id.to_string());
        for n in 0..spans {
            hydrated(&series, &component_dir, n * 100..n * 100 + 10);
        }
        (db, series)
    }

    #[test]
    fn budget_purges_least_recently_touched_first() {
        let dir = tempfile::tempdir().unwrap();
        let (db, series) = hydrated_db(dir.path(), 3);
        let span = series.manifest().span(Timestamp(0)).unwrap().bytes();
        let now = later(GRACE * 4);
        series.touch(Timestamp(0)..Timestamp(10), now - GRACE * 3);
        series.touch(Timestamp(200)..Timestamp(210), now - GRACE * 2);
        // 100 was touched least recently (at creation).

        let all = |_: ComponentId, _: &NodeSpan| true;
        assert_eq!(budget(&db, now, 3 * span, GRACE, all).spans, 0, "fits");
        let evicted = budget(&db, now, 2 * span, GRACE, all);
        assert_eq!(
            evicted,
            Evicted {
                spans: 1,
                bytes: span
            }
        );
        assert_eq!(state(&series, 100), Some(SpanState::RemoteOnly));
        let evicted = budget(&db, now, span, GRACE, all);
        assert_eq!(evicted.spans, 1);
        assert_eq!(state(&series, 0), Some(SpanState::RemoteOnly));
        assert_eq!(state(&series, 200), Some(SpanState::Resident));
    }

    #[test]
    fn budget_keeps_what_was_just_read() {
        let dir = tempfile::tempdir().unwrap();
        let (db, series) = hydrated_db(dir.path(), 2);
        let now = later(GRACE * 4);
        series.touch(Timestamp(100)..Timestamp(110), now);
        let evicted = budget(&db, now, 0, GRACE, |_, _| true);
        assert_eq!(evicted.spans, 1, "over budget, but one span is in use");
        assert_eq!(state(&series, 0), Some(SpanState::RemoteOnly));
        assert_eq!(state(&series, 100), Some(SpanState::Resident));
    }

    #[test]
    fn budget_respects_the_predicate_and_counts_only_evictable_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (db, series) = hydrated_db(dir.path(), 2);
        let span = series.manifest().span(Timestamp(0)).unwrap().bytes();
        let now = later(GRACE * 4);
        // The span at 0 is exempt, so only the other counts: it fits.
        let not_first = |_: ComponentId, span: &NodeSpan| span.cover_start != Timestamp(0);
        assert!(!over_budget(&db, span, not_first));
        assert!(over_budget(&db, span, |_, _| true));
        assert_eq!(budget(&db, now, span, GRACE, not_first).spans, 0);
        assert_eq!(budget(&db, now, 0, GRACE, not_first).spans, 1);
        assert_eq!(state(&series, 0), Some(SpanState::Resident));
        assert_eq!(state(&series, 100), Some(SpanState::RemoteOnly));
        assert_eq!(budget(&db, now, 0, GRACE, |_, _| false).spans, 0);
        assert_eq!(state(&series, 0), Some(SpanState::Resident));
    }
}
