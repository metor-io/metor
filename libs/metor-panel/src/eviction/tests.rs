use super::*;
use crate::views::plot_common::lod::{GapCollector, LodTracking};
use metor_db::manifest::{SpanSource, SpanState};
use metor_db::{Component, ComponentSchema};
use metor_proto::types::{PrimType, Timestamp};

/// How long these tests wait for setup-time stamps to go idle.
const GRACE: Duration = Duration::from_millis(50);
const WORKING_SET: Eviction = Eviction::WorkingSet { grace: GRACE };

fn budget(max_bytes: u64) -> Eviction {
    Eviction::Budget { max_bytes }
}

fn component(db: &DB, name: &str, dim: &[usize], metadata: &[(&str, String)]) -> Component {
    let id = ComponentId::new(name);
    db.with_state_mut(|state| {
        state.insert_component(id, ComponentSchema::new(PrimType::F64, dim), &db.path)
    })
    .unwrap();
    db.with_state_mut(|state| {
        state.set_component_metadata(
            metor_proto_wkt::ComponentMetadata {
                component_id: id,
                name: name.to_string(),
                metadata: metadata
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            },
            &db.path,
        )
    })
    .unwrap();
    db.with_state(|state| state.get_component(id).cloned())
        .unwrap()
}

/// Hydrate `stamps` into `component` as acked peer history.
fn hydrate(component: &Component, stamps: impl Iterator<Item = i64>) {
    let size = component.schema.size();
    let samples: Vec<(Timestamp, Vec<u8>)> =
        stamps.map(|t| (Timestamp(t), vec![0; size])).collect();
    component
        .time_series
        .install_samples(
            size,
            samples.iter().map(|(t, b)| (*t, b.as_slice())),
            SpanSource::RemoteFetch,
        )
        .unwrap();
}

fn resident(component: &Component, start: i64) -> bool {
    component
        .time_series
        .manifest()
        .span(Timestamp(start))
        .is_some_and(|span| span.state == SpanState::Resident)
}

fn level(db: &DB, source: &Component, ratio: u64) -> Component {
    let level = component(
        db,
        &format!("rpm.lod_{ratio}"),
        &[2],
        &[
            (LOD_SOURCE_ID_KEY, source.component_id.0.to_string()),
            (LOD_RATIO_KEY, ratio.to_string()),
        ],
    );
    // Buckets past the raw history, so no raw tail draws over them.
    hydrate(&level, (0..4096).step_by(ratio as usize));
    level
}

#[test]
fn eviction_needs_every_connection_up_and_one_to_fetch() {
    use ConnectionStatus::*;
    assert!(!peers_can_refetch([]), "nothing to fetch back from");
    assert!(peers_can_refetch([(Connected, true)]));
    assert!(peers_can_refetch([(Connected, true), (Connected, false)]));
    assert!(
        !peers_can_refetch([(Connected, false)]),
        "a local producer holds no copy"
    );
    for down in [Connecting, Reconnecting, Failed("x".into()), Disconnected] {
        assert!(!peers_can_refetch([(Connected, true), (down, true)]));
    }
}

#[test]
fn the_coarsest_level_of_each_source_is_kept() {
    let temp = tempfile::tempdir().unwrap();
    let db = DB::create(temp.path().join("db")).unwrap();
    let rpm = component(&db, "rpm", &[], &[]);
    let alt = component(&db, "alt", &[], &[]);
    let fine = level(&db, &rpm, 64);
    let coarse = level(&db, &rpm, 1024);
    let only = component(
        &db,
        "alt.lod_64",
        &[2],
        &[
            (LOD_SOURCE_ID_KEY, alt.component_id.0.to_string()),
            (LOD_RATIO_KEY, "64".to_string()),
        ],
    );
    let kept = coarsest_lod_levels(&db);
    assert!(kept.contains(&coarse.component_id));
    assert!(kept.contains(&only.component_id));
    assert!(!kept.contains(&fine.component_id));
    assert_eq!(kept.len(), 2);
}

/// A plot reading raw keeps raw and lets the unread level go; zoomed out
/// onto a level it keeps that level and lets the raw history go — and the
/// coarsest level survives either way.
#[gpui::test]
fn a_plot_keeps_what_it_draws(cx: &mut gpui::TestAppContext) {
    let temp = tempfile::tempdir().unwrap();
    let db = DB::create(temp.path().join("db")).unwrap();
    let rpm = component(&db, "rpm", &[], &[]);
    hydrate(&rpm, 0..1000);
    hydrate(&rpm, 1000..2000);
    let fine = level(&db, &rpm, 64);
    let coarse = level(&db, &rpm, 1024);
    let source = rpm.component_id;
    let window = Timestamp(1000)..Timestamp(2000);

    let frame = |lod: &LodTracking, cx: &mut gpui::TestAppContext| {
        cx.update(|cx| {
            let mut gaps = GapCollector::new(1000.0, 2000.0, cx);
            gaps.add(&rpm, source, lod, &None, cx);
        })
    };

    // Zoomed in: the raw window draws, nothing else is read.
    let mut lod = LodTracking::default();
    lod.select(&db, source, &rpm, None, window.clone(), 10_000, 1);
    assert!(lod.selected().is_none());
    std::thread::sleep(GRACE * 2);
    frame(&lod, cx);
    let evicted = purge(&db, WORKING_SET, Instant::now());
    assert_eq!(
        evicted.spans, 2,
        "the raw span off screen and the unread level"
    );
    assert!(!resident(&rpm, 0));
    assert!(resident(&rpm, 1000));
    assert!(!resident(&fine, 0));
    assert!(resident(&coarse, 0), "the coarsest level stays");

    // Zoomed out onto the coarse level: raw is no longer read.
    lod.select(&db, source, &rpm, None, window, 10, 1);
    assert_eq!(
        lod.selected().map(|c| c.component_id),
        Some(coarse.component_id),
        "the finest level that fits"
    );
    std::thread::sleep(GRACE * 2);
    frame(&lod, cx);
    assert!(due(&db, WORKING_SET, Instant::now()));
    assert_eq!(purge(&db, WORKING_SET, Instant::now()).spans, 1);
    assert!(!resident(&rpm, 1000));
    assert!(resident(&coarse, 0));
    assert!(!due(&db, WORKING_SET, Instant::now()));
}

/// Over a budget the least recently read goes first, until the rest fits;
/// the coarsest level neither goes nor counts.
#[test]
fn a_budget_keeps_the_most_recently_read() {
    let temp = tempfile::tempdir().unwrap();
    let db = DB::create(temp.path().join("db")).unwrap();
    let rpm = component(&db, "rpm", &[], &[]);
    hydrate(&rpm, 0..1000);
    hydrate(&rpm, 1000..2000);
    let fine = level(&db, &rpm, 64);
    let coarse = level(&db, &rpm, 1024);
    let span = rpm
        .time_series
        .manifest()
        .span(Timestamp(0))
        .unwrap()
        .bytes();
    // Read after everything was created, but long enough ago that the
    // grace period no longer shields it.
    let later = Instant::now() + evict::DEFAULT_GRACE * 4;
    rpm.time_series.touch(
        Timestamp(1000)..Timestamp(2000),
        later - evict::DEFAULT_GRACE * 2,
    );

    assert!(!due(&db, budget(u64::MAX), Instant::now()));
    assert!(due(&db, budget(span), Instant::now()));
    let evicted = purge(&db, budget(span), later);
    assert_eq!(evicted.spans, 2, "the unread raw span and the fine level");
    assert!(!resident(&rpm, 0));
    assert!(!resident(&fine, 0));
    assert!(resident(&rpm, 1000), "the most recently read fits");
    assert!(resident(&coarse, 0), "the coarsest level stays");
    assert!(!due(&db, budget(span), Instant::now()));
}
