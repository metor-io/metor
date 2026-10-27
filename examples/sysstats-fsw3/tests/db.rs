//! Runs `sim.py` under `metor run` and checks that its db records a
//! component for every map entry: one per core, interface, process, and
//! device. Every name, nested map entries included, sits under the target's
//! namespace.

use core::time::Duration;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use metor_proto_stellar::Client;
use metor_proto_wkt::{ComponentMetadata, DumpMetadata, MetadataExt as _};

/// How long the test waits for the db to hold every component.
const DEADLINE: Duration = Duration::from_secs(60);

/// Cycles the target runs before exiting on its own, which bounds how long
/// the child of a killed test lives.
const CYCLES: &str = "12000";

fn sim() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("sim.py")
}

/// Returns a command for the `metor` binary, building it first. The example
/// has no binary of its own, so cargo gives the test no path to it.
fn metor() -> Command {
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "metor-fsw-3", "--bin", "metor"])
        .status()
        .expect("cargo runs");
    assert!(status.success(), "the metor binary builds");
    let exe = std::env::current_exe().expect("this test's path");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("<target>/<profile>/deps/<test>");
    Command::new(dir.join("metor"))
}

/// Two ports free when checked.
fn free_ports() -> [u16; 2] {
    let port = || {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("binds");
        listener.local_addr().expect("bound").port()
    };
    [port(), port()]
}

/// A `metor run` of `sim.py`, killed and reaped on drop.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

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

/// A component name pattern: a prefix, a key, and a field.
struct Keyed {
    prefix: &'static str,
    field: &'static str,
}

impl Keyed {
    fn matches(&self, name: &str) -> bool {
        name.strip_prefix(self.prefix)
            .and_then(|rest| rest.strip_suffix(self.field))
            .is_some_and(|key| !key.is_empty() && !key.contains('.'))
    }
}

const WANTED: [Keyed; 6] = [
    Keyed {
        prefix: "host.cpu.cpu.cores.",
        field: ".usage_pct",
    },
    Keyed {
        prefix: "host.disks.disks.filesystems.",
        field: ".used_pct",
    },
    Keyed {
        prefix: "host.nics.nics.ifaces.",
        field: ".rx_bytes",
    },
    Keyed {
        prefix: "host.procs.procs.pids.",
        field: ".cpu_ns",
    },
    Keyed {
        prefix: "host.procs.procs.pids.",
        field: ".name",
    },
    Keyed {
        prefix: "host.disks.disks.devices.",
        field: ".read_bytes",
    },
];

/// Keyed members carry their template's metadata, and templates stay hidden.
fn check_metadata(all: &[ComponentMetadata]) {
    let visible = || all.iter().filter(|m| !m.is_hidden());
    let names = visible()
        .filter(|m| m.name.starts_with("host.procs.procs.pids.") && m.name.ends_with(".name"));
    assert!(names.clone().count() > 0, "process names");
    assert!(
        names.clone().all(|m| m.is_string()),
        "process names render as text"
    );
    let pcts: Vec<_> = visible().filter(|m| m.name.ends_with("_pct")).collect();
    assert!(!pcts.is_empty(), "percent fields");
    let unitless: Vec<_> = pcts
        .iter()
        .filter(|m| m.get("unit") != Some("%"))
        .map(|m| &m.name)
        .collect();
    assert!(unitless.is_empty(), "percents without a unit: {unitless:?}");
    let template = all.iter().find(|m| m.name == "host.procs.procs.pids.name");
    assert!(
        template.is_some_and(|m| m.is_hidden()),
        "templates stay hidden"
    );
}

#[stellarator::test]
async fn test_db_records_keyed_components() {
    let ports = free_ports();
    let child = metor()
        .arg("run")
        .arg(sim())
        .args(["--cycles", CYCLES])
        .env("METOR_SYSSTATS_PORTS", format!("{},{}", ports[0], ports[1]))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("metor runs");
    let _running = Running(child);
    let mut client = connect(SocketAddr::from(([127, 0, 0, 1], ports[1]))).await;
    let deadline = Instant::now() + DEADLINE;
    loop {
        let dump = client.request(&DumpMetadata).await.expect("the db answers");
        let names: Vec<&str> = dump
            .component_metadata
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        let missing: Vec<_> = WANTED
            .iter()
            .filter(|w| !names.iter().any(|n| w.matches(n)))
            .map(|w| w.prefix.to_string() + "*" + w.field)
            .collect();
        let stray: Vec<_> = names.iter().filter(|n| !n.starts_with("host.")).collect();
        assert!(stray.is_empty(), "named outside the target: {stray:?}");
        if missing.is_empty() && names.contains(&"host.mem.mem.total") {
            check_metadata(&dump.component_metadata);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "missing {missing:?} in {names:?}"
        );
        stellarator::sleep(Duration::from_millis(100)).await;
    }
}
