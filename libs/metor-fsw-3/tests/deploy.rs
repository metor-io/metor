//! Runs the fixture deployment with `metor run`.
//!
//! Target `a` publishes samples, `b` subscribes to them and sends pings, and
//! the gateway's db records both.

mod common;

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use common::{
    DEADLINE, Deadline, Fsw, Running, Source, Target, dial_fsw, fixture_dir, free_ports, local,
    metor, ports_env, source,
};
use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::coordinator::{CoordinatorConfig, InputConfig, SystemConfig, SystemTable};
use metor_fsw_3::{Clock, LinkStatus, Timestamp, WallRate};
use metor_proto::types::{ComponentId, Msg, OwnedPacket};
use metor_proto_stellar::Client;
use metor_proto_wkt::{DumpMetadata, DumpMetadataResp, GetTimeSeries, LogEvent};
use serde_json::json;
use zerocopy::FromBytes;

/// How many cycles a target runs before it exits on its own.
///
/// This is longer than any test here needs. It also bounds how long a
/// child can outlive a killed run.
const CYCLES: &str = "20000";

const PORTS_ENV: &str = "METOR_DEPLOY_PORTS";

fn run(target: &Path, ports: &[u16; 3], args: &[&str]) -> Running {
    let child = metor()
        .arg("run")
        .arg(target)
        .args(args)
        .env(PORTS_ENV, ports_env(ports))
        .stdout(Stdio::null())
        .spawn()
        .expect("metor runs");
    Running(child)
}

/// Dials the db until it answers.
async fn connect(addr: std::net::SocketAddr) -> Client {
    let deadline = Deadline::new();
    loop {
        if let Ok(client) = Client::connect(addr).await {
            return client;
        }
        deadline.tick("the db never came up").await;
    }
}

async fn metadata(client: &mut Client) -> DumpMetadataResp {
    client.request(&DumpMetadata).await.expect("the db answers")
}

#[stellarator::test]
async fn test_gateway_records_every_target() {
    let ports = free_ports();
    let _running = run(
        &fixture_dir().join("deploy.py"),
        &ports,
        &["--cycles", CYCLES],
    );
    let mut client = connect(local(ports[2])).await;

    // A subscribed frame keeps its original names, so `b`'s copy of the
    // sample lands in the same table as `a`'s. `b`'s link status is named
    // under `b`.
    let deadline = Deadline::new();
    let wanted = ["a.sampler.sample.value", "b.mirror.link_status.connections"];
    loop {
        let dump = metadata(&mut client).await;
        let names: Vec<&str> = dump
            .component_metadata
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        if wanted.iter().all(|name| names.contains(name))
            && dump.msg_metadata.iter().any(|m| m.name == "Ping")
        {
            break;
        }
        deadline.tick(format!("announced only {names:?}")).await;
    }

    // Samples only show up in a time series once the db has flushed its log,
    // so queries fail until then.
    let component_id = ComponentId::new(wanted[0]);
    loop {
        let series = client
            .request(&GetTimeSeries {
                id: [0, 0],
                range: Timestamp(0)..Timestamp(i64::MAX / 2),
                component_id,
                limit: Some(64),
            })
            .await;
        // The table gets each sample from `a` and again through `b`, so a
        // timestamp may repeat, but it never goes backwards.
        if let Ok(series) = &series
            && let Ok(stamps) = series.timestamps()
            && stamps.len() >= 2
            && stamps.first() != stamps.last()
        {
            assert!(stamps.windows(2).all(|w| w[0] <= w[1]), "{stamps:?}");
            break;
        }
        deadline
            .tick(format!("the series never filled: {series:?}"))
            .await;
    }
}

/// Runs `metor build` on the fixture deployment and returns the resolved
/// config it wrote.
fn build(ports: Option<&[u16; 3]>, out: &Path) -> serde_json::Value {
    let mut command = metor();
    command
        .args([
            "build",
            &fixture_dir().join("deploy.py").display().to_string(),
            "-o",
        ])
        .arg(out);
    if let Some(ports) = ports {
        command.env(PORTS_ENV, ports_env(ports));
    }
    let output = command.output().expect("metor builds");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&std::fs::read(out).expect("written")).expect("json")
}

#[test]
fn test_build_matches_golden() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut built = build(None, &dir.path().join("deploy.json"));
    let libs = fixture_dir().join(".metor/echo_pack/_libs");
    for target in built["targets"].as_array_mut().expect("targets") {
        for pack in target["packs"].as_array_mut().expect("packs") {
            assert_eq!(pack["libs"], json!(libs.display().to_string()));
            pack["libs"] = json!("<libs>");
        }
        // The unit tests in `deploy` pin the resolved peer port definitions,
        // so they are dropped here.
        for system in target["coordinator"]["systems"]
            .as_array_mut()
            .expect("systems")
        {
            for output in system["outputs"].as_array_mut().expect("outputs") {
                let def = output.as_object_mut().and_then(|o| o.remove("def"));
                assert!(def.is_some_and(|def| def.is_object()), "{output}");
            }
        }
    }
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/deploy.json");
    let expected: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&golden).expect("the golden exists")).expect("json");
    assert_eq!(built, expected);
}

/// A target run from a resolved config only opens its own packs.
#[test]
fn test_resolved_target_opens_only_its_packs() {
    let ports = free_ports();
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dir.path().join("deploy.json");
    let mut built = build(Some(&ports), &out);
    for target in built["targets"].as_array_mut().expect("targets") {
        if target["coordinator"]["namespace"] != "b" {
            for pack in target["packs"].as_array_mut().expect("packs") {
                pack["libs"] = json!("/nonexistent/_libs");
            }
        }
    }
    std::fs::write(&out, serde_json::to_vec(&built).expect("encodes")).expect("writes");
    let output = metor()
        .args([
            "run",
            &out.display().to_string(),
            "--target",
            "b",
            "--cycles",
            "3",
        ])
        .output()
        .expect("metor runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// An in-process target that stands in for `a`, but serves a different frame.
///
/// It announces right away, and starts sending data once `gate` is set.
struct Impostor {
    _target: Target,
    gate: Arc<AtomicBool>,
}

/// Starts an impostor that serves `imu.imu` as target `a`'s `pub` link on
/// `port`. Its table differs from the one `b` was built against.
fn impostor(port: u16) -> Impostor {
    let gate = Arc::new(AtomicBool::new(false));
    let open = gate.clone();
    let table = move || {
        let mut table = SystemTable::new();
        table
            .register("source", source(), move || Source(0, Some(open.clone())))
            .expect("valid records");
        register_builtins(&mut table).expect("valid records");
        table
    };
    let config = CoordinatorConfig {
        clock: Clock::Wall {
            rate: WallRate::try_from(1000.0).expect("a valid rate"),
        },
        namespace: Some("a".into()),
        systems: vec![
            SystemConfig::new("imu", "source"),
            SystemConfig {
                params: json!({
                    "transport": { "listen": { "addr": format!("127.0.0.1:{port}"), "max_connections": 2 } },
                    "namespace": "a",
                    "link": "pub",
                }),
                inputs: vec![InputConfig::new("imu.imu", "imu", "imu")],
                ..SystemConfig::new("pub", "fsw.publish")
            },
        ],
        ..Default::default()
    };
    Impostor {
        _target: Target::spawn(config, table, Arc::new(AtomicU64::new(0))),
        gate,
    }
}

#[stellarator::test]
async fn test_peer_announcing_another_table_is_faulted() {
    let ports = free_ports();
    let dir = tempfile::tempdir().expect("a temp dir");
    let out = dir.path().join("deploy.json");
    let built = build(Some(&ports), &out);
    let mirror = &built["targets"][1]["coordinator"]["systems"][0];
    assert_eq!(
        mirror["params"]["transport"]["connect"]["addr"],
        json!(format!("127.0.0.1:{}", ports[0])),
        "{mirror}"
    );
    let impostor = impostor(ports[0]);
    let Fsw { info, .. } = dial_fsw(local(ports[0])).await;
    assert_eq!(info.link, "pub");
    let _running = run(&out, &ports, &["--target", "b", "--cycles", CYCLES]);
    // The fault is logged only once, on the peer's first data packet, so the
    // observer connects before the impostor sends any.
    let Fsw { mut rx, buf, .. } = dial_fsw(local(ports[1])).await;
    impostor.gate.store(true, Ordering::Release);
    let mut buf = buf;
    let mut seen: Vec<String> = Vec::new();
    let deadline = Instant::now() + DEADLINE;
    loop {
        assert!(
            Instant::now() < deadline,
            "no mismatch fault arrived; saw {seen:?}"
        );
        let taken = core::mem::take(&mut buf);
        let packet = rx.next_grow(taken).await.expect("the link stays up");
        buf = vec![0u8; 64 * 1024];
        let OwnedPacket::Msg(m) = packet else {
            if let OwnedPacket::Table(t) = &packet
                && let Ok(status) = LinkStatus::read_from_bytes(&t.buf)
                && !seen.last().is_some_and(|s| s.contains("connections"))
            {
                seen.push(format!(
                    "connections {} out {}",
                    status.connections, status.bytes_out
                ));
            }
            continue;
        };
        if m.id != LogEvent::ID {
            seen.push(format!("msg {:?}", m.id));
            continue;
        }
        let event = m.parse::<LogEvent>().expect("a log event");
        let kind = event.fields.iter().find(|(name, _)| name == "kind");
        seen.push(format!("log {kind:?} {}", event.message));
        if kind.is_some_and(|(_, kind)| kind == "peer_schema_mismatch") {
            assert!(
                event.message.contains("sampler.sample"),
                "{}",
                event.message
            );
            return;
        }
    }
}
