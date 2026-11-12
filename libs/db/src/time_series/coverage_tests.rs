//! Remote coverage around mirrored data: fetch claims racing purges,
//! settled stretches, ties at peer node boundaries, crash windows, and
//! peers that stop listing seals.

use std::path::Path;

use metor_proto::types::Timestamp;

use crate::{
    manifest::{ComponentManifest, Coverage, GapVec, NodeSpan, SpanSource, SpanState},
    seal::{SealRecord, SealRecordExt},
    store::{NodeFile, NodeStaging},
    time_series::{MirrorRun, TimeSeries, TimeSeriesNode},
};

pub(super) fn peer_node(dir: &Path, stamps: &[i64]) -> TimeSeriesNode {
    let src =
        TimeSeriesNode::create(dir.join(stamps[0].to_string()), Timestamp(stamps[0]), 8).unwrap();
    for ts in stamps {
        src.data.write(&(*ts as u64).to_le_bytes()).unwrap();
        src.index.write(&Timestamp(*ts).to_le_bytes()).unwrap();
    }
    src
}

pub(super) fn every_10(to: i64) -> Vec<i64> {
    (0..=to).step_by(10).collect()
}

/// Stage `src` in full, as a fetch would.
pub(super) fn stage(dir: &Path, src: &TimeSeriesNode, seal: &SealRecord) -> NodeStaging {
    let staging = NodeStaging::create(dir, seal).unwrap();
    staging
        .append_file(NodeFile::Index, src.index.data())
        .unwrap();
    staging
        .append_file(NodeFile::Data, src.data.data())
        .unwrap();
    staging
}

pub(super) fn stamps(series: &TimeSeries, start: i64, end: i64) -> Vec<i64> {
    let Some(slice) = series.get_range(Timestamp(start)..Timestamp(end)) else {
        return vec![];
    };
    let mut nodes: Vec<_> = slice.as_iter().collect();
    nodes.reverse();
    nodes
        .iter()
        .flat_map(|n| n.timestamps().iter().map(|t| t.0))
        .collect()
}

fn gaps(series: &TimeSeries, start: i64, end: i64) -> (Coverage, Vec<(i64, i64)>) {
    let mut gaps = GapVec::new();
    let coverage = series.coverage(Timestamp(start)..Timestamp(end), &mut gaps);
    (
        coverage,
        gaps.iter()
            .map(|g| (g.range.start.0, g.range.end.0))
            .collect(),
    )
}

/// Write `runs` through a mirror, each run its own connection, ending with
/// a head at 500 so every run seals.
fn mirrored(dir: &Path, runs: &[&[i64]]) -> TimeSeries {
    let series = TimeSeries::create(dir).unwrap();
    let mut writer = series.writer().unwrap();
    for run in runs.iter().copied().chain([&[500i64][..]]) {
        series.enter_mirror_run(MirrorRun::next());
        for ts in run {
            writer
                .push_buf(Timestamp(*ts), &(*ts as u64).to_le_bytes())
                .unwrap();
        }
    }
    drop(writer);
    series.seal_rolled_nodes().unwrap();
    series
}

/// A mirrored node at [100, 101] under a peer seal over 0..=140.
fn split(dir: &Path) -> (TimeSeries, TimeSeriesNode, SealRecord) {
    let series = mirrored(&dir.join("c"), &[&[100, 101]]);
    let src = peer_node(&dir.join("peer"), &every_10(140));
    let peer = SealRecord::compute(&src).unwrap();
    assert_eq!(series.merge_remote_spans([peer]).unwrap(), 1);
    (series, src, peer)
}

#[test]
fn purge_waits_for_a_fetch_over_its_data() {
    let dir = tempfile::tempdir().unwrap();
    let (series, src, peer) = split(dir.path());
    assert!(series.begin_fetch(Timestamp(0)).is_some());
    // The only seal behind the mirrored node is mid-fetch: refused.
    assert!(!series.purge_span(Timestamp(100)).unwrap());
    series
        .install_fetched(stage(&dir.path().join("c"), &src, &peer), &peer)
        .unwrap();
    let mut expected = every_10(140);
    expected.insert(expected.binary_search(&100).unwrap() + 1, 101);
    expected.push(500);
    assert_eq!(stamps(&series, 0, 501), expected);
    assert_eq!(gaps(&series, 0, 501).0, Coverage::Complete);

    // Settled, the mirrored node may go, and its range comes back.
    assert!(series.purge_span(Timestamp(100)).unwrap());
    assert_eq!(gaps(&series, 0, 501).1, [(100, 102)]);
}

#[test]
fn abort_rederives_against_current_residency() {
    let dir = tempfile::tempdir().unwrap();
    let (series, _, peer) = split(dir.path());
    series.begin_fetch(Timestamp(0)).unwrap();
    series.abort_fetch(Timestamp(0));
    assert!(series.purge_span(Timestamp(100)).unwrap());
    assert_eq!(gaps(&series, 0, 141).1, [(0, 141)]);
    // A failed fetch must not restore pieces derived around the node.
    series.begin_fetch(Timestamp(0)).unwrap();
    series.abort_fetch(Timestamp(0));
    let manifest = series.manifest();
    let span = manifest.span(Timestamp(0)).unwrap();
    assert!(span.is_whole());
    assert_eq!(span.seal, peer);
}

#[test]
fn live_head_over_a_claim_rederives_and_refuses_above_it() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    std::fs::create_dir_all(&cdir).unwrap();
    let series = TimeSeries::create(&cdir).unwrap();
    let src = peer_node(&dir.path().join("peer"), &every_10(140));
    let peer = SealRecord::compute(&src).unwrap();
    series.merge_remote_spans([peer]).unwrap();
    series.begin_fetch(Timestamp(0)).unwrap();

    // The live head lands inside the claimed seal mid-fetch.
    let mut writer = series.writer().unwrap();
    series.enter_mirror_run(MirrorRun::next());
    writer
        .push_buf(Timestamp(100), &100u64.to_le_bytes())
        .unwrap();
    assert!(
        series
            .install_fetched(stage(&cdir, &src, &peer), &peer)
            .is_err()
    );
    series.abort_fetch(Timestamp(0));
    assert_eq!(gaps(&series, 0, 141).1, [(0, 100), (101, 141)]);

    // Above an unsealed head nothing can land, so it is not claimed.
    assert!(series.begin_fetch(Timestamp(101)).is_none());
    let claim = series.begin_fetch(Timestamp(0)).unwrap();
    assert_eq!(claim.cover(), Timestamp(0)..Timestamp(100));
    series
        .install_fetched(stage(&cdir, &src, &peer), &peer)
        .unwrap();
    assert_eq!(stamps(&series, 0, 101), every_10(100));
    assert_eq!(gaps(&series, 0, 141).1, [(101, 141)]);
}

#[test]
fn split_install_never_publishes_duplicate_keys() {
    let dir = tempfile::tempdir().unwrap();
    let (series, src, peer) = split(dir.path());
    series.begin_fetch(Timestamp(0)).unwrap();
    // Something else installs the front piece's data mid-fetch.
    let staging = stage(&dir.path().join("c"), &src, &peer);
    let (node, piece_seal) = staging.cut(Timestamp(0)..=Timestamp(99)).unwrap().unwrap();
    series
        .install_node(node, &piece_seal, SpanSource::Mirrored)
        .unwrap();
    let manifest = series.manifest();
    assert!(
        manifest
            .spans
            .windows(2)
            .all(|w| w[0].cover_start < w[1].cover_start)
    );
    // The fetch's claim went with it.
    assert!(
        series
            .install_fetched(stage(&dir.path().join("c"), &src, &peer), &peer)
            .is_err()
    );
    assert!(
        series
            .manifest()
            .spans
            .iter()
            .all(|s| s.state != SpanState::Fetching)
    );
    assert_eq!(gaps(&series, 0, 141).1, [(91, 100), (102, 141)]);
}

#[test]
fn reopen_takes_flags_from_the_resident_entry_under_a_shared_key() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let (series, _, peer) = split(dir.path());
    let resident = *series.manifest().span(Timestamp(100)).unwrap();
    drop(series);
    // What an older build could persist: a stale piece keyed like a
    // resident node, sorted ahead of it.
    let stale = NodeSpan {
        cover_start: Timestamp(100),
        cover_end: Timestamp(101),
        ..NodeSpan::whole(peer, SpanState::RemoteOnly, SpanSource::RemoteFetch, true)
    };
    let acked = NodeSpan {
        acked: true,
        ..resident
    };
    ComponentManifest {
        spans: vec![stale, acked].into_boxed_slice(),
        ..Default::default()
    }
    .write_to(&cdir)
    .unwrap();
    let series = TimeSeries::open(&cdir).unwrap();
    let manifest = series.manifest();
    let span = manifest.span(Timestamp(100)).unwrap();
    assert_eq!(span.state, SpanState::Resident);
    assert_eq!(span.source, SpanSource::Mirrored);
    assert!(span.acked);
}

#[test]
fn ties_at_peer_node_boundaries_keep_both_sides() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    std::fs::create_dir_all(&cdir).unwrap();
    let series = TimeSeries::create(&cdir).unwrap();
    let a = peer_node(&dir.path().join("pa"), &[10, 20]);
    let b = peer_node(&dir.path().join("pb"), &[20, 30]);
    let (sa, sb) = (
        SealRecord::compute(&a).unwrap(),
        SealRecord::compute(&b).unwrap(),
    );
    assert_eq!(series.merge_remote_spans([sa, sb]).unwrap(), 2);
    series.begin_fetch(Timestamp(10)).unwrap();
    series.install_fetched(stage(&cdir, &a, &sa), &sa).unwrap();
    let (_, open) = gaps(&series, 0, 100);
    assert_eq!(open, [(20, 31)]);
    series.begin_fetch(Timestamp(20)).unwrap();
    series.install_fetched(stage(&cdir, &b, &sb), &sb).unwrap();
    assert_eq!(stamps(&series, 0, 100), [10, 20, 20, 30]);
    assert_eq!(gaps(&series, 0, 100).0, Coverage::Complete);
}

#[test]
fn duplicates_do_not_hide_a_missing_sample() {
    let dir = tempfile::tempdir().unwrap();
    // The mirror saw 20 twice and missed 30, across a reconnect.
    let series = mirrored(&dir.path().join("c"), &[&[0, 10, 20, 20], &[40]]);
    let src = peer_node(&dir.path().join("peer"), &[0, 10, 20, 30, 40]);
    let peer = SealRecord::compute(&src).unwrap();
    series.merge_remote_spans([peer]).unwrap();
    assert_eq!(gaps(&series, 0, 41).1, [(21, 40)]);
    series.begin_fetch(Timestamp(21)).unwrap();
    series
        .install_fetched(stage(&dir.path().join("c"), &src, &peer), &peer)
        .unwrap();
    assert_eq!(
        series.get(Timestamp(30)).unwrap().data(),
        30u64.to_le_bytes()
    );
}

#[test]
fn settled_stretches_are_not_fetched_again_after_a_purge() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let series = mirrored(
        &cdir,
        &[&[0, 10, 20, 30, 40, 60, 70, 80, 90, 100], &[103, 105]],
    );
    let src = peer_node(&dir.path().join("peer"), &every_10(140));
    let peer = SealRecord::compute(&src).unwrap();
    series.merge_remote_spans([peer]).unwrap();
    assert_eq!(gaps(&series, 0, 141).1, [(101, 103), (106, 141)]);
    series.begin_fetch(Timestamp(101)).unwrap();
    series
        .install_fetched(stage(&cdir, &src, &peer), &peer)
        .unwrap();
    assert_eq!(gaps(&series, 0, 141).0, Coverage::Complete);
    assert_eq!(&*series.manifest().shadowed, &[peer]);

    // Only the purged node's own range is offered again, not the
    // sample-free stretch a fetch already settled beside it.
    assert!(series.purge_span(Timestamp(103)).unwrap());
    assert_eq!(gaps(&series, 0, 141).1, [(103, 106)]);
    series.begin_fetch(Timestamp(103)).unwrap();
    series
        .install_fetched(stage(&cdir, &src, &peer), &peer)
        .unwrap();
    assert_eq!(gaps(&series, 0, 141).0, Coverage::Complete);

    // Settled stretches survive a reopen.
    drop(series);
    let series = TimeSeries::open(&cdir).unwrap();
    assert_eq!(gaps(&series, 0, 141).0, Coverage::Complete);
}

#[test]
fn crash_mid_mirrored_purge_reopens_fetchable() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let (series, _, _) = split(dir.path());
    let before = std::fs::read(cdir.join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(series.purge_span(Timestamp(100)).unwrap());
    drop(series);
    // The node is removed before the manifest is published: a crash in
    // between leaves the old manifest over a missing node.
    std::fs::write(cdir.join(crate::manifest::MANIFEST_FILE), before).unwrap();
    let series = TimeSeries::open(&cdir).unwrap();
    assert!(series.manifest().span(Timestamp(100)).is_none());
    assert_eq!(gaps(&series, 0, 141).1, [(0, 141)]);
}

/// A crash after a purge persisted a hydrated span as remote-only, but
/// before its node was deleted, reopens the node with its source and ack,
/// so it is purged again.
#[test]
fn crash_mid_acked_purge_reopens_purgeable() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let manifest = cdir.join(crate::manifest::MANIFEST_FILE);
    let src = peer_node(&dir.path().join("peer"), &every_10(40));
    let seal = SealRecord::compute(&src).unwrap();
    let series = TimeSeries::create(&cdir).unwrap();
    let install = || {
        let staging = stage(&cdir, &src, &seal);
        series
            .install_node(staging, &seal, SpanSource::RemoteFetch)
            .unwrap();
    };
    install();
    assert!(series.purge_span(Timestamp(0)).unwrap());
    let purged = std::fs::read(&manifest).unwrap();
    install();
    drop(series);
    std::fs::write(&manifest, purged).unwrap();
    // Only the manifest speaks for the node's source here.
    std::fs::remove_file(cdir.join("0").join("fetched")).unwrap();
    let series = TimeSeries::open(&cdir).unwrap();
    let span = *series.manifest().span(Timestamp(0)).unwrap();
    assert_eq!(span.state, SpanState::Resident);
    assert_eq!(span.source, SpanSource::RemoteFetch);
    assert!(span.acked);
    assert!(series.purge_span(Timestamp(0)).unwrap());
}

/// An installed node whose manifest never reached disk reopens as fetched
/// data, not local ingest.
#[test]
fn a_node_installed_before_a_crash_keeps_its_source() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let src = peer_node(&dir.path().join("peer"), &every_10(40));
    let seal = SealRecord::compute(&src).unwrap();
    let series = TimeSeries::create(&cdir).unwrap();
    series
        .install_node(stage(&cdir, &src, &seal), &seal, SpanSource::RemoteFetch)
        .unwrap();
    drop(series);
    std::fs::remove_file(cdir.join(crate::manifest::MANIFEST_FILE)).unwrap();
    let series = TimeSeries::open(&cdir).unwrap();
    let span = *series.manifest().span(Timestamp(0)).unwrap();
    assert_eq!(span.source, SpanSource::RemoteFetch);
}

#[test]
fn a_node_sealed_after_restart_keeps_its_mirrored_tag() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    {
        let series = TimeSeries::create(&cdir).unwrap();
        let mut writer = series.writer().unwrap();
        series.enter_mirror_run(MirrorRun::next());
        writer.push_buf(Timestamp(100), &[0; 8]).unwrap();
        series.enter_mirror_run(MirrorRun::next());
        writer.push_buf(Timestamp(200), &[0; 8]).unwrap();
        // Shut down before the lifecycle task seals the rolled node.
    }
    let series = TimeSeries::open(&cdir).unwrap();
    series.seal_rolled_nodes().unwrap();
    assert_eq!(
        series.manifest().span(Timestamp(100)).unwrap().source,
        SpanSource::Mirrored
    );
}

#[test]
fn a_full_listing_forgets_seals_the_peer_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let (series, src, peer) = split(dir.path());
    let other = peer_node(&dir.path().join("other"), &[300, 310]);
    let other = SealRecord::compute(&other).unwrap();
    assert_eq!(series.sync_remote(vec![peer, other], None).unwrap(), 1);
    assert_eq!(series.sync_remote(vec![peer, other], None).unwrap(), 0);

    // Settle the peer seal, then the peer drops it.
    series.begin_fetch(Timestamp(0)).unwrap();
    series
        .install_fetched(stage(&dir.path().join("c"), &src, &peer), &peer)
        .unwrap();
    series.sync_remote(vec![other], None).unwrap();
    let manifest = series.manifest();
    assert!(manifest.shadowed.is_empty());
    assert!(manifest.settled.is_empty());
    assert!(manifest.spans.iter().all(|s| s.seal != peer));
    // Nothing vouches for the mirrored node any more.
    assert!(!series.purge_span(Timestamp(100)).unwrap());
    assert_eq!(gaps(&series, 0, 400).1, [(300, 311)]);
}

/// The reply to a range fetch of `covered`: the samples of `peer` stamped
/// inside it, valued as their timestamps like [`peer_node`]'s.
fn fetch(series: &TimeSeries, peer: &[i64], covered: std::ops::Range<i64>) -> Vec<(i64, i64)> {
    let stamps: Vec<Timestamp> = peer
        .iter()
        .filter(|t| covered.contains(t))
        .map(|t| Timestamp(*t))
        .collect();
    let data: Vec<u8> = stamps
        .iter()
        .flat_map(|t| (t.0 as u64).to_le_bytes())
        .collect();
    let covered = Timestamp(covered.start)..Timestamp(covered.end);
    series
        .install_fragment(covered, 8, &stamps, &data)
        .unwrap()
        .iter()
        .map(|r| (r.start.0, r.end.0))
        .collect()
}

fn fragment_dirs(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "fragment"))
        .count()
}

/// A remote-only peer seal over 0..=140, sampled every 10.
fn remote(dir: &Path) -> (TimeSeries, TimeSeriesNode, SealRecord) {
    let cdir = dir.join("c");
    std::fs::create_dir_all(&cdir).unwrap();
    let series = TimeSeries::create(&cdir).unwrap();
    let src = peer_node(&dir.join("peer"), &every_10(140));
    let peer = SealRecord::compute(&src).unwrap();
    series.merge_remote_spans([peer]).unwrap();
    (series, src, peer)
}

#[test]
fn a_fragment_claims_what_the_peer_vouched_for() {
    let dir = tempfile::tempdir().unwrap();
    let (series, _, _) = remote(dir.path());
    // Vouched for 15..61: its edges hold no samples, but are answered.
    assert_eq!(fetch(&series, &every_10(140), 15..61), [(15, 61)]);
    assert_eq!(gaps(&series, 0, 141).1, [(0, 15), (61, 141)]);
    assert_eq!(gaps(&series, 15, 61).0, Coverage::Complete);
    assert_eq!(stamps(&series, 0, 141), [20, 30, 40, 50, 60]);
    assert_eq!(
        series.get(Timestamp(30)).unwrap().data(),
        30u64.to_le_bytes()
    );
    // The span itself stays remote-only; only coverage stops asking.
    assert_eq!(
        series.manifest().span(Timestamp(0)).unwrap().state,
        SpanState::RemoteOnly
    );
    // Asking again claims only what is still missing.
    assert_eq!(fetch(&series, &every_10(140), 0..100), [(0, 15), (61, 100)]);
    assert_eq!(gaps(&series, 0, 141).1, [(100, 141)]);
    assert_eq!(stamps(&series, 0, 141), every_10(90));
}

#[test]
fn a_stretch_checked_and_empty_counts_as_covered() {
    let dir = tempfile::tempdir().unwrap();
    let (series, _, _) = remote(dir.path());
    assert_eq!(fetch(&series, &[], 41..50), [(41, 50)]);
    assert_eq!(gaps(&series, 41, 50).0, Coverage::Complete);
    assert_eq!(fragment_dirs(&dir.path().join("c")), 0);
    let manifest = series.manifest();
    assert!(manifest.fragments[0].node.is_none());
}

/// A point read's reply holds the sample before the instant and vouches
/// back to just past the one before that, so a reader at any instant in
/// between finds its predecessor held.
#[test]
fn point_reads_see_fragment_samples() {
    let dir = tempfile::tempdir().unwrap();
    let (series, _, _) = remote(dir.path());
    fetch(&series, &every_10(140), 51..76);
    let before = |t: i64| {
        let slice = series.get_range(Timestamp(i64::MIN)..Timestamp(t))?;
        let node = slice.as_iter().next()?;
        node.timestamps().last().map(|t| t.0)
    };
    assert_eq!(before(75), Some(70));
    assert_eq!(before(60), Some(60));
    assert_eq!(gaps(&series, 51, 76).0, Coverage::Complete);
    assert_eq!(
        series.get(Timestamp(70)).unwrap().data(),
        70u64.to_le_bytes()
    );
}

#[test]
fn a_whole_node_supersedes_its_fragments() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let (series, src, peer) = remote(dir.path());
    fetch(&series, &every_10(140), 15..61);
    fetch(&series, &[], 100..111);
    assert_eq!(fragment_dirs(&cdir), 1);
    // The fragments don't hide the span from a whole-node fetch.
    let claim = series.begin_fetch(Timestamp(0)).unwrap();
    assert!(claim.is_whole());
    series
        .install_fetched(stage(&cdir, &src, &peer), &peer)
        .unwrap();
    let manifest = series.manifest();
    assert!(manifest.fragments.is_empty());
    assert_eq!(
        manifest.span(Timestamp(0)).unwrap().state,
        SpanState::Resident
    );
    assert_eq!(stamps(&series, 0, 141), every_10(140));
    assert_eq!(series.list.iter().count(), 1);
    assert_eq!(fragment_dirs(&cdir), 0);
}

/// Fragments over a split seal give way to the pieces that land, and the
/// mirrored node between them is untouched.
#[test]
fn a_split_install_supersedes_fragments_over_its_pieces() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let (series, src, peer) = split(dir.path());
    assert_eq!(fetch(&series, &every_10(140), 15..61), [(15, 61)]);
    // Never claimed across the mirrored node.
    assert_eq!(
        fetch(&series, &every_10(140), 95..131),
        [(95, 100), (102, 131)]
    );
    assert_eq!(stamps(&series, 90, 135), [100, 101, 110, 120, 130]);
    series.begin_fetch(Timestamp(0)).unwrap();
    series
        .install_fetched(stage(&cdir, &src, &peer), &peer)
        .unwrap();
    assert!(series.manifest().fragments.is_empty());
    let mut expected = every_10(140);
    expected.insert(expected.binary_search(&100).unwrap() + 1, 101);
    expected.push(500);
    assert_eq!(stamps(&series, 0, 501), expected);
    assert_eq!(fragment_dirs(&cdir), 0);
}

/// A range fetch landing while a whole node is mid-fetch leaves the span
/// to that fetch.
#[test]
fn fragments_leave_a_fetch_in_flight_alone() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let (series, src, peer) = remote(dir.path());
    series.begin_fetch(Timestamp(0)).unwrap();
    assert_eq!(fetch(&series, &every_10(140), 15..61), []);
    assert!(series.manifest().fragments.is_empty());
    series
        .install_fetched(stage(&cdir, &src, &peer), &peer)
        .unwrap();
    assert_eq!(stamps(&series, 0, 141), every_10(140));
}

/// A fetch that fails after fragments landed under its claim leaves them,
/// and the span can be claimed again.
#[test]
fn an_aborted_fetch_keeps_fragments_that_landed_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let (series, _, _) = remote(dir.path());
    fetch(&series, &every_10(140), 15..61);
    series.begin_fetch(Timestamp(0)).unwrap();
    series.abort_fetch(Timestamp(0));
    assert_eq!(gaps(&series, 0, 141).1, [(0, 15), (61, 141)]);
    assert!(series.begin_fetch(Timestamp(0)).is_some());
}

#[test]
fn evicting_a_fragment_gives_its_range_back() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let (series, _, _) = remote(dir.path());
    fetch(&series, &every_10(140), 15..61);
    fetch(&series, &[], 100..111);
    let now = std::time::Instant::now();
    let grace = std::time::Duration::from_secs(5);
    // Freshly read: kept.
    let kept = crate::evict::evict(&series, now, grace, |_| true).unwrap();
    assert_eq!(kept.spans, 0);
    // Touched later, only the empty one goes idle.
    let later = now + grace * 2;
    series.touch(Timestamp(30)..Timestamp(31), later);
    let evicted = crate::evict::evict(&series, later, grace, |_| true).unwrap();
    assert_eq!(evicted.spans, 1);
    assert_eq!(evicted.bytes, 0);
    assert_eq!(gaps(&series, 0, 141).1, [(0, 15), (61, 141)]);
    let evicted = crate::evict::evict(&series, later + grace * 2, grace, |_| true).unwrap();
    assert_eq!(evicted.spans, 1);
    assert!(evicted.bytes > 0);
    assert_eq!(gaps(&series, 0, 141).1, [(0, 141)]);
    assert!(stamps(&series, 0, 141).is_empty());
    assert_eq!(fragment_dirs(&cdir), 0);
}

#[test]
fn purge_span_on_a_fragment_drops_it() {
    let dir = tempfile::tempdir().unwrap();
    let (series, _, _) = remote(dir.path());
    fetch(&series, &every_10(140), 15..61);
    // Keyed by its node's first sample, as a list walk would find it.
    assert!(series.purge_span(Timestamp(20)).unwrap());
    assert!(series.manifest().fragments.is_empty());
    assert!(stamps(&series, 0, 141).is_empty());
    assert!(!series.purge_span(Timestamp(15)).unwrap());
}

/// Once the peer stops listing the seal, nothing can give a fragment's
/// samples back, so eviction leaves it.
#[test]
fn a_fragment_of_a_seal_the_peer_dropped_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let (series, _, _) = remote(dir.path());
    fetch(&series, &every_10(140), 15..61);
    series.sync_remote(vec![], None).unwrap();
    assert!(!series.purge_fragment(Timestamp(15)).unwrap());
    assert_eq!(stamps(&series, 0, 141), [20, 30, 40, 50, 60]);
}

#[test]
fn fragments_last_one_run() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    let (series, _, _) = remote(dir.path());
    fetch(&series, &every_10(140), 15..61);
    drop(series);
    let series = TimeSeries::open(&cdir).unwrap();
    assert_eq!(fragment_dirs(&cdir), 0);
    assert!(stamps(&series, 0, 141).is_empty());
    assert_eq!(gaps(&series, 0, 141).1, [(0, 141)]);
}

/// Above an unsealed head nothing may land, fragments included.
#[test]
fn fragments_stay_below_an_unsealed_head() {
    let dir = tempfile::tempdir().unwrap();
    let (series, _, _) = remote(dir.path());
    let mut writer = series.writer().unwrap();
    series.enter_mirror_run(MirrorRun::next());
    writer
        .push_buf(Timestamp(100), &100u64.to_le_bytes())
        .unwrap();
    assert_eq!(fetch(&series, &every_10(140), 0..141), [(0, 100)]);
    assert_eq!(stamps(&series, 0, 141), every_10(100));
    // The writer keeps appending to its head, not to the fragment.
    writer
        .push_buf(Timestamp(105), &105u64.to_le_bytes())
        .unwrap();
    assert_eq!(series.list.iter().count(), 2);
}

/// In an archive without a writer, a fragment can become the newest node;
/// a writer arriving later rolls a node above it rather than appending,
/// and sealing never takes it for local data.
#[test]
fn a_fragment_at_the_head_is_never_written_or_sealed() {
    let dir = tempfile::tempdir().unwrap();
    let (series, _, _) = remote(dir.path());
    fetch(&series, &every_10(140), 125..141);
    let mut writer = series.writer().unwrap();
    writer
        .push_buf(Timestamp(200), &200u64.to_le_bytes())
        .unwrap();
    writer
        .push_buf(Timestamp(300), &300u64.to_le_bytes())
        .unwrap();
    drop(writer);
    series.seal_rolled_nodes().unwrap();
    let manifest = series.manifest();
    assert!(manifest.span(Timestamp(130)).is_none());
    assert_eq!(manifest.fragments.len(), 1);
    assert_eq!(stamps(&series, 0, 400), [130, 140, 200, 300]);
    // Nor are its samples counted twice in estimates.
    assert_eq!(
        series.estimate_samples(Timestamp(125)..Timestamp(141)),
        series
            .manifest()
            .count_in(&(Timestamp(125)..Timestamp(141)))
    );
}

/// Where a whole peer node ends on a timestamp its neighbour's seal also
/// holds, a range fetch over the neighbour claims the shared timestamp and
/// keeps the neighbour's samples there, dropping only as many as the node
/// holds itself, so reads match the origin's. Purging the node takes such
/// a fragment with it, so the shared timestamp is offered again.
#[test]
fn a_fragment_on_a_nodes_edge_goes_with_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    std::fs::create_dir_all(&cdir).unwrap();
    let series = TimeSeries::create(&cdir).unwrap();
    let a = peer_node(&dir.path().join("pa"), &[10, 20]);
    let b = peer_node(&dir.path().join("pb"), &[20, 30]);
    let (sa, sb) = (
        SealRecord::compute(&a).unwrap(),
        SealRecord::compute(&b).unwrap(),
    );
    series.merge_remote_spans([sa, sb]).unwrap();
    series.begin_fetch(Timestamp(10)).unwrap();
    series.install_fetched(stage(&cdir, &a, &sa), &sa).unwrap();
    assert_eq!(gaps(&series, 0, 100).1, [(20, 31)]);
    // The peer replies with both 20s, a's first; b's 20 and 30 are new.
    let reply = [Timestamp(20), Timestamp(20), Timestamp(30)];
    let data: Vec<u8> = [20u64, 2020, 30]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    series
        .install_fragment(Timestamp(20)..Timestamp(31), 8, &reply, &data)
        .unwrap();
    assert_eq!(gaps(&series, 0, 100).0, Coverage::Complete);
    assert_eq!(stamps(&series, 0, 100), [10, 20, 20, 30]);
    // The newer node's sample answers at the tie, as on the origin.
    assert_eq!(
        series.get(Timestamp(20)).unwrap().data(),
        2020u64.to_le_bytes()
    );
    let manifest = series.manifest();
    let held = manifest.span(Timestamp(10)).unwrap().bytes()
        + manifest.fragments.iter().map(|f| f.bytes()).sum::<u64>();
    // The purge reports the fragment's bytes along with the node's.
    assert_eq!(series.purge_span_freed(Timestamp(10)).unwrap(), Some(held));
    assert!(series.manifest().fragments.is_empty());
    assert_eq!(gaps(&series, 0, 100).1, [(10, 21), (20, 31)]);
}

/// The other side of a tie: a fragment ending on the first timestamp of a
/// resident node keeps the older node's samples there.
#[test]
fn a_fragment_before_a_node_keeps_the_older_side_of_a_tie() {
    let dir = tempfile::tempdir().unwrap();
    let cdir = dir.path().join("c");
    std::fs::create_dir_all(&cdir).unwrap();
    let series = TimeSeries::create(&cdir).unwrap();
    let a = peer_node(&dir.path().join("pa"), &[10, 20]);
    let b = peer_node(&dir.path().join("pb"), &[20, 30]);
    let (sa, sb) = (
        SealRecord::compute(&a).unwrap(),
        SealRecord::compute(&b).unwrap(),
    );
    series.merge_remote_spans([sa, sb]).unwrap();
    series.begin_fetch(Timestamp(20)).unwrap();
    series.install_fetched(stage(&cdir, &b, &sb), &sb).unwrap();
    let reply = [Timestamp(10), Timestamp(20), Timestamp(20)];
    let data: Vec<u8> = [10u64, 1020, 20]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    series
        .install_fragment(Timestamp(0)..Timestamp(21), 8, &reply, &data)
        .unwrap();
    assert_eq!(gaps(&series, 0, 100).0, Coverage::Complete);
    assert_eq!(stamps(&series, 0, 100), [10, 20, 20, 30]);
}
