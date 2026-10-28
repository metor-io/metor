//! Tests a `DlSlot` that swaps between the echo pack's sequences, driven by
//! channel commands.

mod common;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use common::{Ping, fixture_lib, fixture_wasm};
use metor_fsw_3::deploy::{self, DeployError};
use metor_fsw_3::deploy::{CONFIG_VERSION, DeploySystem, DeploymentConfig, TargetConfig};
use metor_fsw_3::dl::store;
use metor_fsw_3::dl::{WASM_TRIPLE, cdylib_name, triple, wasm_name};
use metor_fsw_3::metor_proto_wkt::{SequenceChannelState, SequenceCommand, SequenceCommandKind};
use metor_fsw_3::wasm::PAGE;
use metor_fsw_3::{
    Adapter, BuildError, ChannelConfig, ChannelStatus, Coordinator, CoordinatorConfig, DefCx,
    DlPack, Input, InputConfig, Output, RunState, SystemConfig, SystemTable, Timestamp, system,
};
use serde_json::{Value, json};

/// What the test systems have seen, plus the commands still to send.
#[derive(Default)]
struct Seen {
    queue: VecDeque<SequenceCommandKind>,
    /// Each message the channel raised, in order.
    messages: Vec<String>,
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
fn watch(#[state] w: &mut Watch, status: &mut Input<ChannelStatus>, pings: &mut Input<Ping>) {
    let mut seen = w.0.borrow_mut();
    let pings: Vec<u32> = pings.drain().filter_map(Result::ok).map(|p| p.n).collect();
    seen.pings.extend(pings);
    if let Ok(Some(status)) = status.latest() {
        let raised = seen.status.map(|s| s.message_at) != Some(status.message_at);
        let text = status.message.to_str().expect("utf-8");
        if raised && !text.is_empty() {
            seen.messages.push(text.to_string());
        }
        seen.status = Some(*status);
    }
}

/// Returns `ty`'s ports as `metor build` would resolve them for instance
/// `mode`.
fn baked(ty: &str) -> Value {
    // SAFETY: the fixture is a metor-fsw-3 pack this workspace just built.
    let pack = unsafe { DlPack::open(fixture_lib()) }.expect("the fixture opens");
    let cx = DefCx {
        id: "mode",
        ..DefCx::empty()
    };
    serde_json::to_value(pack.def(ty, &cx).expect("a definition")).expect("encodes")
}

/// Returns a resolved candidate that points at the fixture library, with its
/// hash and `ty`'s ports.
fn candidate(name: &str, ty: &str) -> Value {
    let hash = store::hash(fixture_lib()).expect("the fixture reads");
    json!({
        "name": name,
        "ty": ty,
        "stored": {
            "path": fixture_lib(),
            "hash": hash.to_hex().as_str(),
            "def": baked(ty),
        },
    })
}

/// Returns a resolved candidate for the fixture's wasm module, after copying
/// the module into `dir`.
fn wasm_candidate(name: &str, ty: &str, dir: &Path) -> Value {
    let path = dir.join(wasm_name("echo_pack"));
    std::fs::write(&path, fixture_wasm()).expect("writes");
    json!({
        "name": name,
        "ty": ty,
        "stored": {
            "path": path,
            "hash": blake3::hash(fixture_wasm()).to_hex().as_str(),
            "def": baked(ty),
            "kind": "wasm",
        },
    })
}

/// Returns a `count_by` candidate with stride `by`. Its constructor rejects
/// zero.
fn stride(name: &str, by: u32) -> Value {
    let mut candidate = candidate(name, "count_by");
    candidate["params"] = json!({ "by": by });
    candidate
}

fn slot_params(allow: Vec<Value>) -> Value {
    json!({ "allow": allow })
}

/// Builds `cmds -> idle -> mode (the slot) -> watch`.
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
        name: metor_fsw_3::ChannelName::try_from("mode".to_string()).expect("a short name"),
        start: RunState::Stopped,
    }))
}

fn table(seen: &Shared) -> SystemTable {
    let mut table = SystemTable::new();
    metor_fsw_3::builtins::register_builtins(&mut table).expect("valid records");
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

/// Sends `commands` and runs one cycle. Returns the message it raised, if
/// any; a cycle shows only its last.
fn send(
    seen: &Shared,
    coordinator: &mut Coordinator,
    commands: impl IntoIterator<Item = SequenceCommandKind>,
) -> Vec<String> {
    seen.borrow_mut().queue.extend(commands);
    run(seen, coordinator, 1)
}

/// Runs `cycles` cycles. Returns the messages they raised.
fn run(seen: &Shared, coordinator: &mut Coordinator, cycles: usize) -> Vec<String> {
    for _ in 0..cycles {
        let next = Timestamp(coordinator.cycle() as i64 + 1);
        coordinator.step(next);
    }
    std::mem::take(&mut seen.borrow_mut().messages)
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

fn refused(messages: &[String], text: &str) -> bool {
    matches!(messages, [m] if m.starts_with("refused: ") && m.contains(text))
}

fn code(state: SequenceChannelState) -> u32 {
    state.code()
}

#[test]
fn test_load_start_complete() {
    let (seen, mut coordinator) = build(slot_params(both()));
    run(&seen, &mut coordinator, 1);
    assert_eq!(status(&seen).state, code(SequenceChannelState::Unloaded));
    assert_eq!(status(&seen).loaded, u32::MAX);

    let messages = send(&seen, &mut coordinator, [load("up")]);
    assert_eq!(messages, ["loaded up"]);
    let loaded = status(&seen);
    assert_eq!(loaded.loaded, 0);
    let file = fixture_lib()
        .file_name()
        .and_then(|n| n.to_str())
        .expect("a name");
    assert_eq!(loaded.artifact.to_str(), Ok(file));
    let hash = store::hash(fixture_lib()).expect("reads");
    assert_eq!(loaded.hash.0, *hash.as_bytes());

    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    let messages = run(&seen, &mut coordinator, 4);
    assert_eq!(messages, ["completed"]);
    assert_eq!(pings(&seen), [1, 2, 3]);
    assert_eq!(status(&seen).state, code(SequenceChannelState::Completed));
}

#[test]
fn test_load_other_then_abort() {
    let (seen, mut coordinator) = build(slot_params(both()));
    send(&seen, &mut coordinator, [load("down")]);
    assert_eq!(status(&seen).loaded, 1);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    run(&seen, &mut coordinator, 2);
    assert_eq!(pings(&seen), [0]);

    // The sequence sees the abort on the same cycle it is sent, and ends on
    // that cycle, so its end is the cycle's message.
    let messages = send(&seen, &mut coordinator, [SequenceCommandKind::Abort]);
    assert_eq!(messages, ["aborted"]);
    assert_eq!(status(&seen).state, code(SequenceChannelState::Aborted));
}

#[test]
fn test_unknown_load_keeps_occupant() {
    let (seen, mut coordinator) = build(slot_params(both()));
    send(&seen, &mut coordinator, [load("up")]);
    let messages = send(&seen, &mut coordinator, [load("sideways")]);
    assert!(refused(&messages, "sideways"), "{messages:?}");
    assert_eq!(status(&seen).loaded, 0);
}

/// A library whose bytes differ from the ones `metor build` hashed is
/// rejected before it is loaded, and the current occupant keeps running.
#[test]
fn test_changed_bytes_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let copy = dir.path().join("libecho_copy.so");
    std::fs::copy(fixture_lib(), &copy).expect("copies");
    let mut stale = candidate("down", "count_down");
    stale["stored"]["path"] = json!(copy);
    stale["stored"]["hash"] = json!(blake3::hash(b"another build").to_hex().as_str());
    let allow = vec![candidate("up", "count_up"), stale];
    let (seen, mut coordinator) = build(slot_params(allow));
    send(&seen, &mut coordinator, [load("up")]);
    let messages = send(&seen, &mut coordinator, [load("down")]);
    assert!(refused(&messages, "not the library"), "{messages:?}");
    assert_eq!(status(&seen).loaded, 0);
}

/// A wasm module and a native library can take turns in one slot.
#[test]
fn test_wasm_and_dylib_swap() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let allow = vec![
        wasm_candidate("up", "count_up", dir.path()),
        candidate("down", "count_down"),
    ];
    let (seen, mut coordinator) = build(slot_params(allow));
    let messages = send(&seen, &mut coordinator, [load("up")]);
    assert_eq!(messages, ["loaded up"]);
    assert_eq!(status(&seen).artifact.to_str(), Ok("echo_pack.wasm"));

    send(&seen, &mut coordinator, [load("down")]);
    assert_eq!(status(&seen).loaded, 1);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    run(&seen, &mut coordinator, 2);
    assert_eq!(pings(&seen), [0]);
    send(&seen, &mut coordinator, [SequenceCommandKind::Abort]);

    send(&seen, &mut coordinator, [load("up")]);
    assert_eq!(status(&seen).loaded, 0);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    let messages = run(&seen, &mut coordinator, 4);
    assert_eq!(messages, ["completed"]);
    assert_eq!(pings(&seen), [1, 2, 3]);
}

/// A wasm module whose bytes changed after `metor build` hashed them is
/// rejected, and the current occupant keeps running.
#[test]
fn test_changed_module_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let stale = wasm_candidate("down", "count_down", dir.path());
    let path = PathBuf::from(stale["stored"]["path"].as_str().expect("a path"));
    let mut bytes = fixture_wasm().to_vec();
    bytes.push(0);
    std::fs::write(&path, bytes).expect("writes");
    let allow = vec![candidate("up", "count_up"), stale];
    let (seen, mut coordinator) = build(slot_params(allow));
    send(&seen, &mut coordinator, [load("up")]);
    let messages = send(&seen, &mut coordinator, [load("down")]);
    assert!(refused(&messages, "not the library"), "{messages:?}");
    assert_eq!(status(&seen).loaded, 0);
}

/// A candidate's `fuel` and `memory` limits apply to its guest.
///
/// `spin` with little fuel fails on its first step, and a one-page memory
/// limit is rejected.
#[test]
fn test_candidate_grants_reach_the_guest() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut spin = wasm_candidate("spin", "spin", dir.path());
    spin["stored"]["fuel"] = json!(1_000);
    let mut small = wasm_candidate("small", "spin", dir.path());
    small["stored"]["memory"] = json!(PAGE);
    let (seen, mut coordinator) = build(slot_params(vec![spin, small]));
    run(&seen, &mut coordinator, 1);

    let messages = send(&seen, &mut coordinator, [load("small")]);
    assert!(refused(&messages, "ceiling"), "{messages:?}");
    send(&seen, &mut coordinator, [load("spin")]);
    let messages = send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    assert_eq!(messages, ["failed: out of fuel"]);
}

/// A candidate whose ports differ from the slot's is rejected, and the
/// current occupant keeps running.
#[test]
fn test_other_type_refused() {
    let mut other = candidate("gain", "gain");
    other["params"] = json!({ "gain": 2.0 });
    let allow = vec![candidate("up", "count_up"), other];
    let (seen, mut coordinator) = build(slot_params(allow));
    send(&seen, &mut coordinator, [load("up")]);
    let messages = send(&seen, &mut coordinator, [load("gain")]);
    assert!(refused(&messages, "no longer matches"), "{messages:?}");
    assert_eq!(status(&seen).loaded, 0);
}

/// A candidate whose constructor fails is rejected. The previous occupant is
/// restored on the rings it gave up.
#[test]
fn test_refused_constructor_keeps_occupant() {
    let allow = vec![candidate("up", "count_up"), stride("zero", 0)];
    let (seen, mut coordinator) = build(slot_params(allow));
    send(&seen, &mut coordinator, [load("up")]);
    let messages = send(&seen, &mut coordinator, [load("zero")]);
    assert!(refused(&messages, "`by` is zero"), "{messages:?}");
    assert_eq!(status(&seen).loaded, 0);
    assert_eq!(status(&seen).state, code(SequenceChannelState::Stopped));

    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    let messages = run(&seen, &mut coordinator, 4);
    assert_eq!(messages, ["completed"]);
    assert_eq!(pings(&seen), [1, 2, 3]);
}

/// An async candidate cannot run behind the slot's channel.
#[test]
fn test_async_candidate_is_refused() {
    let (seen, mut coordinator) = build(slot_params(vec![candidate("relay", "relay")]));
    // The empty slot reports itself unloaded on its first cycle.
    run(&seen, &mut coordinator, 1);
    let messages = send(&seen, &mut coordinator, [load("relay")]);
    let refusal = "refused: `relay` cannot run behind the slot's adapter";
    assert_eq!(messages, [refusal]);
}

#[test]
fn test_initial_loads_at_build() {
    let mut params = slot_params(both());
    params["initial"] = json!("down");
    let (seen, mut coordinator) = build(params);
    run(&seen, &mut coordinator, 1);
    assert_eq!(status(&seen).loaded, 1);
    assert_eq!(status(&seen).state, code(SequenceChannelState::Stopped));
}

/// A bad `initial` candidate leaves the slot empty and reports why. The rest
/// of the target keeps running.
#[test]
fn test_bad_initial_fails_the_slot_only() {
    let mut params = slot_params(vec![candidate("up", "count_up"), stride("zero", 0)]);
    params["initial"] = json!("zero");
    let (seen, mut coordinator) = build(params);
    let messages = run(&seen, &mut coordinator, 1);
    assert!(
        matches!(messages.as_slice(), [m] if m.starts_with("failed: ") && m.contains("zero")),
        "{messages:?}"
    );
    assert_eq!(status(&seen).state, code(SequenceChannelState::Failed));

    let messages = send(&seen, &mut coordinator, [load("up")]);
    assert_eq!(messages, ["loaded up"]);
    assert_eq!(status(&seen).state, code(SequenceChannelState::Stopped));
}

/// A reset restarts the last loaded candidate, not `initial`.
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
    let messages = send(&seen, &mut coordinator, reset);
    assert_eq!(messages, ["loaded two"]);
    assert_eq!(status(&seen).loaded, 1);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    assert_eq!(pings(&seen), [2]);
}

#[test]
fn test_slot_needs_channel() {
    let seen = Shared::default();
    let mut config = config(slot_params(both()), None);
    // Without a channel there is no status, so drop the watcher.
    config.systems.pop();
    let Err(error) = config.build(&table(&seen)) else {
        panic!("a slot without a channel is refused")
    };
    assert!(
        matches!(
            error,
            BuildError::UnsupportedAdapter { ref id, ref adapter, .. } if id == "mode" && adapter == "without an adapter"
        ),
        "{error:?}"
    );
}

#[test]
fn test_unresolved_slot_refused() {
    let seen = Shared::default();
    let dir = tempfile::tempdir().expect("a temp dir");
    let params = json!({ "allow": [unresolved("up", "count_up", dir.path())] });
    let Err(error) = config(params, channel()).build(&table(&seen)) else {
        panic!("an unresolved slot is refused")
    };
    assert!(error.to_string().contains("`stored`"), "{error}");
}

/// Copies the fixture library into `libs` and returns a pack reference to it.
fn pack_ref(libs: &Path) -> Value {
    let dir = libs.join(triple());
    std::fs::create_dir_all(&dir).expect("creates");
    std::fs::copy(fixture_lib(), dir.join(cdylib_name("echo_pack"))).expect("copies");
    json!({ "id": "echo", "lib": "echo_pack", "libs": libs })
}

/// Returns a candidate the way Python emits it: a pack reference, with no
/// path, hash, or ports yet.
fn unresolved(name: &str, ty: &str, libs: &Path) -> Value {
    json!({ "name": name, "ty": ty, "pack": pack_ref(libs) })
}

/// A runtime config as a deployment writes it.
fn deployed<T: serde::Serialize, D: serde::de::DeserializeOwned>(runtime: T) -> D {
    let json = serde_json::to_value(runtime).expect("encodes");
    serde_json::from_value(json).expect("a runtime config is a deployment config")
}

fn envelope(allow: Vec<Value>) -> DeploymentConfig {
    DeploymentConfig {
        config_version: CONFIG_VERSION,
        hosts: Default::default(),
        targets: vec![TargetConfig {
            packs: Vec::new(),
            coordinator: deployed(config(json!({ "allow": allow }), channel())),
        }],
    }
}

/// Returns a temp dir, plus paths for a libs dir and a store inside it.
fn scratch() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let (libs, store) = (dir.path().join("libs"), dir.path().join("store"));
    (dir, libs, store)
}

fn resolve(envelope: &mut DeploymentConfig, store: &Path) -> Result<(), DeployError> {
    resolve_with(envelope, &table(&Shared::default()), store)
}

/// Resolves whatever `envelope` has pending against `table`.
fn resolve_with(
    envelope: &mut DeploymentConfig,
    table: &SystemTable,
    store: &Path,
) -> Result<(), DeployError> {
    match deploy::pending(envelope)? {
        Some(pending) => pending.resolve(envelope, std::slice::from_ref(table), store),
        None => Ok(()),
    }
}

fn slot(envelope: &DeploymentConfig) -> &Value {
    &envelope.targets[0].coordinator.systems[2].params
}

#[test]
fn test_build_resolves_into_store() {
    let (_dir, libs, store) = scratch();
    let seen = Shared::default();
    let allow = vec![
        unresolved("up", "count_up", &libs),
        unresolved("down", "count_down", &libs),
    ];
    let mut envelope = envelope(allow);
    assert!(deploy::pending(&envelope).expect("decodes").is_some());
    resolve_with(&mut envelope, &table(&seen), &store).expect("resolves");
    assert!(deploy::pending(&envelope).expect("decodes").is_none());

    let params = slot(&envelope);
    let hash = store::hash(fixture_lib()).expect("reads");
    for candidate in params["allow"].as_array().expect("a list") {
        assert!(candidate.get("pack").is_none());
        let stored = &candidate["stored"];
        assert_eq!(stored["hash"], json!(hash.to_hex().as_str()));
        let path = PathBuf::from(stored["path"].as_str().expect("a path"));
        assert!(path.starts_with(&store), "{}", path.display());
        assert!(stored["def"].is_object());
    }

    // Resolving again changes nothing.
    let before = params.clone();
    resolve_with(&mut envelope, &table(&seen), &store).expect("resolves");
    assert_eq!(*slot(&envelope), before);

    let runtime = envelope.into_runtime().expect("no peers");
    let mut coordinator = runtime.targets[0]
        .coordinator
        .clone()
        .build(&table(&seen))
        .expect("the resolved slot builds");
    send(&seen, &mut coordinator, [load("up")]);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    let messages = run(&seen, &mut coordinator, 4);
    assert_eq!(messages, ["completed"]);
}

/// A wasm pack reference resolves to its stored module, keeping its fuel and
/// memory limits.
#[test]
fn test_build_resolves_wasm_candidate() {
    let (_dir, libs, store) = scratch();
    let modules = libs.join(WASM_TRIPLE);
    std::fs::create_dir_all(&modules).expect("creates");
    std::fs::write(modules.join(wasm_name("echo_pack")), fixture_wasm()).expect("writes");
    let pack = json!({
        "id": "echo", "lib": "echo_pack", "libs": libs,
        "kind": "wasm", "memory": 32 << 20, "fuel": 1_000_000,
    });
    let mut envelope = envelope(vec![
        json!({ "name": "up", "ty": "count_up", "pack": pack }),
    ]);
    resolve(&mut envelope, &store).expect("resolves");

    let candidate = &slot(&envelope)["allow"][0]["stored"];
    let path = PathBuf::from(candidate["path"].as_str().expect("a path"));
    assert!(path.starts_with(&store), "{}", path.display());
    assert!(path.ends_with(wasm_name("echo_pack")), "{}", path.display());
    let hash = blake3::hash(fixture_wasm());
    assert_eq!(candidate["hash"], json!(hash.to_hex().as_str()));
    assert_eq!(candidate["kind"], json!("wasm"));
    assert_eq!(candidate["memory"], json!(32 << 20));
    assert_eq!(candidate["fuel"], json!(1_000_000));

    let seen = Shared::default();
    let runtime = envelope.into_runtime().expect("no peers");
    let mut coordinator = runtime.targets[0]
        .coordinator
        .clone()
        .build(&table(&seen))
        .expect("the resolved slot builds");
    send(&seen, &mut coordinator, [load("up")]);
    send(&seen, &mut coordinator, [SequenceCommandKind::Start]);
    let messages = run(&seen, &mut coordinator, 4);
    assert_eq!(messages, ["completed"]);
}

/// Returns the config for slot `next`, which reads slot `mode`'s output.
fn next_slot(libs: &Path) -> DeploySystem {
    deployed::<SystemConfig, _>(SystemConfig {
        params: json!({ "allow": [unresolved("down", "count_down", libs)] }),
        adapter: Some(Adapter::Channel(ChannelConfig {
            name: metor_fsw_3::ChannelName::try_from("next".to_string()).expect("a short name"),
            start: RunState::Stopped,
        })),
        inputs: vec![InputConfig::new("input", "mode", "output")],
        ..SystemConfig::new("next", "fsw.dl_slot")
    })
}

/// A slot that reads another slot's output resolves against that slot's
/// resolved ports.
#[test]
fn test_build_resolves_chained_slots() {
    let (_dir, libs, store) = scratch();
    let mut envelope = envelope(vec![unresolved("up", "count_up", &libs)]);
    envelope.targets[0]
        .coordinator
        .systems
        .push(next_slot(&libs));
    resolve(&mut envelope, &store).expect("resolves");
    assert!(deploy::pending(&envelope).expect("decodes").is_none());
    let next = &envelope.targets[0].coordinator.systems[4].params;
    assert!(next["allow"][0]["stored"]["def"].is_object());
}

/// A slot that reads from a slot later in step order is rejected.
#[test]
fn test_build_refuses_slot_reading_later_slot() {
    let (_dir, libs, store) = scratch();
    let mut envelope = envelope(vec![unresolved("up", "count_up", &libs)]);
    let systems = &mut envelope.targets[0].coordinator.systems;
    let mode = systems.iter().position(|s| s.id == "mode").expect("a slot");
    systems.insert(mode, next_slot(&libs));
    let Err(error) = resolve(&mut envelope, &store) else {
        panic!("a slot reading a later slot resolved")
    };
    assert!(error.to_string().contains("mode"), "{error}");
}

#[test]
fn test_build_refuses_disagreeing_candidates() {
    let (_dir, libs, store) = scratch();
    let mut gain = unresolved("gain", "gain", &libs);
    gain["params"] = json!({ "gain": 2.0 });
    let mut envelope = envelope(vec![unresolved("up", "count_up", &libs), gain]);
    let result = resolve(&mut envelope, &store);
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
    let (dir, libs, store) = scratch();
    let mut missing = unresolved("up", "count_up", &libs);
    missing["pack"]["libs"] = json!(dir.path().join("nowhere"));
    let result = resolve(&mut envelope(vec![missing]), &store);
    assert!(
        matches!(result, Err(DeployError::Slot { .. })),
        "{result:?}"
    );
    let bare = json!({ "name": "up", "ty": "count_up" });
    let result = resolve(&mut envelope(vec![bare]), &store);
    assert!(
        matches!(result, Err(DeployError::Slot { .. })),
        "{result:?}"
    );
}

#[test]
fn test_build_refuses_long_library_name() {
    let (_dir, libs, store) = scratch();
    let mut long = unresolved("up", "count_up", &libs);
    long["pack"]["lib"] = json!("l".repeat(64));
    let result = resolve(&mut envelope(vec![long]), &store);
    assert!(
        matches!(result, Err(DeployError::ArtifactName { cap: 63, .. })),
        "{result:?}"
    );
}
