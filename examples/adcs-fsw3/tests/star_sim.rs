//! `sim.py` with the star simulator: for a few seconds of wall-clock run, its
//! db records every component `presets/star-tracker.json` binds, a
//! `StarImage` every 100 ms, and the star tracker's vectors from them.

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
use metor_proto_wkt::{DumpMetadata, GetTimeSeries};
use serde_json::Value;
use startracker_sim::StarSim;

const RUN: Duration = Duration::from_secs(4);

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// `sim.py`'s target with its db on `db` and stored under `path`.
fn config(db: SocketAddr, path: &Path) -> CoordinatorConfig {
    let deployment = prepare(Some(&manifest().join("sim.py"))).expect("sim.py evaluates");
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

/// Every component the preset binds: the image view's image and overlay
/// fields by name, the plots' traces by id.
fn preset_components() -> Vec<ComponentId> {
    fn walk(node: &Value, out: &mut Vec<ComponentId>) {
        if let Some(children) = node.pointer("/Split/children").and_then(Value::as_array) {
            children.iter().for_each(|child| walk(child, out));
        }
        let items = node.pointer("/Pane/items").and_then(Value::as_array);
        for item in items.into_iter().flatten() {
            let state: Value = serde_json::from_str(item["state"].as_str().unwrap()).unwrap();
            for overlay in state["overlays"].as_array().into_iter().flatten() {
                for field in ["u", "v", "count", "size", "cat_id"] {
                    out.push(ComponentId::new(overlay[field].as_str().unwrap()));
                }
            }
            if let Some(image) = state["component"].as_str() {
                out.push(ComponentId::new(image));
            }
            for trace in state["traces"].as_array().into_iter().flatten() {
                out.push(ComponentId(trace["component_id"].as_u64().unwrap()));
            }
        }
    }
    let preset = std::fs::read_to_string(manifest().join("presets/star-tracker.json")).unwrap();
    let mut out = Vec::new();
    walk(
        &serde_json::from_str::<Value>(&preset).unwrap()["root"],
        &mut out,
    );
    out
}

/// Every sample of a `u64` component as `(timestamp µs, value)`, or none
/// before the first.
async fn u64s(client: &mut Client, component: &str) -> Vec<(i64, u64)> {
    let query = GetTimeSeries {
        id: [0, 0],
        range: Timestamp(0)..Timestamp(i64::MAX),
        component_id: ComponentId::new(component),
        limit: None,
    };
    let Ok(series) = client.request(&query).await else {
        return Vec::new(); // no samples yet
    };
    let (stamps, data) = (series.timestamps().unwrap(), series.data().unwrap());
    let values = data
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| u64::from_le_bytes(*b));
    stamps.iter().map(|t| t.0).zip(values).collect()
}

fn free_port() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    listener.local_addr().expect("an address")
}

#[test]
fn sim_py_records_the_star_tracker() {
    let wanted = preset_components();
    assert!(wanted.len() > 10, "the preset binds {wanted:?}");
    let dir = tempfile::tempdir().expect("a temp dir");
    let db = free_port();
    let stop = Arc::new(AtomicBool::new(false));
    let run = spawn(config(db, &dir.path().join("db")), stop.clone());
    let stopper = stop.clone();
    stellarator::run(move || async move {
        stellarator::sleep(RUN).await;
        let mut client = Client::connect(db).await.expect("the db takes clients");
        let dump = client.request(&DumpMetadata).await.expect("the db answers");
        let images = u64s(&mut client, "sim.star_sim.star_image.frame_id").await;
        // A debug build's lost-in-space searches lag the images by seconds.
        let mut vectors = Vec::new();
        for _ in 0..100 {
            vectors = u64s(&mut client, "sim.star_tracker.star_vectors.count").await;
            if vectors.len() >= images.len() / 4 {
                break;
            }
            stellarator::sleep(Duration::from_millis(100)).await;
        }
        stopper.store(true, Ordering::Release);

        let ids: Vec<ComponentId> = dump
            .component_metadata
            .iter()
            .map(|m| m.component_id)
            .collect();
        let missing: Vec<_> = wanted.iter().filter(|id| !ids.contains(id)).collect();
        assert!(missing.is_empty(), "never announced {missing:?}");

        // Frame ids rise by one per 100 ms of sim time. A debug build may
        // drop a few frames while the target starts, and the first frame
        // can render late.
        assert!(images.len() >= 25, "{} frames in {RUN:?}", images.len());
        assert!(images.windows(2).all(|w| w[1].1 > w[0].1), "{images:?}");
        let ((t0, id0), (t1, id1)) = (images[1], images[images.len() - 1]);
        let period = (t1 - t0) as f64 / (id1 - id0) as f64;
        assert!(
            (period - 100_000.0).abs() < 1_000.0,
            "{period} µs per frame"
        );

        // These seconds are the detumble from 0.15 rad/s, where a 100 ms
        // exposure streaks a star over tens of pixels and a frame rightly
        // fails, so only a share of the frames give vectors, each with
        // several stars.
        let counts: Vec<u64> = vectors.iter().map(|v| v.1).collect();
        assert!(
            counts.len() >= images.len() / 4,
            "{counts:?} of {} frames",
            images.len()
        );
        assert!(counts.iter().all(|&n| n >= 4), "{counts:?}");
    });
    run.join().expect("the target ran");
}
