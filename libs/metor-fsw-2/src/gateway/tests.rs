use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;

use metor_proto_stellar::{Peer, identify};
use metor_proto_wkt::{FEATURE_MSG_SYNC, NODE_PROTOCOL_VERSION};

use super::*;
use crate::SharedLifecycle;

fn params(addr: SocketAddr) -> DbParams {
    DbParams {
        addr,
        path: None,
        name: None,
        store: None,
        max_bytes: None,
        max_age_secs: None,
    }
}

fn loopback() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 0))
}

/// The served db answers a probe as a db, under the member's namespace and
/// the current node protocol, and its default store is a fresh temp dir.
#[cfg(not(miri))]
#[stellarator::test]
async fn db_state_serves_identify() {
    let mut state = DbState::open(params(loopback()), Some("gw"))
        .expect("the db opens")
        .with_identity("db", None);
    let addr = state.local_addr();
    let path = state.db().path.clone();
    assert_eq!(path.parent(), Some(std::env::temp_dir().as_path()));
    assert!(
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("metor-gw-gw-")),
        "the default store names the member: {path:?}"
    );
    state.start();
    assert!(path.is_dir(), "the default store was created");

    let Peer::Db(info) = identify(addr).await.expect("the db answers") else {
        panic!("a gateway identifies as a db")
    };
    assert_eq!(info.namespace.as_deref(), Some("gw"));
    assert_eq!(info.protocol_version, NODE_PROTOCOL_VERSION);
    assert_ne!(info.features & FEATURE_MSG_SYNC, 0);
    assert!(info.command_ids.is_empty(), "no ingest advertised commands");

    state.shutdown();
    let _ = std::fs::remove_dir_all(&path);
}

/// A taken port is a construction error, so it surfaces at resolve.
#[cfg(not(miri))]
#[stellarator::test]
async fn db_state_reports_a_taken_port() {
    let first = DbState::open(params(loopback()), Some("gw")).expect("the db opens");
    let taken = first.local_addr();
    let Err(err) = DbState::open(params(taken), Some("gw")) else {
        panic!("the port is taken")
    };
    assert!(
        err.to_string().contains("io"),
        "the bind failure is reported: {err}"
    );
    let _ = std::fs::remove_dir_all(&first.db().path);
}

/// An explicit path is used as given, and reopened in place.
#[cfg(not(miri))]
#[stellarator::test]
async fn db_state_opens_a_configured_path() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("store");
    let mut p = params(loopback());
    p.path = Some(path.clone());
    let state = DbState::open(p, Some("gw")).expect("the db opens");
    assert_eq!(state.db().path, path);
    drop(state);

    let mut p = params(loopback());
    p.path = Some(path.clone());
    DbState::open(p, Some("gw")).expect("the db reopens");
}

/// A cyclic system attached to the `Db` state, standing in for the ingest
/// and record systems until they land: it proves the state constructs,
/// starts, and grants its `Arc<DB>` from inside a cycle.
struct Probe {
    db: crate::Shared<DbState>,
    cycles: Rc<Cell<u64>>,
}

#[derive(crate::SystemOutput)]
struct ProbeOut {}

impl crate::System for Probe {
    type Input = ();
    type Output = crate::Out<ProbeOut>;
    const NAME: &'static str = "probe";
}

impl crate::CyclicSystem for Probe {
    fn execute(
        &mut self,
        _now: metor_proto::types::Timestamp,
        _input: &mut (),
        _output: &mut Self::Output,
    ) {
        assert!(Arc::strong_count(self.db.get().db()) > 0);
        self.cycles.set(self.cycles.get() + 1);
    }
}

impl crate::BuildSystem for Probe {
    type Params = ();
    fn new(_params: ()) -> Self {
        unreachable!("the pack ctor builds every probe")
    }
}

/// A target declaring a `Db` resolves against the built-in registry and
/// runs, the db served for the whole run.
#[cfg(not(miri))]
#[stellarator::test]
async fn a_target_with_a_db_resolves_and_runs() {
    use crate::wiring::{Registry, WiringBuilder, resolve};

    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let wiring = WiringBuilder::new()
        .coordinator(1000.0, crate::ClockSpec::Wall)
        .db("db", addr)
        .system("probe")
        .ty("Probe")
        .attach("db")
        .end()
        .build();
    let cycles = Rc::new(Cell::new(0));
    let counter = cycles.clone();
    let mut registry = Registry::with_builtins();
    let mut probes = crate::Pack::new();
    probes = probes.system_type_shared::<Probe, DbState>("Probe", move |(), db| Probe {
        db,
        cycles: counter.clone(),
    });
    registry.register_pack(probes);

    let mut coord = resolve(&wiring, &registry).expect("the gateway target resolves");
    coord.run_for(10).await;
    assert_eq!(cycles.get(), 10, "the attached system stepped every cycle");
}

/// A frame the recorder stores; `every` cycles apart so a test can tell a
/// per-change write from a per-cycle one.
#[derive(
    crate::Frame,
    zerocopy::IntoBytes,
    zerocopy::Immutable,
    zerocopy::KnownLayout,
    zerocopy::FromBytes,
    Default,
)]
#[repr(C)]
#[metor_fsw(name = "tick")]
struct Tick {
    #[metor_fsw(timestamp)]
    timestamp: metor_proto::types::Timestamp,
    count: u64,
}

#[derive(serde::Serialize, serde::Deserialize, postcard_schema::Schema, Debug, Clone, Default)]
struct TickerParams {
    /// Publish on every `every`-th cycle.
    every: u64,
}

#[derive(crate::SystemOutput)]
struct TickerOut {
    tick: crate::Output<Tick>,
}

/// Publishes one frame every `every` cycles.
struct Ticker {
    every: u64,
    count: u64,
}

impl crate::System for Ticker {
    type Input = ();
    type Output = crate::Out<TickerOut>;
    const NAME: &'static str = "ticker";
}

impl crate::CyclicSystem for Ticker {
    fn execute(
        &mut self,
        timestamp: metor_proto::types::Timestamp,
        _input: &mut (),
        output: &mut Self::Output,
    ) {
        self.count += 1;
        if self.count == 1 {
            output.log().log(crate::LogLevel::Info, "ticker started");
        }
        if self.count.is_multiple_of(self.every) {
            output.tick.publish(&Tick {
                timestamp,
                count: self.count,
            });
        }
    }
}

impl crate::BuildSystem for Ticker {
    type Params = TickerParams;
    fn new(params: TickerParams) -> Self {
        Self {
            every: params.every.max(1),
            count: 0,
        }
    }
}

/// A free loopback port: bound to learn the number, released so the state
/// under test can take it.
fn free_port() -> u16 {
    let listener =
        stellarator::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    listener.local_addr().unwrap().port()
}

fn source(namespace: &str, port: u16, commands: &[&str]) -> crate::IngestParams {
    crate::IngestParams {
        namespace: namespace.into(),
        link: "link".into(),
        port,
        commands: commands.iter().map(|c| c.to_string()).collect(),
        host: Some("127.0.0.1".into()),
    }
}

fn gateway(addr: SocketAddr) -> crate::wiring::WiringBuilder {
    crate::wiring::WiringBuilder::new()
        .coordinator(1000.0, crate::ClockSpec::Wall)
        .db("db", addr)
}

/// Two ingests' command tokens resolve in `configure` and reach the db's
/// advertisement before it serves its first probe.
#[cfg(not(miri))]
#[stellarator::test]
async fn ingests_advertise_the_union_of_their_commands() {
    use crate::wiring::{Registry, resolve};
    use metor_proto::types::Msg;

    let addr = SocketAddr::from(([127, 0, 0, 1], free_port()));
    let mut wiring = gateway(addr)
        .ingest("a", "db", source("a", 1, &["AlarmAck"]))
        .ingest("b", "db", source("b", 2, &["ReloadSequences"]))
        .build();
    wiring.coordinator.namespace = Some("gw".into());

    let mut coord = resolve(&wiring, &Registry::with_builtins()).expect("the gateway resolves");
    let run = stellarator::spawn(async move { coord.run_for(50).await }).drop_guard();

    let Peer::Db(info) = identify(addr).await.expect("the db answers") else {
        panic!("a gateway identifies as a db")
    };
    assert_eq!(info.namespace.as_deref(), Some("gw"));
    assert!(info.command_ids.contains(&metor_proto_wkt::AlarmAck::ID));
    assert!(
        info.command_ids
            .contains(&metor_proto_wkt::ReloadSequences::ID)
    );
    drop(run);
}

/// A command token no registered message carries is a resolve error naming it.
#[test]
fn an_unknown_command_token_is_a_resolve_error() {
    use crate::wiring::{Registry, resolve};

    let addr = SocketAddr::from(([127, 0, 0, 1], 0));
    let wiring = gateway(addr)
        .ingest("a", "db", source("a", 1, &["NoSuchCommand"]))
        .build();
    let Err(err) = resolve(&wiring, &Registry::with_builtins()) else {
        panic!("the token is unknown")
    };
    assert!(
        err.to_string().contains("NoSuchCommand"),
        "the error names the token: {err}"
    );
}

/// Hands the shared db's handle back to a test, so the recorded store is
/// read in process while the target still owns it.
struct Grab {
    db: crate::Shared<DbState>,
    out: Rc<std::cell::RefCell<Option<Arc<metor_db::DB>>>>,
}

impl crate::System for Grab {
    type Input = ();
    type Output = crate::Out<ProbeOut>;
    const NAME: &'static str = "grab";
}

impl crate::CyclicSystem for Grab {
    fn execute(
        &mut self,
        _now: metor_proto::types::Timestamp,
        _input: &mut (),
        _output: &mut Self::Output,
    ) {
        let mut out = self.out.borrow_mut();
        if out.is_none() {
            *out = Some(self.db.get().db().clone());
        }
    }
}

impl crate::BuildSystem for Grab {
    type Params = ();
    fn new(_params: ()) -> Self {
        unreachable!("the pack ctor builds every grab")
    }
}

/// The recorder stores this target's own frames, the coordinator's status,
/// and its log lines, straight from the rings — a snapshot tap once per
/// publish, not once per cycle.
#[cfg(not(miri))]
#[stellarator::test]
async fn record_stores_the_targets_outputs() {
    use crate::wiring::{Registry, WiringBuilder, resolve};
    use metor_proto::types::Msg;

    let wiring = WiringBuilder::new()
        .coordinator(1000.0, crate::ClockSpec::Wall)
        .db("db", loopback())
        .system("ticker")
        .ty("Ticker")
        .params_value(serde_json::json!({ "every": 2 }))
        .end()
        .system("grab")
        .ty("Grab")
        .attach("db")
        .end()
        .record("record", "db")
        .build();

    let handle: Rc<std::cell::RefCell<Option<Arc<metor_db::DB>>>> = Default::default();
    let out = handle.clone();
    let mut registry = Registry::with_builtins();
    registry.register::<Ticker, _>("Ticker");
    let mut probes = crate::Pack::new();
    probes = probes.system_type_shared::<Grab, DbState>("Grab", move |(), db| Grab {
        db,
        out: out.clone(),
    });
    registry.register_pack(probes);

    let mut coord = resolve(&wiring, &registry).expect("the gateway target resolves");
    coord.run_for(11).await;

    let db = handle.borrow().clone().expect("the db handle");
    let ticks = samples(&db, "ticker.tick.count");
    assert!(
        (1..=5).contains(&ticks),
        "a snapshot tap writes once per publish, not once per cycle: {ticks}"
    );
    assert!(
        samples(&db, "coordinator.system_status.cycles") > 0,
        "the coordinator's status is recorded"
    );
    let logs = db
        .with_state_mut(|s| {
            s.get_or_insert_msg_log(crate::LogEvent::ID, &db.path)
                .cloned()
        })
        .expect("the log's message log");
    assert!(
        logs.latest().is_some(),
        "the target's log lines are recorded"
    );
    let path = db.path.clone();
    drop(db);
    drop(coord);
    stellarator::sleep(std::time::Duration::from_millis(20)).await;
    let _ = std::fs::remove_dir_all(path);
}

/// The number of samples the recorded store holds for one component.
#[cfg(not(miri))]
fn samples(db: &metor_db::DB, component: &str) -> usize {
    use metor_proto::types::{ComponentId, Timestamp};

    db.with_state(|s| {
        s.get_component(ComponentId::new(component))
            .and_then(|c| {
                c.time_series
                    .get_range(Timestamp(i64::MIN)..Timestamp(i64::MAX))
                    .map(|slice| slice.len())
            })
            .unwrap_or(0)
    })
}
