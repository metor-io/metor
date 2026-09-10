//! Two members of one deployment exchanging data: `b` mirrors `a`'s
//! downlink, the mirror's frames reach `b`'s own ground link, a member run
//! alone runs disconnected, bundles carry the peer across, and `--peer`
//! points the client at an address nothing answers.
//!
//! One `#[test]` runs every case in sequence: they share the fixture's link
//! ports and the process-global current directory.

use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use metor_proto::types::{Msg, OwnedPacket};
use metor_proto_stellar::{Peer, identify};
use metor_proto_wkt::{SetComponentMetadata, VTableMsg};

/// The mirror's `link_status.connections` under `b`'s namespace: the one
/// component that moves only because the peer's record arrived.
const MIRRORED: &str = "b.a_link.link_status.connections";

/// Whether a usable `python3` is on PATH, so the source cases can skip
/// cleanly on a host without one.
fn have_python() -> bool {
    Command::new("python3")
        .args([
            "-c",
            "import sys;sys.exit(0 if sys.version_info[:2]>=(3,10) else 1)",
        ])
        .status()
        .is_ok_and(|status| status.success())
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The CLI under test, from the fixture directory, with the interpreter
/// resolved the default way and the members logging so their lines carry the
/// client's connect events.
fn fsw(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_metor-fsw"));
    command
        .args(args)
        .current_dir(fixtures())
        .env_remove("METOR_PYTHON")
        .env("RUST_LOG", "info");
    command
}

fn run(args: &[&str]) -> Output {
    fsw(args).output().expect("the CLI runs")
}

/// The subscriber's connect event, under the member prefix the launcher adds.
fn connected(stderr: &str) -> bool {
    stderr
        .lines()
        .any(|line| line.starts_with("b │") && line.contains("peer connected"))
}

/// Kill a spawned deployment and read back what it logged. The launcher
/// spawns its members as its own children and does not take them down with
/// it, so the whole process group goes.
fn kill(mut child: Child) -> String {
    let _ = Command::new("kill")
        .arg("-9")
        .arg(format!("-{}", child.id()))
        .status();
    let _ = child.wait();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    stderr
}

/// Dial `addr` until an fsw link answers or the deadline passes.
async fn dial(addr: SocketAddr, deadline: Instant) -> Peer {
    loop {
        match identify(addr).await {
            Ok(peer) => return peer,
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "nothing answered at {addr}: {err}"
                );
                stellarator::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Read `b`'s ground link until one `Table` arrives on the packet id the
/// mirror's `connections` component was announced under.
///
/// A frame downlinks when it changes, so the change is made here: once the
/// mirror's announce is in hand, this dials `a`'s own ground link, which
/// moves `a`'s `link_status.connections` from 0 to 1. That record can only
/// reach `b`'s link by crossing the peer connection.
async fn mirrored_frame_arrives(b_link: SocketAddr, a_link: SocketAddr, deadline: Instant) {
    let Peer::Fsw { mut rx, .. } = dial(b_link, deadline).await else {
        panic!("`b`'s ground link is an fsw link");
    };

    let mut buf = vec![0u8; 64 * 1024];
    let mut last_table = None;
    let mut mirrored = None;
    let mut moved = None;
    loop {
        assert!(
            Instant::now() < deadline,
            "no `{MIRRORED}` frame within the deadline"
        );
        let pkt = rx.next_grow(buf).await.expect("`b`'s link stays up");
        match &pkt {
            OwnedPacket::Msg(m) if m.id == VTableMsg::ID => {
                last_table = m.parse::<VTableMsg>().ok().map(|msg| msg.id);
            }
            OwnedPacket::Msg(m) if m.id == SetComponentMetadata::ID => {
                if let Ok(msg) = m.parse::<SetComponentMetadata>()
                    && msg.0.name == MIRRORED
                {
                    mirrored = last_table;
                    moved = Some(dial(a_link, deadline).await);
                }
            }
            OwnedPacket::Table(table) if Some(table.id) == mirrored => {
                drop(moved);
                return;
            }
            _ => {}
        }
        buf = pkt.into_buf().into_inner();
    }
}

#[test]
fn peers_exchange_data() {
    if !have_python() {
        eprintln!("skipping peers_exchange_data: no python3 >= 3.10 on PATH");
        return;
    }

    // The whole deployment: `b`'s pre-flight names the mirror and its client
    // connects to `a`'s publish server. Wall-clocked, because the client's
    // socket task is polled once per cycle.
    let output = run(&["run", "comms_target.py", "--cycles", "200"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("mirror of a/peer"),
        "`b`'s pre-flight names the peer:\n{stderr}"
    );
    assert!(connected(&stderr), "the mirror connects:\n{stderr}");

    // The data assertion: a ground client on `b`'s own link sees the
    // mirrored port announced and then a record on it.
    let child = fsw(&["run", "comms_target.py"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("the CLI spawns");
    let deadline = Instant::now() + Duration::from_secs(20);
    let b_link: SocketAddr = "127.0.0.1:2255".parse().unwrap();
    let a_link: SocketAddr = "127.0.0.1:2253".parse().unwrap();
    let watched = std::thread::spawn(move || {
        stellarator::run(|| mirrored_frame_arrives(b_link, a_link, deadline));
    });
    let watched = watched.join();
    let stderr = kill(child);
    watched.unwrap_or_else(|_| panic!("the mirrored frame arrives:\n{stderr}"));

    // One member alone: the mirror runs, finds nobody, and the run still
    // ends cleanly.
    let output = run(&["run", "comms_target.py", "--target", "b", "--cycles", "100"]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    assert!(
        !stderr.contains("peer connected"),
        "nothing to connect to here:\n{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");

    // The same pair from bundles: the peer crosses the package boundary.
    let temp = tempfile::tempdir().expect("a temp dir");
    let mut bundles = Vec::new();
    for ns in ["a", "b"] {
        let bundle = temp.path().join(format!("{ns}.bundle"));
        let output = run(&[
            "package",
            "comms_target.py",
            "--target",
            ns,
            "-o",
            bundle.to_str().unwrap(),
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        bundles.push(bundle);
    }
    let output = run(&[
        "run",
        bundles[0].to_str().unwrap(),
        bundles[1].to_str().unwrap(),
        "--cycles",
        "200",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    assert!(connected(&stderr), "the bundled mirror connects:\n{stderr}");

    // `--peer` overrides the candidates; nothing listens on port 1, so the
    // client says so and the member keeps running.
    let output = run(&[
        "run",
        "comms_target.py",
        "--target",
        "b",
        "--peer",
        "a=127.0.0.1:1",
        "--cycles",
        "300",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("no peer answered"),
        "the unreachable override is reported:\n{stderr}"
    );
}
