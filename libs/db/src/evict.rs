//! Working-set eviction: drop local copies nobody has read lately, as long
//! as something else can give them back.
//!
//! Readers mark what they use with [`TimeSeries::touch`]; this module purges
//! the sealed spans no touch has reached within a grace period. Whether a
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

    fn idle_before(&self, cutoff: u64) -> bool {
        self.0.load(Ordering::Relaxed) < cutoff
    }
}

type Victims = SmallVec<[(Timestamp, u64); 8]>;

/// Append the keys (and sizes) of `series`' spans eviction would take now:
/// resident, sealed, not locally produced, untouched since `now - grace`,
/// and accepted by `evictable`. Whether `purge_span` agrees is only known
/// when it runs.
fn idle_spans(
    series: &TimeSeries,
    now: Instant,
    grace: Duration,
    evictable: &mut impl FnMut(&NodeSpan) -> bool,
    out: &mut Victims,
) {
    let cutoff = LastAccess::stamp(now).saturating_sub(grace.as_micros() as u64);
    let manifest = series.manifest();
    for node in series.list.iter() {
        if !node.last_access.idle_before(cutoff) {
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
            out.push((start, span.bytes()));
        }
    }
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
    for (start, bytes) in victims {
        if series.purge_span(start)? {
            evicted.spans += 1;
            evicted.bytes += bytes;
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
}
