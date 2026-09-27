//! A `DlSlot` over the echo pack's two sequences, driven by channel commands.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

use metor_fsw_3::cli::cargo::{cargo_build, cdylib_name, triple};
use metor_fsw_3::cli::config::{CONFIG_VERSION, DeploymentConfig, TargetConfig};
use metor_fsw_3::deploy::{self, DeployError};
use metor_fsw_3::dl::store;
use metor_fsw_3::metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};
use metor_fsw_3::{
    Adapter, BuildError, ChannelConfig, ChannelState, ChannelStatus, Coordinator,
    CoordinatorConfig, DefCx, Input, InputConfig, Output, Pack, Record, RunState, SystemConfig,
    SystemTable, Timestamp, system,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The pack's record, redeclared here: the component id is the name's hash.
#[derive(Record, metor_fsw_3::Schema, Serialize, Deserialize, Clone, Copy, Debug)]
#[postcard(crate = metor_fsw_3::postcard_schema)]
#[record(max_len = 8)]
struct Ping {
    n: u32,
}

/// What the test systems saw, and the commands still to send.
#[derive(Default)]
struct Seen {
    queue: VecDeque<SequenceCommandKind>,
    events: Vec<SequenceEventKind>,
    pings: Vec<u32>,
    status: Option<ChannelStatus>,
}

type Shared = Rc<RefCell<Seen>>;

struct Commander(Shared);

#[system]
fn commander(#[state] c: &mut Commander, commands: &mut Output<SequenceCommand>) {
    while let Some(command) = c.0.borrow_mut().queue.pop_front() {
        let _ = commands.write(&SequenceCommand {
            channel: "mode".into(),
            command,
        });
    }
}

#[system]
fn idle(output: &mut Output<Ping>) {
    let _ = output;
}

struct Watch(Shared);

#[system]
fn watch(
    #[state] w: &mut Watch,
    events: &mut Input<SequenceChannelEvent>,
    status: &mut Input<ChannelStatus>,
    pings: &mut Input<Ping>,
) {
    let mut seen = w.0.borrow_mut();
    let events = events.drain().filter_map(Result::ok);
    seen.events.extend(events.map(|event| event.kind));
    let pings: Vec<u32> = pings.drain().filter_map(Result::ok).map(|p| p.n).collect();
    seen.pings.extend(pings);
    if let Ok(Some(status)) = status.latest() {
        seen.status = Some(*status);
    }
}

/// Builds the fixture once per test binary and returns its cdylib.
fn fixture() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        cargo_build("echo-pack", root, false).expect("the fixture builds")
    })
}

/// `ty`'s ports as `metor build` would bake them for instance `mode`.
fn baked(ty: &str) -> Value {
    // SAFETY: the fixture is a metor-fsw-3 pack this workspace just built.
    let pack = unsafe { Pack::open(fixture()) }.expect("the fixture opens");
    let cx = DefCx {
        id: "mode",
        ..DefCx::empty()
    };
    serde_json::to_value(pack.def(ty, &cx).expect("a definition")).expect("encodes")
}

/// A resolved candidate: the fixture's path and hash, and `ty`'s ports.
fn candidate(name: &str, ty: &str) -> Value {
    let hash = store::hash(fixture()).expect("the fixture reads");
    json!({
        "name": name,
        "ty": ty,
        "path": fixture(),
        "hash": hash.to_hex().as_str(),
        "def": baked(ty),
    })
}

/// `count_by` with a stride, whose constructor refuses zero.
fn stride(name: &str, by: u32) -> Value {
    let mut candidate = candidate(name, "count_by");
    candidate["params"] = json!({ "by": by });
    candidate
}

fn slot_params(allow: Vec<Value>) -> Value {
    json!({ "allow": allow })
}

/// `cmds -> idle -> mode (the slot) -> watch`.
fn config(params: Value, adapter: Option<Adapter>) -> CoordinatorConfig {
    let mut inputs = vec![InputConfig::new("input", "idle", "output")];
    if adapter.is_some() {
        inputs.push(InputConfig::new("commands", "cmds", "commands"));
    }
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("cmds", "commander"),
            SystemConfig::new("idle", "idle"),
            SystemConfig {
                params,
                adapter,
                inputs,
                ..SystemConfig::new("mode", "fsw.dl_slot")
            },
            SystemConfig {
                inputs: vec![
                    InputConfig::new("events", "mode", "events"),
                    InputConfig::new("status", "mode", "channel_status"),
                    InputConfig::new("pings", "mode", "output"),
                ],
                ..SystemConfig::new("watch", "watch")
            },
        ],
        ..Default::default()
    }
}

fn channel() -> Option<Adapter> {
    Some(Adapter::Channel(ChannelConfig {
        name: "mode".into(),
        start: RunState::Stopped,
    }))
}

fn table(seen: &Shared) -> SystemTable {
    let mut table = SystemTable::new();
    metor_fsw_3::link::register_builtins(&mut table).expect("valid records");
    let s = seen.clone();
    table
        .register("commander", commander(), move || Commander(s.clone()))
        .expect("valid records");
    table
        .register("idle", idle(), Default::default)
        .expect("valid records");
    let s = seen.clone();
    table
        .register("watch", watch(), move || Watch(s.clone()))
        .expect("valid records");
    table
}

fn both() -> Vec<Value> {
    vec![candidate("up", "count_up"), candidate("down", "count_down")]
}

fn build(params: Value) -> (Shared, Coordinator) {
    let seen = Shared::default();
    let coordinator = config(params, channel())
        .build(&table(&seen))
        .expect("the slot builds");
    (seen, coordinator)
}

/// Sends `commands` and runs one cycle, returning the events it produced.
fn send(
    seen: &Shared,
    coordinator: &mut Coordinator,
    commands: impl IntoIterator<Item = SequenceCommandKind>,
) -> Vec<SequenceEventKind> {
    seen.borrow_mut().queue.extend(commands);
    run(seen, coordinator, 1)
}

/// Runs `cycles` cycles, returning the events they produced.
fn run(seen: &Shared, coordinator: &mut Coordinator, cycles: usize) -> Vec<SequenceEventKind> {
    for _ in 0..cycles {
        let next = Timestamp(coordinator.cycle() as i64 + 1);
        coordinator.step(next);
    }
    std::mem::take(&mut seen.borrow_mut().events)
}

fn load(name: &str) -> SequenceCommandKind {
    SequenceCommandKind::Load { name: name.into() }
}

fn pings(seen: &Shared) -> Vec<u32> {
    std::mem::take(&mut seen.borrow_mut().pings)
}

fn status(seen: &Shared) -> ChannelStatus {
    seen.borrow().status.expect("a status")
}

fn refused(events: &[SequenceEventKind], text: &str) -> bool {
    matches!(events, [SequenceEventKind::Refused { reason }] if reason.contains(text))
}

#[test]
fn test_load_start_complete() {
    let (seen, mut coordinator) = build(slot_params(both()));
    run(&seen, &mut coordinator, 1);
    assert_eq!(status(&seen).state, ChannelState::Unloaded.code());
    assert_eq!(status(&seen).loaded, u32::MAX);

    let events = send(&seen, &mut coordinator, [load("up")]);
    assert_eq!(events, [SequenceEventKind::Loaded { name: "up".into() }]);
    let loaded = status(&seen);
    assert_eq!(loaded.loaded, 0);
    let file = fixture()
        .file_name()
        .and_then(|n| n.to_str())
        .expect("a name");
    assert_eq!(loaded.artifact.to_str(), Ok(file));
    let hash = store::hash(fixture()).expect("reads");
    assert_eq!(loaded.hash.0, *hash.as_bytes());

    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    let events = run(&seen, &mut coordinator, 4);
    assert_eq!(events, [SequenceEventKind::Completed]);
    assert_eq!(pings(&seen), [1, 2, 3]);
    assert_eq!(status(&seen).state, ChannelState::Completed.code());
}

#[test]
fn test_load_other_then_abort() {
    let (seen, mut coordinator) = build(slot_params(both()));
    send(&seen, &mut coordinator, [load("down")]);
    assert_eq!(status(&seen).loaded, 1);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    run(&seen, &mut coordinator, 2);
    assert_eq!(pings(&seen), [0]);

    // The sequence sees the abort on the cycle it is sent, and ends there.
    let events = send(&seen, &mut coordinator, [SequenceCommandKind::Abort]);
    let requested = SequenceEventKind::Progress {
        detail: "abort requested".into(),
    };
    assert_eq!(events, [requested, SequenceEventKind::Aborted]);
    assert_eq!(status(&seen).state, ChannelState::Aborted.code());
}

#[test]
fn test_reset_restarts_loaded() {
    let (seen, mut coordinator) = build(slot_params(both()));
    send(&seen, &mut coordinator, [load("up")]);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    run(&seen, &mut coordinator, 1);
    assert_eq!(pings(&seen), [1, 2]);

    let reset = [SequenceCommandKind::Stop, SequenceCommandKind::Reset];
    send(&seen, &mut coordinator, reset);
    assert_eq!(status(&seen).loaded, 0);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    assert_eq!(pings(&seen), [1]);
}

#[test]
fn test_unknown_load_keeps_occupant() {
    let (seen, mut coordinator) = build(slot_params(both()));
    send(&seen, &mut coordinator, [load("up")]);
    let events = send(&seen, &mut coordinator, [load("sideways")]);
    assert!(refused(&events, "sideways"), "{events:?}");
    assert_eq!(status(&seen).loaded, 0);
}

/// A library whose bytes are not the ones build hashed is refused before
/// it is mapped, and the occupant stays.
#[test]
fn test_changed_bytes_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let copy = dir.path().join("libecho_copy.so");
    std::fs::copy(fixture(), &copy).expect("copies");
    let mut stale = candidate("down", "count_down");
    stale["path"] = json!(copy);
    stale["hash"] = json!(blake3::hash(b"another build").to_hex().as_str());
    let allow = vec![candidate("up", "count_up"), stale];
    let (seen, mut coordinator) = build(slot_params(allow));
    send(&seen, &mut coordinator, [load("up")]);
    let events = send(&seen, &mut coordinator, [load("down")]);
    assert!(refused(&events, "not the library"), "{events:?}");
    assert_eq!(status(&seen).loaded, 0);
}

/// A candidate of a type whose ports differ from the slot's is refused, and
/// the occupant stays.
#[test]
fn test_other_type_refused() {
    let mut other = candidate("gain", "gain");
    other["params"] = json!({ "gain": 2.0 });
    let allow = vec![candidate("up", "count_up"), other];
    let (seen, mut coordinator) = build(slot_params(allow));
    send(&seen, &mut coordinator, [load("up")]);
    let events = send(&seen, &mut coordinator, [load("gain")]);
    assert!(refused(&events, "no longer matches"), "{events:?}");
    assert_eq!(status(&seen).loaded, 0);
}

/// A slot whose baked ports no longer match the library refuses the load.
#[test]
fn test_stale_baked_ports_refused() {
    let mut stale = candidate("up", "count_up");
    stale["def"] = baked("gain");
    let (seen, mut coordinator) = build(slot_params(vec![stale]));
    run(&seen, &mut coordinator, 1);
    let events = send(&seen, &mut coordinator, [load("up")]);
    assert!(refused(&events, "no longer matches"), "{events:?}");
    assert_eq!(status(&seen).state, ChannelState::Unloaded.code());
}

/// A candidate whose constructor refuses is refused, and the occupant is
/// put back over the rings it released.
#[test]
fn test_refused_constructor_keeps_occupant() {
    let allow = vec![candidate("up", "count_up"), stride("zero", 0)];
    let (seen, mut coordinator) = build(slot_params(allow));
    send(&seen, &mut coordinator, [load("up")]);
    let events = send(&seen, &mut coordinator, [load("zero")]);
    assert!(refused(&events, "`by` is zero"), "{events:?}");
    assert_eq!(status(&seen).loaded, 0);
    assert_eq!(status(&seen).state, ChannelState::Stopped.code());

    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    let events = run(&seen, &mut coordinator, 4);
    assert_eq!(events, [SequenceEventKind::Completed]);
    assert_eq!(pings(&seen), [1, 2, 3]);
}

#[test]
fn test_initial_loads_at_build() {
    let mut params = slot_params(both());
    params["initial"] = json!("down");
    let (seen, mut coordinator) = build(params);
    run(&seen, &mut coordinator, 1);
    assert_eq!(status(&seen).loaded, 1);
    assert_eq!(status(&seen).state, ChannelState::Stopped.code());
}

/// A bad `initial` reports why the slot is empty, and blocks nothing else.
#[test]
fn test_bad_initial_fails_the_slot_only() {
    let mut params = slot_params(vec![candidate("up", "count_up"), stride("zero", 0)]);
    params["initial"] = json!("zero");
    let (seen, mut coordinator) = build(params);
    let events = run(&seen, &mut coordinator, 1);
    assert!(
        matches!(events.as_slice(), [SequenceEventKind::Failed { reason }] if reason.contains("zero")),
        "{events:?}"
    );
    assert_eq!(status(&seen).state, ChannelState::Failed.code());

    let events = send(&seen, &mut coordinator, [load("up")]);
    assert_eq!(events, [SequenceEventKind::Loaded { name: "up".into() }]);
    assert_eq!(status(&seen).state, ChannelState::Stopped.code());
}

/// A reset restarts the last load without going through `initial`.
#[test]
fn test_reset_skips_initial() {
    let mut params = slot_params(vec![candidate("up", "count_up"), stride("two", 2)]);
    params["initial"] = json!("up");
    let (seen, mut coordinator) = build(params);
    send(&seen, &mut coordinator, [load("two")]);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    run(&seen, &mut coordinator, 1);
    assert_eq!(pings(&seen), [2, 4]);

    let reset = [SequenceCommandKind::Stop, SequenceCommandKind::Reset];
    let events = send(&seen, &mut coordinator, reset);
    assert_eq!(events[1], SequenceEventKind::Loaded { name: "two".into() });
    assert_eq!(status(&seen).loaded, 1);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    assert_eq!(pings(&seen), [2]);
}

#[test]
fn test_slot_needs_channel() {
    let seen = Shared::default();
    let mut config = config(slot_params(both()), None);
    // Without a channel there are no events or status to watch.
    config.systems.pop();
    let Err(error) = config.build(&table(&seen)) else {
        panic!("a slot without a channel is refused")
    };
    assert!(
        matches!(error, BuildError::SlotWithoutChannel { ref id } if id == "mode"),
        "{error:?}"
    );
}

#[test]
fn test_unresolved_slot_refused() {
    let seen = Shared::default();
    let params = json!({ "allow": [{ "name": "up", "ty": "count_up" }] });
    let Err(error) = config(params, channel()).build(&table(&seen)) else {
        panic!("an unresolved slot is refused")
    };
    assert!(error.to_string().contains("metor build"), "{error}");
}

/// A pack reference whose libs directory holds a copy of the fixture.
fn pack_ref(libs: &Path) -> Value {
    let dir = libs.join(triple());
    std::fs::create_dir_all(&dir).expect("creates");
    std::fs::copy(fixture(), dir.join(cdylib_name("echo_pack"))).expect("copies");
    json!({ "id": "echo", "lib": "echo_pack", "libs": libs })
}

/// A candidate as Python emits it: a pack reference, no path, hash, or ports.
fn unresolved(name: &str, ty: &str, libs: &Path) -> Value {
    json!({ "name": name, "ty": ty, "pack": pack_ref(libs) })
}

fn envelope(allow: Vec<Value>) -> DeploymentConfig {
    DeploymentConfig {
        config_version: CONFIG_VERSION,
        hosts: Default::default(),
        targets: vec![TargetConfig {
            packs: Vec::new(),
            coordinator: config(json!({ "allow": allow }), channel()),
        }],
    }
}

fn slot(envelope: &DeploymentConfig) -> &Value {
    &envelope.targets[0].coordinator.systems[2].params
}

#[test]
fn test_build_resolves_into_store() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (libs, store) = (dir.path().join("libs"), dir.path().join("store"));
    let seen = Shared::default();
    let allow = vec![
        unresolved("up", "count_up", &libs),
        unresolved("down", "count_down", &libs),
    ];
    let mut envelope = envelope(allow);
    assert!(deploy::unresolved(&envelope));
    deploy::resolve_slots(&mut envelope, &[table(&seen)], &store).expect("resolves");
    assert!(!deploy::unresolved(&envelope));

    let params = slot(&envelope);
    let hash = store::hash(fixture()).expect("reads");
    for candidate in params["allow"].as_array().expect("a list") {
        assert!(candidate.get("pack").is_none());
        assert_eq!(candidate["hash"], json!(hash.to_hex().as_str()));
        let path = PathBuf::from(candidate["path"].as_str().expect("a path"));
        assert!(path.starts_with(&store), "{}", path.display());
        assert!(candidate["def"].is_object());
    }

    // Resolving again changes nothing.
    let before = params.clone();
    deploy::resolve_slots(&mut envelope, &[table(&seen)], &store).expect("resolves");
    assert_eq!(*slot(&envelope), before);

    let mut coordinator = envelope.targets[0]
        .coordinator
        .clone()
        .build(&table(&seen))
        .expect("the resolved slot builds");
    send(&seen, &mut coordinator, [load("up")]);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    let events = run(&seen, &mut coordinator, 4);
    assert_eq!(events, [SequenceEventKind::Completed]);
}

/// Slot `next`, reading slot `mode`'s output.
fn next_slot(libs: &Path) -> SystemConfig {
    SystemConfig {
        params: json!({ "allow": [unresolved("down", "count_down", libs)] }),
        adapter: Some(Adapter::Channel(ChannelConfig {
            name: "next".into(),
            start: RunState::Stopped,
        })),
        inputs: vec![InputConfig::new("input", "mode", "output")],
        ..SystemConfig::new("next", "fsw.dl_slot")
    }
}

/// A slot reading another slot's output resolves against that slot's baked ports.
#[test]
fn test_build_resolves_chained_slots() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (libs, store) = (dir.path().join("libs"), dir.path().join("store"));
    let mut envelope = envelope(vec![unresolved("up", "count_up", &libs)]);
    envelope.targets[0]
        .coordinator
        .systems
        .push(next_slot(&libs));
    deploy::resolve_slots(&mut envelope, &[table(&Shared::default())], &store).expect("resolves");
    assert!(!deploy::unresolved(&envelope));
    let next = &envelope.targets[0].coordinator.systems[4].params;
    assert!(next["allow"][0]["def"].is_object());
}

/// A slot reading a slot later in step order is refused.
#[test]
fn test_build_refuses_slot_reading_later_slot() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (libs, store) = (dir.path().join("libs"), dir.path().join("store"));
    let mut envelope = envelope(vec![unresolved("up", "count_up", &libs)]);
    let systems = &mut envelope.targets[0].coordinator.systems;
    let mode = systems.iter().position(|s| s.id == "mode").expect("a slot");
    systems.insert(mode, next_slot(&libs));
    let resolved = deploy::resolve_slots(&mut envelope, &[table(&Shared::default())], &store);
    let Err(error) = resolved else {
        panic!("a slot reading a later slot resolved")
    };
    assert!(error.to_string().contains("mode"), "{error}");
}

#[test]
fn test_build_refuses_disagreeing_candidates() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let libs = dir.path().join("libs");
    let mut gain = unresolved("gain", "gain", &libs);
    gain["params"] = json!({ "gain": 2.0 });
    let mut envelope = envelope(vec![unresolved("up", "count_up", &libs), gain]);
    let result = deploy::resolve_slots(&mut envelope, &[table(&Shared::default())], dir.path());
    let Err(DeployError::SlotContract {
        first, candidate, ..
    }) = result
    else {
        panic!("gain's ports differ from count_up's: {result:?}")
    };
    assert_eq!((first.as_str(), candidate.as_str()), ("up", "gain"));
}

#[test]
fn test_build_refuses_missing_pack() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut missing = unresolved("up", "count_up", &dir.path().join("libs"));
    missing["pack"]["libs"] = json!(dir.path().join("nowhere"));
    let mut missing = envelope(vec![missing]);
    let result = deploy::resolve_slots(&mut missing, &[table(&Shared::default())], dir.path());
    assert!(
        matches!(result, Err(DeployError::Slot { .. })),
        "{result:?}"
    );
    let mut bare = envelope(vec![json!({ "name": "up", "ty": "count_up" })]);
    let result = deploy::resolve_slots(&mut bare, &[table(&Shared::default())], dir.path());
    assert!(
        matches!(result, Err(DeployError::Slot { .. })),
        "{result:?}"
    );
}

#[test]
fn test_build_refuses_long_library_name() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut long = unresolved("up", "count_up", &dir.path().join("libs"));
    long["pack"]["lib"] = json!("l".repeat(64));
    let result = deploy::resolve_slots(
        &mut envelope(vec![long]),
        &[table(&Shared::default())],
        dir.path(),
    );
    assert!(
        matches!(result, Err(DeployError::ArtifactName { cap: 63, .. })),
        "{result:?}"
    );
}
