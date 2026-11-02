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

fn peer_node(dir: &Path, stamps: &[i64]) -> TimeSeriesNode {
    let src =
        TimeSeriesNode::create(dir.join(stamps[0].to_string()), Timestamp(stamps[0]), 8).unwrap();
    for ts in stamps {
        src.data.write(&(*ts as u64).to_le_bytes()).unwrap();
        src.index.write(&Timestamp(*ts).to_le_bytes()).unwrap();
    }
    src
}

fn every_10(to: i64) -> Vec<i64> {
    (0..=to).step_by(10).collect()
}

/// Stage `src` in full, as a fetch would.
fn stage(dir: &Path, src: &TimeSeriesNode, seal: &SealRecord) -> NodeStaging {
    let staging = NodeStaging::create(dir, seal).unwrap();
    staging
        .append_file(NodeFile::Index, src.index.data())
        .unwrap();
    staging
        .append_file(NodeFile::Data, src.data.data())
        .unwrap();
    staging
}

fn stamps(series: &TimeSeries, start: i64, end: i64) -> Vec<i64> {
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
    assert_eq!(series.sync_remote_spans(vec![peer, other]).unwrap(), 1);
    assert_eq!(series.sync_remote_spans(vec![peer, other]).unwrap(), 0);

    // Settle the peer seal, then the peer drops it.
    series.begin_fetch(Timestamp(0)).unwrap();
    series
        .install_fetched(stage(&dir.path().join("c"), &src, &peer), &peer)
        .unwrap();
    series.sync_remote_spans(vec![other]).unwrap();
    let manifest = series.manifest();
    assert!(manifest.shadowed.is_empty());
    assert!(manifest.settled.is_empty());
    assert!(manifest.spans.iter().all(|s| s.seal != peer));
    // Nothing vouches for the mirrored node any more.
    assert!(!series.purge_span(Timestamp(100)).unwrap());
    assert_eq!(gaps(&series, 0, 400).1, [(300, 311)]);
}
