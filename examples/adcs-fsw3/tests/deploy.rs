//! Runs `target.py` under `metor run` and checks that the gateway's db records
//! both the plant and the flight software.

use core::time::Duration;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Instant;

use metor_proto_stellar::Client;
use metor_proto_wkt::DumpMetadata;

/// How long the test waits for the deployment.
const DEADLINE: Duration = Duration::from_secs(60);

/// Cycles each target runs before exiting on its own. This outlasts the test
/// and bounds how long the children of a killed test live.
const CYCLES: &str = "200000";

const PORTS_ENV: &str = "METOR_DEPLOY_PORTS";

fn target() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target.py")
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

/// Returns three ports that were free when checked.
fn free_ports() -> [u16; 3] {
    let held: Vec<TcpListener> = (0..3)
        .map(|_| TcpListener::bind("127.0.0.1:0").expect("a free port"))
        .collect();
    let ports: Vec<u16> = held
        .iter()
        .map(|listener| listener.local_addr().expect("bound").port())
        .collect();
    [ports[0], ports[1], ports[2]]
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
    let child = metor()
        .arg("run")
        .arg(target())
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
