//! Tests the host against `echo-pack` built as a wasm module.

mod common;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;

use common::{Ping, fixture_lib, fixture_wasm, open_wasm, panicked};
use metor_fsw_3::coordinator::{BuildError, Coordinator, CoordinatorConfig, ParamError};
use metor_fsw_3::metor_proto_wkt::{
    LogEvent, SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
};
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{
    Adapter, ChannelConfig, DefCx, DlPack, Frame, Input, InputConfig, Output, Pack, PortRef,
    RunState, SystemConfig, SystemTable, Timestamp, system,
};
use serde_json::{Value, json};

fn open_dylib() -> DlPack {
    // SAFETY: the fixture is a metor-fsw-3 pack this workspace just built.
    unsafe { DlPack::open(fixture_lib()) }.expect("the fixture opens")
}

fn value(value: impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("encodes")
}

#[test]
fn test_descriptor_matches_dylib() {
    let (wasm, dylib) = (open_wasm(), open_dylib());
    assert_eq!(value(wasm.descriptor()), value(dylib.descriptor()));
    let kinds: Vec<_> = wasm.descriptor().systems.iter().map(|s| s.kind).collect();
    assert!(kinds.contains(&metor_fsw_3::SystemKind::Async));
    assert!(kinds.contains(&metor_fsw_3::SystemKind::Sequence));
}

#[test]
fn test_def_matches_dylib() {
    let params = json!({ "gain": 2.0 });
    let cx = DefCx {
        id: "scaled",
        params: &params,
        ..DefCx::empty()
    };
    let wasm = open_wasm().def("gain", &cx).expect("a definition");
    assert_eq!(wasm, open_dylib().def("gain", &cx).expect("a definition"));
}

#[test]
fn test_def_of_unknown_type_is_refused() {
    let refused = open_wasm().def("nope", &DefCx::empty());
    assert!(matches!(refused, Err(metor_fsw_3::DefError::Pack { .. })));
}

/// Pins the wasm signatures of the exported ABI functions.
///
/// rustc passes a by-value `RawSlice` as an `i32` pointer. If that ever
/// changes, this test fails first.
#[test]
fn test_export_signatures() {
    let engine = wasmi::Engine::default();
    let module = wasmi::Module::new(&engine, fixture_wasm()).expect("compiles");
    let mut exports: Vec<String> = module
        .exports()
        .filter(|export| export.name().starts_with("metor_fsw_"))
        .map(|export| {
            let ty = export.ty().func().expect("a function");
            format!("{} {:?} -> {:?}", export.name(), ty.params(), ty.results())
        })
        .collect();
    exports.sort();
    assert_eq!(
        exports,
        [
            "metor_fsw_abi_version [] -> [I32]",
            "metor_fsw_alloc [I32, I32] -> [I32]",
            "metor_fsw_create [I32, I32, I32, I32, I32, I32, I32] -> [I32]",
            "metor_fsw_def [I32, I32, I32, I32, I32] -> [I32]",
            "metor_fsw_destroy [I32] -> []",
            "metor_fsw_execute [I32, I64] -> [I32]",
            "metor_fsw_free [I32, I32, I32] -> []",
            "metor_fsw_pack_def [I32, I32, I32] -> [I32]",
        ]
    );
    let imports: Vec<_> = module
        .imports()
        .map(|import| format!("{}.{}", import.module(), import.name()))
        .collect();
    assert_eq!(imports, ["env.memory"]);
}

/// The fixture pack's `Sample` frame, declared again here.
///
/// The host matches records by a hash of the name, so this copy is the same
/// frame as the pack's.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug)]
#[frame(name = "sample")]
#[repr(C)]
struct Sample {
    #[frame(timestamp)]
    timestamp: Timestamp,
    value: f64,
}

/// What the host's test systems have seen, plus the commands still to send.
#[derive(Default)]
struct Seen {
    pings: Vec<u32>,
    lines: Vec<LogEvent>,
    events: Vec<SequenceEventKind>,
    queue: VecDeque<SequenceCommandKind>,
    /// How many of the source's writes its ring rejected.
    refused: u32,
    /// How many cycles the sink has run. The sink runs after the system under
    /// test.
    after: u32,
}

type Shared = Rc<RefCell<Seen>>;

struct Source(u32, Shared);

/// Publishes a count that goes up by one each cycle, and counts the writes
/// its ring rejects.
#[system]
fn source(#[state] s: &mut Source, output: &mut Output<Ping>) {
    s.0 += 1;
    if output.write(&Ping { n: s.0 }).is_err() {
        s.1.borrow_mut().refused += 1;
    }
}

/// Writes one sample per cycle, timestamped at twice the cycle plus an
/// offset.
struct Sampler(i64, i64);

#[system]
fn sampler(#[state] s: &mut Sampler, sample: &mut Output<Sample>) {
    s.0 += 1;
    let t = 2 * s.0 + s.1;
    let _ = sample.write(&Sample {
        timestamp: Timestamp(t),
        value: t as f64,
    });
}

struct Sink(Shared);

/// Records every ping and log line it receives, and counts its cycles.
#[system]
fn sink(#[state] s: &mut Sink, pings: &mut Input<Ping>, lines: &mut Input<LogEvent>) {
    let mut seen = s.0.borrow_mut();
    seen.after += 1;
    let pings: Vec<u32> = pings.drain().filter_map(Result::ok).map(|p| p.n).collect();
    seen.pings.extend(pings);
    seen.lines.extend(lines.drain().filter_map(Result::ok));
}

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

struct Watch(Shared);

#[system]
fn watch(#[state] w: &mut Watch, events: &mut Input<SequenceChannelEvent>) {
    let events = events.drain().filter_map(Result::ok);
    w.0.borrow_mut()
        .events
        .extend(events.map(|event| event.kind));
}

fn table(pack: &Arc<Pack>, seen: &Shared) -> SystemTable {
    let mut table = SystemTable::new();
    let s = seen.clone();
    table
        .register("source", source(), move || Source(0, s.clone()))
        .expect("valid records");
    table
        .register("older", sampler(), || Sampler(0, 0))
        .expect("valid records");
    table
        .register("newer", sampler(), || Sampler(0, 1))
        .expect("valid records");
    let s = seen.clone();
    table
        .register("sink", sink(), move || Sink(s.clone()))
        .expect("valid records");
    let s = seen.clone();
    table
        .register("commander", commander(), move || Commander(s.clone()))
        .expect("valid records");
    let s = seen.clone();
    table
        .register("watch", watch(), move || Watch(s.clone()))
        .expect("valid records");
    table
        .register_pack("echo", pack.clone())
        .expect("valid records");
    table
}

fn wasm_pack() -> Arc<Pack> {
    Arc::new(Pack::Wasm(open_wasm()))
}

fn dylib_pack() -> Arc<Pack> {
    Arc::new(Pack::Dl(open_dylib()))
}

/// Builds `source -> middle (echo.<ty>) -> sink`. The sink also reads
/// `middle`'s log.
fn pipeline(ty: &str) -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("source", "source"),
            SystemConfig {
                inputs: vec![InputConfig::new("input", "source", "output")],
                ..SystemConfig::new("middle", format!("echo.{ty}"))
            },
            SystemConfig {
                inputs: vec![
                    InputConfig::new("pings", "middle", "output"),
                    InputConfig::new("lines", "middle", "log"),
                ],
                ..SystemConfig::new("sink", "sink")
            },
        ],
        ..Default::default()
    }
}

fn build(config: CoordinatorConfig, pack: &Arc<Pack>) -> (Coordinator, Shared) {
    let seen = Shared::default();
    let coordinator = config
        .build(&table(pack, &seen))
        .expect("the config builds");
    (coordinator, seen)
}

fn run(coordinator: &mut Coordinator, cycles: usize) {
    for _ in 0..cycles {
        let next = Timestamp(coordinator.cycle() as i64 + 1);
        coordinator.step(next);
    }
}

fn pings(seen: &Shared) -> Vec<u32> {
    std::mem::take(&mut seen.borrow_mut().pings)
}

/// Returns the `kind` field of every log line the sink saw.
fn kinds(seen: &Shared) -> Vec<String> {
    let seen = seen.borrow();
    let fields = seen.lines.iter().flat_map(|line| &line.fields);
    fields
        .filter(|(name, _)| name == "kind")
        .map(|(_, value)| value.to_string())
        .collect()
}

#[test]
fn test_echo_delivers_each_cycle() {
    let (mut coordinator, seen) = build(pipeline("echo"), &wasm_pack());
    for cycle in 1..=32 {
        run(&mut coordinator, 1);
        assert_eq!(pings(&seen), [cycle], "cycle {cycle}");
    }
    assert_eq!(panicked(&coordinator).len(), 0);
}

/// Builds `older, newer -> middle (echo.meter) -> sink`.
///
/// The meter pings the newest sample from either of its two producers.
fn fan_in() -> CoordinatorConfig {
    let producers = vec![
        PortRef::new("older", "sample"),
        PortRef::new("newer", "sample"),
    ];
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("older", "older"),
            SystemConfig::new("newer", "newer"),
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "sample".into(),
                    from: producers,
                }],
                ..SystemConfig::new("middle", "echo.meter")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("pings", "middle", "output")],
                ..SystemConfig::new("sink", "sink")
            },
        ],
        ..Default::default()
    }
}

#[test]
fn test_fan_in_takes_newest() {
    let (mut coordinator, seen) = build(fan_in(), &wasm_pack());
    run(&mut coordinator, 8);
    let newest: Vec<u32> = (1..=8).map(|cycle| 2 * cycle + 1).collect();
    assert_eq!(pings(&seen), newest);
}

#[test]
fn test_module_matches_dylib() {
    let streams = |pack: Arc<Pack>| {
        let (mut echo, echoed) = build(pipeline("echo"), &pack);
        let (mut meter, metered) = build(fan_in(), &pack);
        run(&mut echo, 32);
        run(&mut meter, 32);
        (pings(&echoed), pings(&metered))
    };
    assert_eq!(streams(wasm_pack()), streams(dylib_pack()));
}

#[test]
fn test_async_refused() {
    let pack = wasm_pack();
    let Err(error) = pipeline("relay").build(&table(&pack, &Shared::default())) else {
        panic!("a module has no threads")
    };
    assert!(
        matches!(
            error,
            BuildError::Params {
                source: ParamError::ThreadsUnsupported { .. },
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn test_panic_reports_trap_and_message() {
    let mut config = pipeline("boom");
    config.systems[1].inputs.clear();
    let (mut coordinator, seen) = build(config, &wasm_pack());
    run(&mut coordinator, 3);
    assert_eq!(panicked(&coordinator), ["middle"]);
    assert_eq!(pings(&seen), [1]);
    assert_eq!(kinds(&seen), ["panic", "trap"]);
    let lines = &seen.borrow().lines;
    assert!(
        lines[0].message.contains("boom on cycle 2"),
        "{}",
        lines[0].message
    );
    assert_eq!(lines[1].message, "unreachable");
}

#[test]
fn test_spin_fails_and_frees_its_producer() {
    let mut config = pipeline("spin");
    config.ring_depth = core::num::NonZeroUsize::new(2).expect("nonzero");
    config.systems[2].inputs[0].from = vec![PortRef::new("source", "output")];
    let (mut coordinator, seen) = build(config, &wasm_pack());
    run(&mut coordinator, 1);
    assert_eq!(kinds(&seen), ["fuel"]);
    run(&mut coordinator, 32);
    assert_eq!(seen.borrow().refused, 0);
    assert_eq!(seen.borrow().after, 33);
    assert_eq!(pings(&seen), (1..=33).collect::<Vec<_>>());
}

#[test]
fn test_logging_guest_runs() {
    let mut config = pipeline("tick");
    config.systems[1].inputs.clear();
    let (mut coordinator, seen) = build(config, &wasm_pack());
    run(&mut coordinator, 100);
    assert_eq!(panicked(&coordinator).len(), 0);
    assert_eq!(pings(&seen), (1..=100).collect::<Vec<_>>());
    let ticks = seen
        .borrow()
        .lines
        .iter()
        .filter(|l| l.message == "tick")
        .count();
    assert_eq!(ticks, 100);
}

/// Builds `cmds -> source -> mode (echo.count_up behind a channel) -> watch,
/// sink`.
fn channel() -> CoordinatorConfig {
    CoordinatorConfig {
        systems: vec![
            SystemConfig::new("cmds", "commander"),
            SystemConfig::new("source", "source"),
            SystemConfig {
                adapter: Some(Adapter::Channel(ChannelConfig {
                    name: metor_fsw_3::ChannelName::try_from("mode".to_string())
                        .expect("a short name"),
                    start: RunState::Stopped,
                })),
                inputs: vec![
                    InputConfig::new("input", "source", "output"),
                    InputConfig::new("commands", "cmds", "commands"),
                ],
                ..SystemConfig::new("mode", "echo.count_up")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("events", "mode", "events")],
                ..SystemConfig::new("watch", "watch")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("pings", "mode", "output")],
                ..SystemConfig::new("sink", "sink")
            },
        ],
        ..Default::default()
    }
}

/// Sends `command` and runs `cycles` cycles. Returns the events they
/// produced.
fn send(
    coordinator: &mut Coordinator,
    seen: &Shared,
    command: SequenceCommandKind,
    cycles: usize,
) -> Vec<SequenceEventKind> {
    seen.borrow_mut().queue.push_back(command);
    run(coordinator, cycles);
    std::mem::take(&mut seen.borrow_mut().events)
}

#[test]
fn test_sequence_completes_aborts_and_resets() {
    for pack in [wasm_pack(), dylib_pack()] {
        sequence_completes_aborts_and_resets(&pack);
    }
}

fn sequence_completes_aborts_and_resets(pack: &Arc<Pack>) {
    let (mut coordinator, seen) = build(channel(), pack);
    let events = send(&mut coordinator, &seen, SequenceCommandKind::Start, 4);
    let completed = [SequenceEventKind::Started, SequenceEventKind::Completed];
    assert_eq!(events, completed);
    assert_eq!(pings(&seen), [1, 2, 3]);

    send(&mut coordinator, &seen, SequenceCommandKind::Reset, 1);
    send(&mut coordinator, &seen, SequenceCommandKind::Start, 1);
    // The sequence sees the abort on the same cycle it is sent, and ends on
    // that cycle.
    let events = send(&mut coordinator, &seen, SequenceCommandKind::Abort, 1);
    assert!(events.contains(&SequenceEventKind::Aborted), "{events:?}");
    assert_eq!(pings(&seen), [1]);

    send(&mut coordinator, &seen, SequenceCommandKind::Reset, 1);
    let events = send(&mut coordinator, &seen, SequenceCommandKind::Start, 4);
    assert_eq!(events, completed);
    assert_eq!(pings(&seen), [1, 2, 3]);
    assert_eq!(panicked(&coordinator).len(), 0);
}
