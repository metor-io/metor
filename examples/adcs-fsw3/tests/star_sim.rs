//! `sim.py` with the star simulator: for a few seconds of wall-clock run, its
//! db holds a `StarImage` and a `StarTruth` every 100 ms, and the star
//! tracker's vectors and centroids from them.

use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use metor_fsw_3::Timestamp;
use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::cli::run::prepare;
use metor_fsw_3::coordinator::CoordinatorConfig;
use metor_proto::types::ComponentId;
use metor_proto_stellar::Client;
use metor_proto_wkt::GetTimeSeries;
use startracker_sim::StarSim;

const PIXELS: &str = "sim.star_sim.star_image.pixels";
const IMAGE_ID: &str = "sim.star_sim.star_image.frame_id";
const TRUTH_ID: &str = "sim.star_sim.star_truth.frame_id";
const TRUTH_COUNT: &str = "sim.star_sim.star_truth.count";
const VECTOR_COUNT: &str = "sim.star_tracker.star_vectors.count";
const CENTROID_COUNT: &str = "sim.star_tracker.star_centroids.count";
const SOLUTION_MATCHED: &str = "sim.star_tracker.star_solution.n_matched";
const PIXEL_BYTES: usize = 2 << 20;
const RUN: Duration = Duration::from_secs(4);

fn free_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.local_addr().expect("an address")
}

/// `sim.py`'s target with its db on `db` and stored under `path`.
fn config(db: SocketAddr, path: &Path) -> CoordinatorConfig {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let deployment = prepare(Some(&manifest.join("sim.py"))).expect("sim.py evaluates");
    let mut config = deployment
        .targets
        .into_iter()
        .next()
        .expect("one target")
        .coordinator;
    for system in &mut config.systems {
        for pack in ["adcs.", "startracker_sim."] {
            if let Some(ty) = system.ty.strip_prefix(pack) {
                system.ty = ty.into();
            }
        }
        if system.id == "db" {
            system.params["listen"] = db.to_string().into();
            system.params["path"] = path.to_string_lossy().into_owned().into();
        }
    }
    config
}

/// Runs `config` on its own thread until `stop` is set.
fn spawn(config: CoordinatorConfig, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut table = adcs_systems::pack();
        table
            .register_async(
                "star_sim",
                startracker_sim::system::star_sim(),
                StarSim::new,
            )
            .expect("valid records");
        register_builtins(&mut table).expect("valid records");
        let mut coordinator = config.build(&table).expect("the config builds");
        stellarator::run(move || async move {
            let stopped = async move {
                while !stop.load(Ordering::Acquire) {
                    stellarator::sleep(Duration::from_millis(10)).await;
                }
            };
            coordinator.run(stopped).await;
        });
    })
}

/// Every sample of `component` as `(timestamp µs, bytes)`.
async fn series(client: &mut Client, component: &str, size: usize) -> Vec<(i64, Vec<u8>)> {
    let query = GetTimeSeries {
        id: [0, 0],
        range: Timestamp(0)..Timestamp(i64::MAX),
        component_id: ComponentId::new(component),
        limit: None,
    };
    let series = client.request(&query).await.expect("a series");
    let stamps = series.timestamps().expect("timestamps");
    let data = series.data().expect("data");
    assert_eq!(data.len(), stamps.len() * size, "{component}");
    let samples = data.chunks(size).map(<[u8]>::to_vec);
    stamps.iter().map(|t| t.0).zip(samples).collect()
}

fn u64s(samples: &[(i64, Vec<u8>)]) -> Vec<u64> {
    let le = |b: &[u8]| u64::from_le_bytes(b.try_into().expect("8 bytes"));
    samples.iter().map(|(_, b)| le(b)).collect()
}

#[test]
fn sim_py_records_star_images_at_10_hz() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let db = free_port();
    let stop = Arc::new(AtomicBool::new(false));
    let run = spawn(config(db, &dir.path().join("db")), stop.clone());
    let stopper = stop.clone();
    stellarator::run(move || async move {
        stellarator::sleep(RUN).await;
        let mut client = Client::connect(db).await.expect("the db takes clients");
        let images = series(&mut client, IMAGE_ID, 8).await;
        let truths = series(&mut client, TRUTH_ID, 8).await;
        let counts = series(&mut client, TRUTH_COUNT, 8).await;
        let pixels = series(&mut client, PIXELS, PIXEL_BYTES).await;
        let vectors = series(&mut client, VECTOR_COUNT, 8).await;
        let centroids = series(&mut client, CENTROID_COUNT, 8).await;
        let matched = series(&mut client, SOLUTION_MATCHED, 2).await;
        stopper.store(true, Ordering::Release);

        // Frame ids rise by one per 100 ms of sim time. A debug build may
        // drop a few frames while the target starts.
        let ids = u64s(&images);
        assert!(ids.len() >= 25, "{} frames in {RUN:?}", ids.len());
        assert_eq!(ids.first(), Some(&0));
        for (i, (pair, id)) in images.windows(2).zip(ids.windows(2)).enumerate() {
            assert!(id[1] > id[0], "{ids:?}");
            // The first frame can render late while the target warms up.
            if i == 0 {
                continue;
            }
            let dt = pair[1].0 - pair[0].0;
            let expected = 100_000 * (id[1] - id[0]) as i64;
            // A wall-clock cycle starts up to a few ms late.
            assert!(
                (dt - expected).abs() <= 10_000,
                "{dt} µs from {} to {}",
                id[0],
                id[1]
            );
        }
        let (first, last) = (images[1].0, images[images.len() - 1].0);
        let period = (last - first) as f64 / (ids[ids.len() - 1] - ids[1]) as f64;
        assert!(
            (period - 100_000.0).abs() < 1_000.0,
            "{period} µs per frame"
        );
        let truth_ids = u64s(&truths);
        assert!(
            ids.iter()
                .take(ids.len() - 1)
                .all(|id| truth_ids.contains(id))
        );
        assert!(pixels.len() >= ids.len() - 1);
        assert!(pixels.iter().all(|(_, p)| p.iter().any(|&b| b != 0)));
        assert!(
            u64s(&counts).iter().all(|&n| n > 0),
            "a frame with no stars"
        );
        // The tracker identifies the images: most frames give vectors, each
        // with several stars.
        assert!(
            vectors.len() >= ids.len() / 2,
            "{} vector frames of {}",
            vectors.len(),
            ids.len()
        );
        assert!(
            u64s(&vectors).iter().all(|&n| n >= 4),
            "{:?}",
            u64s(&vectors)
        );
        assert!(centroids.len() >= vectors.len());
        let n_matched = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]);
        assert!(matched.iter().any(|(_, b)| n_matched(b) >= 6));
    });
    run.join().expect("the target ran");
}
