//! Tests a `DlSlot` that swaps between the echo pack's sequences, driven by
//! channel commands.

mod common;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;

use common::{Ping, fixture_lib};
use metor_fsw_3::dl::store;
use metor_fsw_3::dl::{cdylib_name, triple};
use metor_fsw_3::metor_proto_wkt::{
    SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};
use metor_fsw_3::{
    Adapter, BuildError, ChannelConfig, ChannelState, ChannelStatus, Coordinator,
    CoordinatorConfig, DefCx, DlPack, Input, InputConfig, Output, RunState, SystemConfig,
    SystemTable, Timestamp, system,
};
use serde_json::{Value, json};

/// What the test systems have seen, plus the commands still to send.
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
        "source": { "stored": {
            "path": fixture_lib(),
            "hash": hash.to_hex().as_str(),
            "def": baked(ty),
        }},
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

/// Sends `commands` and runs one cycle. Returns the events it produced.
fn send(
    seen: &Shared,
    coordinator: &mut Coordinator,
    commands: impl IntoIterator<Item = SequenceCommandKind>,
) -> Vec<SequenceEventKind> {
    seen.borrow_mut().queue.extend(commands);
    run(seen, coordinator, 1)
}

/// Runs `cycles` cycles. Returns the events they produced.
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
    let file = fixture_lib()
        .file_name()
        .and_then(|n| n.to_str())
        .expect("a name");
    assert_eq!(loaded.artifact.to_str(), Ok(file));
    let hash = store::hash(fixture_lib()).expect("reads");
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

    // The sequence sees the abort on the same cycle it is sent, and ends on
    // that cycle.
    let events = send(&seen, &mut coordinator, [SequenceCommandKind::Abort]);
    let requested = SequenceEventKind::Progress {
        detail: "abort requested".into(),
    };
    assert_eq!(events, [requested, SequenceEventKind::Aborted]);
    assert_eq!(status(&seen).state, ChannelState::Aborted.code());
}

#[test]
fn test_unknown_load_keeps_occupant() {
    let (seen, mut coordinator) = build(slot_params(both()));
    send(&seen, &mut coordinator, [load("up")]);
    let events = send(&seen, &mut coordinator, [load("sideways")]);
    assert!(refused(&events, "sideways"), "{events:?}");
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
    stale["source"]["stored"]["path"] = json!(copy);
    stale["source"]["stored"]["hash"] = json!(blake3::hash(b"another build").to_hex().as_str());
    let allow = vec![candidate("up", "count_up"), stale];
    let (seen, mut coordinator) = build(slot_params(allow));
    send(&seen, &mut coordinator, [load("up")]);
    let events = send(&seen, &mut coordinator, [load("down")]);
    assert!(refused(&events, "not the library"), "{events:?}");
    assert_eq!(status(&seen).loaded, 0);
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
    let events = send(&seen, &mut coordinator, [load("gain")]);
    assert!(refused(&events, "no longer matches"), "{events:?}");
    assert_eq!(status(&seen).loaded, 0);
}

/// A candidate whose constructor fails is rejected. The previous occupant is
/// restored on the rings it gave up.
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

/// A bad `initial` candidate leaves the slot empty and reports why. The rest
/// of the target keeps running.
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
    // Without a channel there are no events or status, so drop the watcher.
    config.systems.pop();
    let Err(error) = config.build(&table(&seen)) else {
        panic!("a slot without a channel is refused")
    };
    assert!(
        matches!(
            error,
            BuildError::Params { ref id, source: metor_fsw_3::ParamError::SlotChannel } if id == "mode"
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
    assert!(error.to_string().contains("metor build"), "{error}");
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
    json!({ "name": name, "ty": ty, "source": { "pack": pack_ref(libs) } })
}
