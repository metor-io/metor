//! A 1024² `Mono16<12>` image from a camera, through a `Publish`/`Subscribe`
//! link, into a db, and back out through `GetTimeSeries`.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{
    Camera, Deadline, ImageFrame, Shots, Target, camera, free_ports, local, lower_links, pattern,
};
use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::coordinator::{
    BuildError, CoordinatorConfig, InputConfig, OutputConfig, OutputSource, PortRef, SystemConfig,
    SystemTable,
};
use metor_fsw_3::{
    Clock, DefCx, DefError, Input, LinkStatus, Output, PortDef, Timestamp, WallRate, system,
};
use metor_proto::types::ComponentId;
use metor_proto_stellar::{Client, Peer, identify};
use metor_proto_wkt::{DumpMetadata, GetSchema, GetTimeSeries, ImageFormat, ImageMeta, Stretch};
use serde_json::json;
use zerocopy::IntoBytes;

const CONN_CAP: usize = 4 << 20;
const PIXELS: &str = "cam.camera.image.pixels";
const FRAME_ID: &str = "cam.camera.image.frame_id";
const EXPOSURE: &str = "cam.camera.image.exposure_us";
const PIXEL_BYTES: usize = 2 << 20;

/// The link status a target's sink last saw.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<LinkStatus>>);

impl Seen {
    fn link(&self) -> LinkStatus {
        *self.0.lock().expect("an unpoisoned sink")
    }
}

#[system]
fn link_sink(#[state] s: &mut Seen, link_status: &mut Input<LinkStatus>) {
    for status in link_status.drain().flatten() {
        // PANIC Safety: no test holds this lock across a panic.
        *s.0.lock().expect("an unpoisoned sink") = *status;
    }
}

fn table(seen: &Seen, shots: &Arc<Shots>) -> SystemTable {
    let mut table = SystemTable::new();
    let shots = shots.clone();
    table
        .register("camera", camera(), move || Camera::new(10.0, shots.clone()))
        .expect("valid records");
    let link = seen.clone();
    table
        .register("link_sink", link_sink(), move || link.clone())
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
    config(
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
        ],
    )
}

fn spawn(config: CoordinatorConfig, seen: &Seen, shots: &Arc<Shots>) -> Target {
    let (seen, shots) = (seen.clone(), shots.clone());
    let cycles = Arc::new(AtomicU64::new(0));
    Target::spawn(config, move || table(&seen, &shots), cycles)
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

#[stellarator::test]
async fn test_images_at_10_hz() {
    let [cam_port, db_port] = free_ports();
    let dir = tempfile::tempdir().expect("a temp dir");
    let shots = Arc::new(Shots::default());
    let (cam_seen, gw_seen) = (Seen::default(), Seen::default());
    let _cam = spawn(cam_config(local(cam_port)), &cam_seen, &shots);
    let gw = gw_config(
        local(cam_port),
        local(db_port),
        &dir.path().join("db"),
        CONN_CAP,
    );
    let _gw = spawn(gw, &gw_seen, &shots);
    let mut client = dial_db(local(db_port)).await;
    until("the subscriber never connected", || {
        cam_seen.link().connections == 1
    })
    .await;

    shots.on.store(true, Ordering::Release);
    stellarator::sleep(Duration::from_secs(3)).await;
    shots.on.store(false, Ordering::Release);
    let written = shots.written.load(Ordering::Relaxed);
    let deadline = Deadline::new();
    while (frame_ids(&mut client).await.len() as u64) < written {
        deadline.tick("the db never stored every frame").await;
    }

    let (stored, pages) = page_pixels(&mut client).await;
    check_metadata(&mut client).await;
    assert_eq!(stored, frame_ids(&mut client).await);
    assert!(written >= 25, "{written}");
    assert_eq!(shots.refused.load(Ordering::Relaxed), 0);
    let ids: Vec<u64> = stored.iter().map(|(_, id)| *id).collect();
    assert_eq!(ids, (1..=written).collect::<Vec<_>>());
    assert!(stored.windows(2).all(|w| w[0].0 < w[1].0));
    assert_eq!(pages, [stored.len()]);
}

/// The same `gw` with a 1 MiB `conn_cap` fails to build.
#[test]
fn test_subscriber_cap_below_an_image_fails_the_build() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let [cam, db] = free_ports();
    let config = gw_config(local(cam), local(db), &dir.path().join("db"), 1 << 20);
    let error = config
        .build(&table(&Seen::default(), &Arc::default()))
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
