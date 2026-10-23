//! Runs `target.py` under `metor run` and checks that the gateway's db records
//! both the plant and the flight software, and forwards the flight software's
//! commands.

use core::time::Duration;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use metor_proto::types::{ComponentId, Msg, Timestamp};
use metor_proto_stellar::Client;
use metor_proto_wkt::{DumpMetadata, GetDbInfo, GetTimeSeries, SequenceCommand};

/// How long the test waits for the deployment.
const DEADLINE: Duration = Duration::from_secs(60);

/// Cycles each target runs before exiting on its own. This outlasts the test
/// and bounds how long the children of a killed test live.
const CYCLES: &str = "200000";

const PORTS_ENV: &str = "METOR_DEPLOY_PORTS";

fn target() -> PathBuf {
    example("target.py")
}

fn example(file: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(file)
}

/// Returns a command for the `metor` binary, building it on first use. The
/// example has no binary of its own, so cargo gives the test no path to it.
fn metor() -> Command {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    Command::new(BUILT.get_or_init(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-p", "metor-fsw-3", "--bin", "metor"])
            .status()
            .expect("cargo runs");
        assert!(status.success(), "the metor binary builds");
        let exe = std::env::current_exe().expect("this test's path");
        exe.parent()
            .and_then(Path::parent)
            .expect("<target>/<profile>/deps/<test>")
            .join("metor")
    }))
}

/// The ports [`free_ports`] hands out.
///
/// They sit below the ephemeral ranges of Linux (32768 and up) and macOS
/// (49152 and up), so the OS never gives one to a `bind(0)` or an outgoing
/// connection.
const TEST_PORTS: core::ops::Range<u16> = 20000..32000;

/// How far into [`TEST_PORTS`] this process has handed out ports.
static NEXT_PORT: AtomicU32 = AtomicU32::new(0);

/// Returns three ports that nothing listened on when checked.
///
/// Each call in a process gets different ports, and each process starts at a
/// different place in [`TEST_PORTS`]. A port is probed by dialing it, not by
/// binding it: a socket bound here can leak into a child spawned by another
/// test at the same moment, and the child then holds the port.
fn free_ports() -> [u16; 3] {
    let span = u32::from(TEST_PORTS.end - TEST_PORTS.start);
    let start = std::process::id().wrapping_mul(997);
    let ports: Vec<u16> = (0..span)
        .map(|_| start.wrapping_add(NEXT_PORT.fetch_add(1, Ordering::Relaxed)) % span)
        .filter_map(|offset| u16::try_from(offset).ok())
        .map(|offset| TEST_PORTS.start + offset)
        .filter(|&port| TcpStream::connect(SocketAddr::from(([127, 0, 0, 1], port))).is_err())
        .take(3)
        .collect();
    ports.try_into().expect("enough free test ports")
}

/// A `metor run` of the deployment, killed and reaped on drop.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(ports: &[u16; 3]) -> Running {
    run_file(target(), ports)
}

fn run_file(file: PathBuf, ports: &[u16; 3]) -> Running {
    let child = metor()
        .arg("run")
        .arg(file)
        .args(["--cycles", CYCLES])
        .env(PORTS_ENV, format!("{},{},{}", ports[0], ports[1], ports[2]))
        .stdout(Stdio::null())
        .spawn()
        .expect("metor runs");
    Running(child)
}

/// Dials the db until it answers.
async fn connect(addr: SocketAddr) -> Client {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Ok(client) = Client::connect(addr).await {
            return client;
        }
        assert!(Instant::now() < deadline, "the db never came up");
        stellarator::sleep(Duration::from_millis(50)).await;
    }
}

/// The gateway records the plant's gyro and the flight software's estimate,
/// each under its own target's namespace.
#[stellarator::test]
async fn test_gateway_records_both_members() {
    let ports = free_ports();
    let _running = run(&ports);
    let mut client = connect(SocketAddr::from(([127, 0, 0, 1], ports[2]))).await;

    let wanted = [
        "plant.plant.sensors.gyro_b",
        "fsw.nav.attitude_estimate.omega_b",
    ];
    let deadline = Instant::now() + DEADLINE;
    loop {
        let dump = client.request(&DumpMetadata).await.expect("the db answers");
        let names: Vec<&str> = dump
            .component_metadata
            .iter()
            .map(|metadata| metadata.name.as_str())
            .collect();
        if wanted.iter().all(|name| names.contains(name)) {
            return;
        }
        assert!(Instant::now() < deadline, "announced only {names:?}");
        stellarator::sleep(Duration::from_millis(50)).await;
    }
}

/// The gateway's db advertises `sequence_command`, the record the flight
/// software's `cmds` subscriber takes, under the id that record's port carries.
#[stellarator::test]
async fn test_gateway_advertises_sequence_command() {
    let ports = free_ports();
    let _running = run(&ports);
    let mut client = connect(SocketAddr::from(([127, 0, 0, 1], ports[2]))).await;
    let info = client.request(&GetDbInfo).await.expect("the db answers");
    assert_eq!(info.command_ids, vec![<SequenceCommand as Msg>::ID]);
}

/// Every component `presets/star-tracker.json` binds, read out of the layout:
/// the image view's image and overlay fields by name, the plots' traces by id.
fn preset_components() -> Vec<ComponentId> {
    fn walk(node: &serde_json::Value, out: &mut Vec<ComponentId>) {
        if let Some(children) = node.pointer("/Split/children").and_then(|c| c.as_array()) {
            children.iter().for_each(|child| walk(child, out));
        }
        for item in node
            .pointer("/Pane/items")
            .and_then(|i| i.as_array())
            .into_iter()
            .flatten()
        {
            let state: serde_json::Value =
                serde_json::from_str(item["state"].as_str().unwrap()).unwrap();
            match item["kind"].as_str().unwrap() {
                "image_view" => {
                    out.push(ComponentId::new(state["component"].as_str().unwrap()));
                    for overlay in state["overlays"].as_array().unwrap() {
                        for field in ["u", "v", "count", "size", "cat_id"] {
                            out.push(ComponentId::new(overlay[field].as_str().unwrap()));
                        }
                    }
                }
                "time_series_plot" => {
                    for trace in state["traces"].as_array().unwrap() {
                        out.push(ComponentId(trace["component_id"].as_u64().unwrap()));
                    }
                }
                kind => panic!("unexpected pane {kind}"),
            }
        }
    }
    let preset = std::fs::read_to_string(example("presets/star-tracker.json")).unwrap();
    let layout: serde_json::Value = serde_json::from_str(&preset).unwrap();
    let mut out = Vec::new();
    walk(&layout["root"], &mut out);
    out
}

/// `sim.py`'s db records every component the star-tracker preset binds, and
/// star images reach it.
#[stellarator::test]
async fn test_sim_records_star_tracker_preset() {
    let wanted = preset_components();
    assert!(wanted.len() > 10, "the preset binds {wanted:?}");
    let ports = free_ports();
    let _running = run_file(example("sim.py"), &ports);
    let mut client = connect(SocketAddr::from(([127, 0, 0, 1], ports[2]))).await;

    let deadline = Instant::now() + DEADLINE;
    loop {
        let dump = client.request(&DumpMetadata).await.expect("the db answers");
        let ids: Vec<ComponentId> = dump
            .component_metadata
            .iter()
            .map(|metadata| metadata.component_id)
            .collect();
        let missing: Vec<_> = wanted.iter().filter(|id| !ids.contains(id)).collect();
        if missing.is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "never announced {missing:?}");
        stellarator::sleep(Duration::from_millis(100)).await;
    }

    let query = GetTimeSeries {
        id: [0, 1],
        range: Timestamp(i64::MIN)..Timestamp(i64::MAX),
        component_id: ComponentId::new("sim.star_sim.star_image.pixels"),
        limit: Some(2),
    };
    loop {
        if let Ok(series) = client.request(&query).await
            && series.timestamps().is_ok_and(|t| t.len() == 2)
        {
            assert_eq!(series.data().unwrap().len(), 2 * 1024 * 1024 * 2);
            return;
        }
        assert!(Instant::now() < deadline, "no star images recorded");
        stellarator::sleep(Duration::from_millis(100)).await;
    }
}
