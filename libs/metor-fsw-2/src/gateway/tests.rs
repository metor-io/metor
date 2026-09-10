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
