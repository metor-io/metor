//! A 1024² `Mono16<12>` image from a camera, through a `Publish`/`Subscribe`
//! link, into a db, and back out through `GetTimeSeries`.

mod common;

use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::{
    Camera, Deadline, ImageFrame, Shots, Target, camera, free_ports, local, lower_links, pattern,
};
use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::coordinator::{
    BuildError, CoordinatorConfig, InputConfig, OutputConfig, OutputSource, PortRef, SystemConfig,
    SystemTable,
};
use metor_fsw_3::link::DbStatus;
use metor_fsw_3::{
    Clock, DefCx, DefError, Input, LinkStatus, Output, PortDef, Timestamp, WallRate, system,
};
use metor_proto::types::ComponentId;
use metor_proto_stellar::{Client, Peer, identify};
use metor_proto_wkt::{
    DumpMetadata, GetSchema, GetTimeSeries, ImageFormat, ImageMeta, LogEvent, Stretch,
};
use serde_json::json;
use zerocopy::IntoBytes;

const CONN_CAP: usize = 4 << 20;
const PIXELS: &str = "cam.camera.image.pixels";
const FRAME_ID: &str = "cam.camera.image.frame_id";
const EXPOSURE: &str = "cam.camera.image.exposure_us";
const PIXEL_BYTES: usize = 2 << 20;
/// Image samples one `GetTimeSeries` response holds.
const PAGE: usize = 127;

/// The image tests share the machine's disk and memory, so they run one at a
/// time.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a target's sinks report back to the test.
#[derive(Clone, Default)]
struct Seen {
    link: Arc<Mutex<LinkStatus>>,
    db: Arc<Mutex<DbStatus>>,
    /// Records the target's mirrors reported dropped.
    mirror_dropped: Arc<AtomicU64>,
    faults: Arc<Mutex<Vec<String>>>,
}

impl Seen {
    fn link(&self) -> LinkStatus {
        *self.link.lock().expect("an unpoisoned sink")
    }

    fn db(&self) -> DbStatus {
        *self.db.lock().expect("an unpoisoned sink")
    }
}

#[system]
fn link_sink(#[state] s: &mut Seen, link_status: &mut Input<LinkStatus>) {
    for status in link_status.drain().flatten() {
        // PANIC Safety: no test holds this lock across a panic.
        *s.link.lock().expect("an unpoisoned sink") = *status;
    }
}

#[system]
fn db_sink(#[state] s: &mut Seen, db_status: &mut Input<DbStatus>) {
    for status in db_status.drain().flatten() {
        // PANIC Safety: as above.
        *s.db.lock().expect("an unpoisoned sink") = *status;
    }
}

/// Keeps every log kind and sums the counts of `mirror_dropped` lines.
#[system]
fn log_sink(#[state] s: &mut Seen, log: &mut Input<LogEvent>) {
    for event in log.drain().flatten() {
        let kind = event.fields.iter().find(|(name, _)| name == "kind");
        let kind = kind.map_or_else(|| event.message.to_string(), |(_, k)| k.to_string());
        if kind == "mirror_dropped" {
            let counts = event
                .fields
                .iter()
                .filter_map(|(_, v)| v.parse::<u64>().ok());
            s.mirror_dropped.fetch_add(counts.sum(), Ordering::Relaxed);
        }
        // PANIC Safety: as above.
        s.faults.lock().expect("an unpoisoned sink").push(kind);
    }
}

fn table(seen: &Seen, rate_hz: f64, shots: &Arc<Shots>) -> SystemTable {
    let mut table = SystemTable::new();
    let shots = shots.clone();
    table
        .register("camera", camera(), move || {
            Camera::new(rate_hz, shots.clone())
        })
        .expect("valid records");
    let link = seen.clone();
    table
        .register("link_sink", link_sink(), move || link.clone())
        .expect("valid records");
    let db = seen.clone();
    table
        .register("db_sink", db_sink(), move || db.clone())
        .expect("valid records");
    let logs = seen.clone();
    table
        .register("log_sink", log_sink(), move || logs.clone())
        .expect("valid records");
    register_builtins(&mut table).expect("valid records");
    table
}

fn reading(id: &str, ty: &str, port: &str, from: &[PortRef]) -> SystemConfig {
    SystemConfig {
        inputs: vec![InputConfig {
            port: port.into(),
            from: from.to_vec(),
        }],
        ..SystemConfig::new(id, ty)
    }
}

fn config(namespace: &str, systems: Vec<SystemConfig>) -> CoordinatorConfig {
    let mut config = CoordinatorConfig {
        clock: Clock::Wall {
            rate: WallRate::try_from(100.0).expect("a valid rate"),
        },
        namespace: Some(namespace.into()),
        systems,
        ..Default::default()
    };
    lower_links(&mut config);
    config
}

/// Target `cam`: the camera and a `Publish` serving one peer.
fn cam_config(addr: SocketAddr) -> CoordinatorConfig {
    let publish = SystemConfig {
        params: json!({
            "ep": { "tcp_listen": { "addr": addr.to_string() } },
            "max_connections": 1,
            "namespace": "cam",
            "link": "cam-pub",
            "conn_cap": CONN_CAP,
        }),
        ..reading(
            "pub",
            "fsw.publish",
            "camera.image",
            &[PortRef::new("camera", "image")],
        )
    };
    config(
        "cam",
        vec![
            SystemConfig::new("camera", "camera"),
            publish,
            reading(
                "status",
                "link_sink",
                "link_status",
                &[PortRef::new("pub", "link_status")],
            ),
            reading("logs", "log_sink", "log", &[PortRef::new("pub", "log")]),
        ],
    )
}

/// Target `gw`: a `Subscribe` dialing `cam` into a db.
fn gw_config(cam: SocketAddr, db: SocketAddr, path: &Path, conn_cap: usize) -> CoordinatorConfig {
    let cx = DefCx {
        id: "camera",
        namespace: Some("cam"),
        ..DefCx::empty()
    };
    let def = PortDef {
        name: "image".into(),
        ..Output::<ImageFrame>::def(&cx, "image")
    };
    let subscribe = SystemConfig {
        params: json!({
            "ep": { "tcp_dial": { "addr": cam.to_string() } },
            "link": "gw-sub",
            "conn_cap": conn_cap,
        }),
        outputs: vec![OutputConfig {
            port: "image".into(),
            source: OutputSource::Def(def),
        }],
        ..SystemConfig::new("sub", "fsw.subscribe")
    };
    let db = SystemConfig {
        params: json!({ "listen": db.to_string(), "path": path, "namespace": "gw" }),
        ..reading("db", "fsw.db", "sub.image", &[PortRef::new("sub", "image")])
    };
    let logs = [PortRef::new("sub", "log"), PortRef::new("db", "log")];
    let config = config(
        "gw",
        vec![
            subscribe,
            db,
            reading(
                "status",
                "link_sink",
                "link_status",
                &[PortRef::new("sub", "link_status")],
            ),
            reading(
                "db_status",
                "db_sink",
                "db_status",
                &[PortRef::new("db", "db_status")],
            ),
            reading("logs", "log_sink", "log", &logs),
        ],
    );
    // 16 frames in the ring and 32 in the db's 2x mirror: about a second
    // of 30 Hz frames before the db, if it stalls, drops one.
    CoordinatorConfig {
        ring_depth: NonZeroUsize::new(16).expect("nonzero"),
        ring_bytes: NonZeroUsize::new(32 << 20).expect("nonzero"),
        ..config
    }
}

fn spawn(config: CoordinatorConfig, seen: &Seen, rate_hz: f64, shots: &Arc<Shots>) -> Target {
    let (seen, shots) = (seen.clone(), shots.clone());
    let cycles = Arc::new(AtomicU64::new(0));
    Target::spawn(config, move || table(&seen, rate_hz, &shots), cycles)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Deadline::new();
    while !done() {
        deadline.tick(what).await;
    }
}

/// Dials the db at `addr` until it answers.
async fn dial_db(addr: SocketAddr) -> Client {
    let deadline = Deadline::new();
    loop {
        match identify(addr).await {
            Ok(Peer::Db(_)) => return Client::connect(addr).await.expect("the db takes clients"),
            Ok(_) => panic!("{addr} is not a db"),
            Err(e) => deadline.tick(format!("{addr} never came up: {e}")).await,
        }
    }
}

/// Anonymous resident memory, in bytes, or 0 off Linux.
fn rss_anon() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let line = status.lines().find(|l| l.starts_with("RssAnon:"));
    let kb = line.and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok());
    kb.unwrap_or(0) * 1024
}

fn query(component: &str, start: i64, limit: Option<usize>) -> GetTimeSeries {
    GetTimeSeries {
        id: [0, 0],
        range: Timestamp(start)..Timestamp(i64::MAX),
        component_id: ComponentId::new(component),
        limit,
    }
}

/// The frame id whose pattern `pixels` holds, if it holds one exactly.
fn frame_of(pixels: &[u8]) -> Option<u64> {
    let first = u16::from_le_bytes([*pixels.first()?, *pixels.get(1)?]);
    let id = u64::from(first);
    (pixels == pattern(id).as_bytes()).then_some(id)
}

/// Follows the `frame_id` series while the camera runs, timing each
/// frame's arrival in the db. The pixels are checked when paged at the end,
/// so following adds little load to the db.
#[derive(Default)]
struct Follower {
    next: i64,
    frames: Vec<u64>,
    latency_us: Vec<i64>,
    peak_anon: u64,
}

impl Follower {
    /// Fetches the frames stored since the last step. Returns whether any
    /// came.
    async fn step(&mut self, client: &mut Client) -> bool {
        self.peak_anon = self.peak_anon.max(rss_anon());
        let Ok(series) = client.request(&query(FRAME_ID, self.next, None)).await else {
            return false;
        };
        let (Ok(stamps), Ok(data)) = (series.timestamps(), series.data()) else {
            return false;
        };
        let now = Timestamp::now().0;
        for (stamp, id) in stamps.iter().zip(data.as_chunks::<8>().0) {
            self.latency_us.push(now - stamp.0);
            self.frames.push(u64::from_le_bytes(*id));
            self.next = stamp.0 + 1;
        }
        !stamps.is_empty()
    }

    /// Steps until `stop` returns true.
    async fn run_until(&mut self, client: &mut Client, mut stop: impl FnMut(&Self) -> bool) {
        while !stop(self) {
            if !self.step(client).await {
                stellarator::sleep(Duration::from_millis(2)).await;
            }
        }
    }

    /// Steps until no frame has come for `quiet`.
    async fn drain(&mut self, client: &mut Client, quiet: Duration) {
        let mut last = Instant::now();
        while last.elapsed() < quiet {
            match self.step(client).await {
                true => last = Instant::now(),
                false => stellarator::sleep(Duration::from_millis(2)).await,
            }
        }
    }
}

/// Every stored frame, paged oldest first. Returns `(timestamp, frame id)`
/// per frame and each page's size.
async fn page_pixels(client: &mut Client) -> (Vec<(i64, u64)>, Vec<usize>) {
    let (mut frames, mut pages, mut start) = (Vec::new(), Vec::new(), 0);
    loop {
        let series = client
            .request(&query(PIXELS, start, None))
            .await
            .expect("a series");
        let stamps = series.timestamps().expect("timestamps");
        let data = series.data().expect("data");
        let Some(last) = stamps.last() else {
            return (frames, pages);
        };
        assert_eq!(data.len(), stamps.len() * PIXEL_BYTES);
        pages.push(stamps.len());
        for (stamp, pixels) in stamps.iter().zip(data.as_chunks::<PIXEL_BYTES>().0) {
            let id = frame_of(pixels).unwrap_or_else(|| panic!("frame at {stamp:?} differs"));
            frames.push((stamp.0, id));
        }
        start = last.0 + 1;
    }
}

/// Every stored `frame_id` sample, as `(timestamp, id)`.
async fn frame_ids(client: &mut Client) -> Vec<(i64, u64)> {
    let series = client
        .request(&query(FRAME_ID, 0, None))
        .await
        .expect("a series");
    let stamps = series.timestamps().expect("timestamps");
    let data = series.data().expect("data");
    let ids = data
        .as_chunks::<8>()
        .0
        .iter()
        .map(|b| u64::from_le_bytes(*b));
    stamps.iter().map(|t| t.0).zip(ids).collect()
}

/// Checks the pixels component's announced metadata and schema, and that
/// no LoD level was built from it.
async fn check_metadata(client: &mut Client) {
    let dump = client.request(&DumpMetadata).await.expect("metadata");
    let named = |name: &str| dump.component_metadata.iter().find(|m| m.name == name);
    let pixels = named(PIXELS).expect("the pixels component");
    assert_eq!(
        pixels.metadata.get("image.format").map(String::as_str),
        Some("mono16")
    );
    assert_eq!(
        pixels.metadata.get("image.bits").map(String::as_str),
        Some("12")
    );
    assert_eq!(pixels.lod(), Some(false));
    let meta = ImageMeta::parse(pixels).expect("parses").expect("an image");
    assert_eq!(
        (meta.format, meta.bits, meta.stretch),
        (ImageFormat::Mono16, 12, Stretch::Auto)
    );
    let schema = client
        .request(&GetSchema {
            component_id: ComponentId::new(PIXELS),
        })
        .await
        .expect("a schema")
        .0;
    meta.check(schema.prim_type(), schema.dim())
        .expect("the schema fits");
    for sibling in [FRAME_ID, EXPOSURE] {
        assert!(named(sibling).is_some(), "{sibling} shares the parent path");
    }
    let id = pixels.component_id.0.to_string();
    let derived = dump
        .component_metadata
        .iter()
        .filter(|m| m.metadata.get("lod_source_id") == Some(&id));
    assert_eq!(derived.count(), 0, "LoD levels built from an image");
}

/// Drops counted anywhere on the path from the camera's ring to the db.
fn counted_drops(cam: &Seen, gw: &Seen) -> u64 {
    let (link, sub, db) = (cam.link(), gw.link(), gw.db());
    cam.mirror_dropped.load(Ordering::Relaxed)
        + gw.mirror_dropped.load(Ordering::Relaxed)
        + link.batches_dropped
        + sub.inbound_dropped
        + db.refused
        + db.wal_dropped
}

struct Run {
    written: u64,
    refused: u64,
    stored: Vec<(i64, u64)>,
    pages: Vec<usize>,
    drops: u64,
}

/// Runs the camera at `rate_hz` for `secs` into a fresh db and returns what
/// arrived. Prints the end-to-end numbers.
async fn run_images(rate_hz: f64, secs: f64) -> Run {
    let anon_before = rss_anon();
    let [cam_port, db_port] = free_ports();
    let dir = tempfile::tempdir().expect("a temp dir");
    let shots = Arc::new(Shots::default());
    let (cam_seen, gw_seen) = (Seen::default(), Seen::default());
    let _cam = spawn(cam_config(local(cam_port)), &cam_seen, rate_hz, &shots);
    let gw = gw_config(
        local(cam_port),
        local(db_port),
        &dir.path().join("db"),
        CONN_CAP,
    );
    let _gw = spawn(gw, &gw_seen, rate_hz, &shots);
    let mut client = dial_db(local(db_port)).await;
    until("the subscriber never connected", || {
        cam_seen.link().connections == 1
    })
    .await;

    let mut follower = Follower::default();
    shots.on.store(true, Ordering::Release);
    let started = Instant::now();
    let run = Duration::from_secs_f64(secs);
    follower
        .run_until(&mut client, |_| started.elapsed() >= run)
        .await;
    shots.on.store(false, Ordering::Release);
    follower
        .drain(&mut client, Duration::from_millis(1500))
        .await;
    let elapsed = started.elapsed();
    let anon_growth = follower.peak_anon.saturating_sub(anon_before);

    let written = shots.written.load(Ordering::Relaxed);
    let refused = shots.refused.load(Ordering::Relaxed);
    let drops = counted_drops(&cam_seen, &gw_seen);
    let (stored, pages) = page_pixels(&mut client).await;
    check_metadata(&mut client).await;
    let ids = frame_ids(&mut client).await;
    assert_eq!(stored, ids, "the frame_id series matches the pixels");
    assert_eq!(
        follower.frames,
        stored.iter().map(|(_, id)| *id).collect::<Vec<_>>()
    );

    let mut latency = follower.latency_us.clone();
    latency.sort_unstable();
    let pct = |p: f64| {
        latency.get(((latency.len() as f64 * p) as usize).min(latency.len().saturating_sub(1)))
    };
    let mib = stored.len() as f64 * PIXEL_BYTES as f64 / f64::from(1 << 20);
    eprintln!(
        "image {rate_hz} Hz for {secs} s: written {written}, refused {refused}, stored {}, \
         counted drops {drops}, latency ms p50 {:?} p99 {:?} max {:?}, \
         stored {:.1} MiB/s, faults {:?}, rss anon growth {} MiB",
        stored.len(),
        pct(0.5).map(|us| *us as f64 / 1e3),
        pct(0.99).map(|us| *us as f64 / 1e3),
        latency.last().map(|us| *us as f64 / 1e3),
        mib / elapsed.as_secs_f64(),
        gw_seen.faults.lock().expect("an unpoisoned sink"),
        anon_growth >> 20,
    );
    if cfg!(target_os = "linux") {
        assert!(
            anon_growth < 512 << 20,
            "anonymous memory grew {anon_growth} bytes"
        );
    }
    Run {
        written,
        refused,
        stored,
        pages,
        drops,
    }
}

/// Checks the stored frames are in order, each once, and that every frame
/// written and not stored is counted as a drop.
fn check_accounted(run: &Run) {
    let ids: Vec<u64> = run.stored.iter().map(|(_, id)| *id).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    assert!(run.stored.windows(2).all(|w| w[0].0 < w[1].0));
    let missing = run.written - run.stored.len() as u64;
    assert!(
        missing <= run.drops,
        "{missing} frames lost, {} counted",
        run.drops
    );
}

#[stellarator::test]
#[expect(
    clippy::await_holding_lock,
    reason = "each test runs on its own thread"
)]
async fn test_images_at_10_hz() {
    let _serial = serial();
    let run = run_images(10.0, 3.0).await;
    check_accounted(&run);
    assert!(run.written >= 25, "{}", run.written);
    assert_eq!((run.refused, run.drops), (0, 0));
    assert_eq!(run.stored.len() as u64, run.written);
    let ids: Vec<u64> = run.stored.iter().map(|(_, id)| *id).collect();
    assert_eq!(ids, (1..=run.written).collect::<Vec<_>>());
    assert_eq!(run.pages, [run.stored.len()]);
}

/// 300 frames at 30 Hz: the db keeps up, and `GetTimeSeries` pages them
/// 127 at a time.
#[stellarator::test]
#[expect(
    clippy::await_holding_lock,
    reason = "each test runs on its own thread"
)]
async fn test_images_at_30_hz_page_through_get_time_series() {
    let _serial = serial();
    let run = run_images(30.0, 10.0).await;
    check_accounted(&run);
    // A debug build's db cannot keep up with 60 MB/s; its drops are counted.
    if !cfg!(debug_assertions) {
        assert_eq!((run.refused, run.drops), (0, 0));
    }
    let stored = run.stored.len();
    let mut pages = vec![PAGE; stored / PAGE];
    pages.extend(Some(stored % PAGE).filter(|rest| *rest > 0));
    assert_eq!(run.pages, pages);
}

/// A subscriber restarted mid-run loses only the frames sent while it was
/// down, and the series stays in order across the reopened db.
#[stellarator::test]
#[expect(
    clippy::await_holding_lock,
    reason = "each test runs on its own thread"
)]
async fn test_subscriber_reconnect_keeps_the_series_ordered() {
    let _serial = serial();
    let [cam_port, db_a, db_b] = free_ports();
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("db");
    let shots = Arc::new(Shots::default());
    let cam_seen = Seen::default();
    let _cam = spawn(cam_config(local(cam_port)), &cam_seen, 10.0, &shots);

    let gw = spawn(
        gw_config(local(cam_port), local(db_a), &path, CONN_CAP),
        &Seen::default(),
        10.0,
        &shots,
    );
    let mut client = dial_db(local(db_a)).await;
    until("the subscriber never connected", || {
        cam_seen.link().connections == 1
    })
    .await;
    shots.on.store(true, Ordering::Release);
    let mut follower = Follower::default();
    follower
        .run_until(&mut client, |f| f.frames.len() >= 10)
        .await;
    drop(client);
    drop(gw);
    until("the publisher never saw the drop", || {
        cam_seen.link().connections == 0
    })
    .await;
    stellarator::sleep(Duration::from_millis(500)).await;

    let _gw = spawn(
        gw_config(local(cam_port), local(db_b), &path, CONN_CAP),
        &Seen::default(),
        10.0,
        &shots,
    );
    let mut client = dial_db(local(db_b)).await;
    until("the subscriber never reconnected", || {
        cam_seen.link().connections == 1
    })
    .await;
    let mut follower = Follower {
        next: 0,
        ..Default::default()
    };
    let target = shots.written.load(Ordering::Relaxed) + 10;
    follower
        .run_until(&mut client, |f| {
            f.frames.last().is_some_and(|id| *id >= target)
        })
        .await;
    shots.on.store(false, Ordering::Release);

    let (stored, _) = page_pixels(&mut client).await;
    let ids: Vec<u64> = stored.iter().map(|(_, id)| *id).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    let runs = 1 + ids.windows(2).filter(|w| w[1] != w[0] + 1).count();
    assert!(runs <= 2, "frames lost outside the downtime: {ids:?}");
    assert_eq!(ids.first(), Some(&1));
    assert!(ids.len() >= 20, "{ids:?}");
}

/// The same `gw` with a 1 MiB `conn_cap` fails to build.
#[test]
fn test_subscriber_cap_below_an_image_fails_the_build() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let [cam, db] = free_ports();
    let config = gw_config(local(cam), local(db), &dir.path().join("db"), 1 << 20);
    let error = config
        .build(&table(&Seen::default(), 10.0, &Arc::default()))
        .err();
    let Some(BuildError::Def { id, source }) = error else {
        panic!("{error:?}");
    };
    assert_eq!(id, "sub");
    let refused = DefError::PortExceedsConnCap {
        port: "image".into(),
        max_len: size_of::<ImageFrame>(),
        conn_cap: 1 << 20,
    };
    assert_eq!(source, refused);
}
