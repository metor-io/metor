//! A gateway member ingesting a deployment: two members serve ground links,
//! `gw` streams both into its embedded db, a ground db mirrors the gateway
//! and sees both members, a command pushed into the mirror reaches exactly
//! the member that forwards it, a member restart is a new session, the
//! gateway alone runs disconnected, and the three bundles do the same.
//!
//! One `#[test]` runs every case in sequence: they share the fixture's link
//! ports, the gateway's address, and the process-global current directory.
//!
//! A panic inside a stellarator runtime aborts the process, taking the
//! harness's report and the spawned members with it, so the watching futures
//! return `Err(what)` and the test asserts on the main thread.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use metor_db::DB;
use metor_db::remote::RemoteDb;
use metor_proto::types::{ComponentId, Msg, OwnedPacket, Timestamp};
use metor_proto_stellar::{Peer, identify};
use metor_proto_wkt::{LogEvent, ReloadSequences, WiringManifest};

/// The fixture's addresses: `a`'s link, `b`'s link, the gateway's db.
const A_LINK: &str = "127.0.0.1:2256";
const B_LINK: &str = "127.0.0.1:2257";
const GATEWAY: &str = "127.0.0.1:2258";

/// What a watching future reports back: the observation it never made.
type Watch = Result<(), String>;

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
/// ingests' connect events.
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

/// The thread draining each spawned child's stderr, by process id. A member
/// logs on its coordinator thread, and the launcher copies every member's
/// lines into this one pipe: a pipe left unread fills at 64 KB and blocks the
/// member that wrote the line, so it is read as it arrives and kept here
/// until the process is killed.
static DRAINS: Mutex<BTreeMap<u32, JoinHandle<String>>> = Mutex::new(BTreeMap::new());

/// Start a member or a whole deployment in its own process group, so killing
/// it takes the launcher's children with it.
fn spawn(args: &[&str]) -> Child {
    let mut child = fsw(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("the CLI spawns");
    let mut pipe = child.stderr.take().expect("the child's stderr");
    let drain = std::thread::spawn(move || {
        let mut stderr = String::new();
        let _ = pipe.read_to_string(&mut stderr);
        stderr
    });
    DRAINS
        .lock()
        .expect("the drain table")
        .insert(child.id(), drain);
    child
}

/// Kill a spawned process group and read back what it logged.
fn kill(mut child: Child) -> String {
    let drain = DRAINS.lock().expect("the drain table").remove(&child.id());
    let _ = Command::new("kill")
        .arg("-9")
        .arg(format!("-{}", child.id()))
        .status();
    let _ = child.wait();
    drain
        .map(|drain| drain.join().expect("the drain thread ends"))
        .unwrap_or_default()
}

/// One identity probe, abandoned after a second: a socket that connects and
/// is then never served answers nothing, and would otherwise hold the dial
/// loop past its deadline.
async fn probe(addr: SocketAddr) -> Option<Peer> {
    let deadline = Instant::now() + Duration::from_secs(1);
    futures_lite::future::or(async { identify(addr).await.ok() }, async {
        rest(deadline).await;
        None
    })
    .await
}

/// Sleep until `deadline` in short steps.
async fn rest(deadline: Instant) {
    while Instant::now() < deadline {
        stellarator::sleep(Duration::from_millis(50)).await;
    }
}

/// Dial `addr` until it answers, or give up at the deadline.
async fn dial(addr: SocketAddr, deadline: Instant) -> Option<Peer> {
    loop {
        match probe(addr).await {
            Some(peer) => return Some(peer),
            None if Instant::now() >= deadline => return None,
            None => stellarator::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// A ground db mirroring the gateway, the way the panel's connection does:
/// components, message logs, and the commands the gateway advertises. The
/// gateway is probed first, so a db that never answers is reported as that
/// rather than as data the mirror never carried.
async fn served_mirror(dir: &Path, deadline: Instant) -> Result<Arc<DB>, String> {
    let addr = GATEWAY.parse().expect("the gateway address");
    match dial(addr, deadline).await {
        Some(Peer::Db(_)) => Ok(mirror(dir)),
        Some(_) => Err(format!("an fsw link answered at {addr}")),
        None => Err(format!("never saw the gateway answer at {addr}")),
    }
}

/// The mirror itself: a local db and the supervisor that fills it.
fn mirror(dir: &Path) -> Arc<DB> {
    let path = dir.join(format!("mirror-{}", fastrand::u64(..)));
    let db = Arc::new(DB::create(path).expect("a local db"));
    RemoteDb::new(GATEWAY.parse().expect("the gateway address")).spawn(db.clone());
    db
}

/// The newest sample of a scalar `u64` component, `None` while the component
/// has no data (or has not been mirrored at all).
fn scalar(db: &DB, name: &str) -> Option<u64> {
    db.with_state(|state| {
        let component = state.get_component(ComponentId::new(name))?;
        let latest = component.time_series.latest()?;
        Some(u64::from_le_bytes(latest.data().try_into().ok()?))
    })
}

/// Whether the mirror knows `name` at all: metadata crosses on connect even
/// for a component that has never published.
fn known(db: &DB, name: &str) -> bool {
    db.with_state(|state| {
        state
            .get_component_metadata(ComponentId::new(name))
            .is_some()
    })
}

/// The `source` of every `LogEvent` the mirror holds.
fn log_sources(db: &Arc<DB>) -> Vec<String> {
    let Ok(log) =
        db.with_state_mut(|state| state.get_or_insert_msg_log(LogEvent::ID, &db.path).cloned())
    else {
        return Vec::new();
    };
    let _ = log.flush();
    let Some(slice) = log.get_range(Timestamp(i64::MIN)..Timestamp(i64::MAX)) else {
        return Vec::new();
    };
    slice
        .as_iter()
        .flat_map(|node| {
            node.msgs()
                .filter_map(|(_, msg)| postcard::from_bytes::<LogEvent>(msg).ok())
                .map(|event| event.source)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Poll `pred` every 50ms until it holds, or report `what` at the deadline.
async fn wait_for(deadline: Instant, what: &str, mut pred: impl FnMut() -> bool) -> Watch {
    loop {
        if pred() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("never saw {what}"));
        }
        stellarator::sleep(Duration::from_millis(50)).await;
    }
}

/// Case 1's assertion, over a mirror of a running gateway: both members'
/// components arrived, both sources read connected, and a log line from each
/// member crossed too.
async fn both_sources_live(db: &Arc<DB>, deadline: Instant) -> Watch {
    wait_for(deadline, "both members' components", || {
        scalar(db, "a.coordinator.system_status.cycles").is_some()
            && scalar(db, "b.coordinator.system_status.cycles").is_some()
    })
    .await?;
    wait_for(deadline, "both sources connected", || {
        scalar(db, "gw.a.source_status.connected") == Some(1)
            && scalar(db, "gw.b.source_status.connected") == Some(1)
    })
    .await?;

    // A probe that connects and drops leaves each member's downlink one
    // closed connection to report, so each logs a line that reaches the
    // gateway only over the ingest.
    for addr in [A_LINK, B_LINK] {
        drop(dial(addr.parse().expect("a link address"), deadline).await);
    }
    wait_for(deadline, "a log line from each member", || {
        let sources = log_sources(db);
        sources.iter().any(|source| source == "a_downlink")
            && sources.iter().any(|source| source == "b_downlink")
    })
    .await
}

/// Count `WiringManifest` broadcasts on one member's ground link until the
/// task is dropped. The link replays its retained announce on connect, so
/// the first count is the baseline and a re-broadcast is the second.
async fn count_manifests(addr: SocketAddr, seen: Rc<Cell<u64>>, deadline: Instant) {
    let Some(Peer::Fsw { mut rx, .. }) = dial(addr, deadline).await else {
        return;
    };
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let Ok(pkt) = rx.next_grow(buf).await else {
            return;
        };
        if let OwnedPacket::Msg(msg) = &pkt
            && msg.id == WiringManifest::ID
        {
            seen.set(seen.get() + 1);
        }
        buf = pkt.into_buf().into_inner();
    }
}

/// Cases 1 and 2 over one mirror of a live deployment.
async fn mirror_and_command(dir: PathBuf, deadline: Instant) -> Watch {
    let db = served_mirror(&dir, deadline).await?;
    both_sources_live(&db, deadline).await?;

    let (a_seen, b_seen) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let _a = stellarator::spawn(count_manifests(
        A_LINK.parse().expect("`a`'s link address"),
        a_seen.clone(),
        deadline,
    ))
    .drop_guard();
    let _b = stellarator::spawn(count_manifests(
        B_LINK.parse().expect("`b`'s link address"),
        b_seen.clone(),
        deadline,
    ))
    .drop_guard();
    wait_for(deadline, "each link's retained manifest", || {
        a_seen.get() >= 1 && b_seen.get() >= 1
    })
    .await?;
    if (a_seen.get(), b_seen.get()) != (1, 1) {
        return Err(format!(
            "one announce each, not {} and {}",
            a_seen.get(),
            b_seen.get()
        ));
    }

    // Only `b` forwards `ReloadSequences`, and only `b` routes it on to its
    // coordinator, so the re-broadcast happens there and nowhere else.
    db.push_msg(
        Timestamp::now(),
        ReloadSequences::ID,
        &postcard::to_allocvec(&ReloadSequences {}).expect("the command encodes"),
    )
    .expect("the command is pushed");
    wait_for(deadline, "`b`'s re-broadcast", || b_seen.get() == 2).await?;
    rest(Instant::now() + Duration::from_secs(1)).await;
    if (a_seen.get(), b_seen.get()) != (1, 2) {
        return Err(format!(
            "the command reached `b` alone, not {} and {}",
            a_seen.get(),
            b_seen.get()
        ));
    }
    Ok(())
}

/// Case 5's assertion: case 1 again, over a fresh mirror.
async fn sources_live(dir: PathBuf, deadline: Instant) -> Watch {
    let db = served_mirror(&dir, deadline).await?;
    both_sources_live(&db, deadline).await
}

/// Case 3's assertion: the first session is live, then — once the test has
/// killed and restarted `a` on the `restart` signal — the same source reads a
/// second session and `a`'s own components move again.
async fn source_reconnects(dir: PathBuf, restart: Sender<()>, deadline: Instant) -> Watch {
    let db = served_mirror(&dir, deadline).await?;
    wait_for(deadline, "`a`'s first session", || {
        scalar(&db, "gw.a.source_status.sessions") == Some(1)
            && scalar(&db, "gw.a.source_status.connected") == Some(1)
    })
    .await?;
    if restart.send(()).is_err() {
        return Err("the test stopped waiting to restart `a`".into());
    }

    wait_for(deadline, "the restarted member's second session", || {
        scalar(&db, "gw.a.source_status.sessions") == Some(2)
            && scalar(&db, "gw.a.source_status.connected") == Some(1)
    })
    .await?;
    let resumed = scalar(&db, "a.coordinator.system_status.cycles").unwrap_or_default();
    wait_for(deadline, "`a`'s cycles advancing again", || {
        scalar(&db, "a.coordinator.system_status.cycles").is_some_and(|now| now > resumed)
    })
    .await
}

/// Case 4's assertion: the gateway alone announces both sources and never
/// reports one connected. The gauge publishes on change and starts at zero,
/// so a source that never answers has metadata and no sample at all.
async fn sources_stay_down(dir: PathBuf, deadline: Instant) -> Watch {
    let db = served_mirror(&dir, deadline).await?;
    wait_for(deadline, "both sources announced", || {
        known(&db, "gw.a.source_status.connected") && known(&db, "gw.b.source_status.connected")
    })
    .await?;
    match (
        scalar(&db, "gw.a.source_status.connected"),
        scalar(&db, "gw.b.source_status.connected"),
    ) {
        (None, None) => Ok(()),
        (a, b) => Err(format!("a lone gateway published {a:?} and {b:?}")),
    }
}

/// Watch a running deployment from its own thread and runtime, take the
/// deployment down, and hand back what the watcher saw and what it logged.
fn watched(child: Child, body: impl FnOnce() -> Watch + Send + 'static) -> (Watch, String) {
    let watcher = std::thread::spawn(body);
    let watched = watcher.join().expect("the watcher thread ends");
    (watched, kill(child))
}

#[test]
#[ignore = "the db mirror stalls for ~60 s in about 1 run in 5 (design-deployment-gateway.md, decision 11); run by hand with --ignored"]
fn gateway_ingests_a_deployment() {
    if !have_python() {
        eprintln!("skipping gateway_ingests_a_deployment: no python3 >= 3.10 on PATH");
        return;
    }
    let temp = tempfile::tempdir().expect("a temp dir");

    // Cases 1 and 2: the whole deployment under the launcher, mirrored from
    // a ground db, and one command pushed back up through the gateway.
    let child = spawn(&["run", "gateway_target.py"]);
    let dir = temp.path().to_path_buf();
    let (seen, stderr) = watched(child, move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        stellarator::run(|| mirror_and_command(dir, deadline))
    });
    seen.unwrap_or_else(|what| panic!("{what}:\n{stderr}"));
    assert!(
        stderr.contains("source link connected addr=127.0.0.1:2256 source=a/link")
            && stderr.contains("source link connected addr=127.0.0.1:2257 source=b/link"),
        "both ingests report their connection:\n{stderr}"
    );

    // Case 3: a member restarts. The launcher fails the run when one of its
    // children dies, so the three members run as their own processes here
    // and the test owns `a`'s.
    let mut children: Vec<Child> = ["b", "gw"]
        .iter()
        .map(|target| spawn(&["run", "gateway_target.py", "--target", target]))
        .collect();
    let mut a = spawn(&["run", "gateway_target.py", "--target", "a"]);
    let (restart, wait) = std::sync::mpsc::channel();
    let dir = temp.path().to_path_buf();
    let restarted = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        stellarator::run(|| source_reconnects(dir, restart, deadline))
    });
    if wait.recv_timeout(Duration::from_secs(60)).is_ok() {
        let _ = kill(a);
        a = spawn(&["run", "gateway_target.py", "--target", "a"]);
    }
    let restarted = restarted.join().expect("the watcher thread ends");
    children.push(a);
    let stderr: String = children.into_iter().map(kill).collect();
    restarted.unwrap_or_else(|what| panic!("{what}:\n{stderr}"));

    // Case 4: the gateway alone. Its sources never answer, and the run still
    // ends cleanly.
    let dir = temp.path().to_path_buf();
    let watcher = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        stellarator::run(|| sources_stay_down(dir, deadline))
    });
    let output = run(&[
        "run",
        "gateway_target.py",
        "--target",
        "gw",
        "--cycles",
        "200",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    watcher
        .join()
        .expect("the watcher thread ends")
        .unwrap_or_else(|what| panic!("{what}:\n{stderr}"));

    // Case 5: the same three members from bundles, cargo-free.
    let mut bundles = Vec::new();
    for target in ["a", "b", "gw"] {
        let bundle = temp.path().join(format!("{target}.bundle"));
        let output = run(&[
            "package",
            "gateway_target.py",
            "--target",
            target,
            "-o",
            bundle.to_str().expect("a utf-8 path"),
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        bundles.push(bundle);
    }
    let child = spawn(&[
        "run",
        bundles[0].to_str().expect("a utf-8 path"),
        bundles[1].to_str().expect("a utf-8 path"),
        bundles[2].to_str().expect("a utf-8 path"),
    ]);
    let dir = temp.path().to_path_buf();
    let (seen, stderr) = watched(child, move || {
        let deadline = Instant::now() + Duration::from_secs(60);
        stellarator::run(|| sources_live(dir, deadline))
    });
    seen.unwrap_or_else(|what| panic!("{what}:\n{stderr}"));
    assert!(!stderr.contains("panicked"), "{stderr}");
}
