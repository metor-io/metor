//! Tests the host against a real pack: builds `echo-pack`, opens it, and runs
//! its systems.

mod common;

#[path = "fixtures/echo-pack/src/gps.rs"]
#[allow(dead_code)]
mod gps;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use common::{Ping, fixture_lib, panicked};
use metor_fsw_3::coordinator::{
    BuildError, Coordinator, CoordinatorConfig, InputConfig, MakeError, ParamError, PortRef,
    SystemConfig, SystemTable,
};
use metor_fsw_3::{ABI_VERSION, DlPack, Input, Output, Pack, PackError, Record, Timestamp, system};

/// Counts up and publishes the count.
#[derive(Default)]
struct Source(u32);

#[system]
fn source(#[state] s: &mut Source, output: &mut Output<Ping>) {
    s.0 += 1;
    let _ = output.write(&Ping { n: s.0 });
}

/// Records every ping it receives.
struct Sink(Rc<RefCell<Vec<u32>>>);

#[system]
fn sink(#[state] s: &mut Sink, input: &mut Input<Ping>) {
    for ping in input.drain() {
        let Ok(ping) = ping else { continue };
        s.0.borrow_mut().push(ping.n);
    }
}

/// Opens the fixture pack.
fn open() -> Arc<Pack> {
    // SAFETY: the fixture is a metor-fsw-3 pack this workspace just built.
    let pack = unsafe { DlPack::open(fixture_lib()) }.expect("the fixture opens");
    Arc::new(Pack::Dl(pack))
}

/// Returns the fixture's loaded library. Panics for a wasm pack, which has
/// none.
fn library(pack: &Pack) -> &Arc<libloading::Library> {
    match pack {
        Pack::Dl(pack) => pack.library(),
        Pack::Wasm(_) => panic!("a module has no library"),
    }
}

#[test]
fn test_pack_builder_panic_returns_error() {
    const CHILD: &str = "METOR_TEST_PACK_BUILDER_PANIC";
    if std::env::var_os(CHILD).is_some() {
        // SAFETY: the fixture uses this workspace's ABI, including panic containment.
        assert!(matches!(
            unsafe { DlPack::open(fixture_lib()) },
            Err(PackError::DescriptorStatus(3))
        ));
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "test_pack_builder_panic_returns_error",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Builds the pipeline `source -> echo.<ty> -> sink`, passing `params` to the
/// pack system.
fn pipeline(ty: &str, params: serde_json::Value) -> (CoordinatorConfig, Rc<RefCell<Vec<u32>>>) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("source", "source"),
            SystemConfig {
                params,
                inputs: vec![InputConfig::new("input", "source", "output")],
                ..SystemConfig::new("middle", format!("echo.{ty}"))
            },
            SystemConfig {
                inputs: vec![InputConfig::new("input", "middle", "output")],
                ..SystemConfig::new("sink", "sink")
            },
        ],
        ..Default::default()
    };
    (config, seen)
}

fn table(pack: &Arc<Pack>, seen: &Rc<RefCell<Vec<u32>>>) -> SystemTable {
    let mut table = SystemTable::new();
    table
        .register("source", source(), Source::default)
        .expect("valid records");
    let seen = seen.clone();
    table
        .register("sink", sink(), move || Sink(seen.clone()))
        .expect("valid records");
    table
        .register_pack("echo", pack.clone())
        .expect("valid records");
    table
}

fn built(ty: &str, params: serde_json::Value) -> (Coordinator, Rc<RefCell<Vec<u32>>>, Arc<Pack>) {
    let pack = open();
    let (config, seen) = pipeline(ty, params);
    let coordinator = config
        .build(&table(&pack, &seen))
        .expect("the config builds");
    (coordinator, seen, pack)
}

#[test]
fn test_pack_reports_system_metadata() {
    let pack = open();
    let types: Vec<_> = pack
        .descriptor()
        .systems
        .iter()
        .map(|s| s.ty.as_str())
        .collect();
    assert_eq!(
        types,
        vec![
            "sampler",
            "meter",
            "echo",
            "dice",
            "relay",
            "tap",
            "spin",
            "tick",
            "boom",
            "gain",
            "fail_input",
            "retained",
            "gps",
            "uart_script",
            "chatter",
            "reader",
            "grower",
            "until_fix",
            "count_up",
            "count_down",
            "count_by"
        ]
    );

    let gps = pack
        .descriptor()
        .systems
        .iter()
        .find(|s| s.ty == "gps")
        .expect("gps");
    let grant = &gps.def.grants[..];
    assert_eq!(grant.len(), 1);
    assert_eq!((&*grant[0].param, &*grant[0].interface), ("uart", "uart"));

    let tap = pack
        .descriptor()
        .systems
        .iter()
        .find(|s| s.ty == "tap")
        .expect("tap");
    assert!(tap.takes_inputs && !tap.takes_outputs);
    assert!(tap.def.inputs.is_empty());

    let echo = pack
        .descriptor()
        .systems
        .iter()
        .find(|s| s.ty == "echo")
        .expect("echo");
    assert_eq!(echo.def.inputs[0].name, "input");
    assert_eq!(echo.def.inputs[0].record, Ping::NAME);
    assert_eq!(echo.def.inputs[0].id, Ping::ID);
    // Every fn system has a `log` output, and it comes last.
    let outputs: Vec<_> = echo.def.outputs.iter().map(|p| p.name.as_ref()).collect();
    assert_eq!(outputs, vec!["output", "log"]);
    assert_eq!(echo.doc, "Copies the newest ping.");

    let gain = pack
        .descriptor()
        .systems
        .iter()
        .find(|s| s.ty == "gain")
        .expect("gain");
    let schema = gain.params.as_deref().expect("gain takes params").get();
    assert!(schema.contains(r#""description":"Scales every ping.""#));
    assert!(echo.params.is_none());
}

/// The host wires two outputs into a pack system that does not declare those
/// inputs. The pack computes their definitions from the wiring.
#[test]
fn test_pack_configures_inputs() {
    let pack = open();
    let seen = Rc::new(RefCell::new(Vec::new()));
    let taps = ["source", "other"].map(|id| InputConfig {
        port: format!("{id}.output"),
        from: vec![PortRef::new(id, "output")],
    });
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("source", "source"),
            SystemConfig::new("other", "source"),
            SystemConfig {
                inputs: taps.to_vec(),
                ..SystemConfig::new("tap", "echo.tap")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("input", "tap", "output")],
                ..SystemConfig::new("sink", "sink")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config
        .build(&table(&pack, &seen))
        .expect("the config builds");
    coordinator.step(Timestamp(1));
    // One ping from each source, summed by the pack system.
    assert_eq!(*seen.borrow(), vec![2]);
}

#[test]
fn test_missing_export_error() {
    #[cfg(target_os = "macos")]
    let path = Path::new("/usr/lib/libSystem.B.dylib");
    #[cfg(not(target_os = "macos"))]
    let path = Path::new("libc.so.6");
    // SAFETY: this is a system library. It is only loaded to check that it
    // does not export the pack symbols.
    let Err(error) = (unsafe { DlPack::open(path) }) else {
        panic!("a system library is no pack")
    };
    assert!(matches!(
        error,
        PackError::MissingSymbol("metor_fsw_abi_version")
    ));
}

#[test]
fn test_pack_transfers_record() {
    let (mut coordinator, seen, _pack) = built("echo", serde_json::Value::Null);
    coordinator.step(Timestamp(1));
    assert_eq!(*seen.borrow(), vec![1]);
    coordinator.step(Timestamp(2));
    assert_eq!(*seen.borrow(), vec![1, 2]);
    assert_eq!(panicked(&coordinator).len(), 0);
}

/// A pack system's `Rng` draws from the host's target seed.
#[test]
fn test_pack_rng_draws_from_the_target_seed() {
    let pack = open();
    let (mut config, seen) = pipeline("dice", serde_json::Value::Null);
    config.seed = 7;
    let mut coordinator = config.build(&table(&pack, &seen)).expect("builds");
    coordinator.step(Timestamp(1));
    coordinator.step(Timestamp(2));
    let mut rng = metor_fsw_3::Rng::seeded(7, "middle.rng");
    let want: Vec<u32> = (0..2).map(|_| rng.next_u64() as u32).collect();
    assert_eq!(*seen.borrow(), want);
}

#[test]
fn test_async_pack_transfers_record() {
    let (mut coordinator, seen, _pack) = built("relay", serde_json::Value::Null);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut cycle = 0;
    while seen.borrow().len() < 2 {
        assert!(
            std::time::Instant::now() < deadline,
            "the pack never relayed"
        );
        cycle += 1;
        coordinator.step(Timestamp(cycle));
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(panicked(&coordinator).len(), 0);
    assert_eq!(&seen.borrow()[..2], &[1, 2]);
}

#[test]
fn test_pack_receives_params() {
    let (mut coordinator, seen, _pack) = built("gain", serde_json::json!({ "gain": 3.0 }));
    coordinator.step(Timestamp(1));
    assert_eq!(*seen.borrow(), vec![3]);
}

#[test]
fn test_param_error_identifies_system() {
    let pack = open();
    let (config, seen) = pipeline("gain", serde_json::json!({ "gain": 1.0, "gian": 2.0 }));
    let Err(error) = config.build(&table(&pack, &seen)) else {
        panic!("a bad key is rejected")
    };
    assert!(matches!(
        error,
        BuildError::Make { ref id, source: MakeError::Params(ParamError::UnknownKey(ref key)) }
            if id == "middle" && key == "gian"
    ));
}

#[test]
fn test_pack_panic_preserves_cycle() {
    let pack = open();
    let (mut config, seen) = pipeline("boom", serde_json::Value::Null);
    // `boom` writes on its own, so it takes no input.
    config.systems[1].inputs.clear();
    let mut coordinator = config.build(&table(&pack, &seen)).expect("builds");

    coordinator.step(Timestamp(1));
    assert_eq!(panicked(&coordinator).len(), 0);
    coordinator.step(Timestamp(2));
    assert_eq!(panicked(&coordinator), vec!["middle"]);
    coordinator.step(Timestamp(3));
    // The coordinator kept cycling after the panic. The sink only got the
    // ping `boom` wrote before it failed.
    assert_eq!(*seen.borrow(), vec![1]);
    assert_eq!(coordinator.cycle(), 3);
}

#[test]
fn test_library_survives_instance_drop() {
    let (coordinator, _seen, pack) = built("echo", serde_json::Value::Null);
    let weak = Arc::downgrade(library(&pack));
    drop(pack);
    assert!(weak.upgrade().is_some(), "an instance still holds it");
    drop(coordinator);
    assert!(weak.upgrade().is_some(), "guest code remains resident");
    let reopened = open();
    assert!(weak.ptr_eq(&Arc::downgrade(library(&reopened))));
}

#[test]
fn test_failed_consumer_isolation() {
    let pack = open();
    let (mut config, seen) = pipeline("fail_input", serde_json::Value::Null);
    config.ring_depth = core::num::NonZeroUsize::new(2).expect("nonzero");
    config.systems[2].inputs[0].from = vec![PortRef::new("source", "output")];
    let mut coordinator = config.build(&table(&pack, &seen)).expect("builds");

    for cycle in 1..=32 {
        coordinator.step(Timestamp(cycle));
    }
    assert_eq!(*seen.borrow(), (1..=32).collect::<Vec<_>>());
    assert_eq!(panicked(&coordinator), vec!["middle"]);
}

#[test]
fn test_guest_ports_survive_pack_drop() {
    let (mut coordinator, _, pack) = built("retained", serde_json::Value::Null);
    // SAFETY: the fixture exports a function with this signature, and its
    // code stays loaded.
    let release = unsafe {
        *library(&pack)
            .get::<unsafe extern "C" fn() -> u32>(b"echo_pack_release_retained")
            .expect("fixture export")
    };
    coordinator.step(Timestamp(1));
    drop(coordinator);
    drop(pack);

    // SAFETY: called on the thread that bound the retained fixture ports.
    assert_eq!(unsafe { release() }, 1);
    // SAFETY: as above. A second call checks that both TLS handles were
    // released.
    assert_eq!(unsafe { release() }, 0);
}

mod io {
    use metor_fsw_3::coordinator::{CoordinatorConfig, SystemConfig};

    use super::*;
    use crate::gps;

    type Fixes = Rc<RefCell<usize>>;

    #[system]
    fn fixes(#[state] seen: &mut Fixes, fix: &mut Input<gps::Gps>) {
        *seen.borrow_mut() += fix.drain().count();
    }

    fn io_table(pack: &Arc<Pack>, fixes_seen: &Fixes) -> SystemTable {
        let mut table = SystemTable::new();
        metor_fsw_3::builtins::register_builtins(&mut table).unwrap();
        table.register_pack("echo", pack.clone()).unwrap();
        table
            .register("uart_script", gps::uart_script(), gps::UartScript::new)
            .unwrap();
        table
            .register("gps", gps::gps(), gps::GpsState::new)
            .unwrap();
        let seen = fixes_seen.clone();
        table
            .register("fixes", fixes(), move || seen.clone())
            .unwrap();
        table
    }

    /// `tests/golden/sim.py`'s target: echo-pack's `gps` driver on a sim
    /// serial port, served by a host model.
    fn sim_target() -> CoordinatorConfig {
        let mut config: CoordinatorConfig =
            serde_json::from_str(include_str!("golden/sim.json")).unwrap();
        config.systems[2].ty = "uart_script".into();
        config.systems.push(SystemConfig {
            inputs: vec![InputConfig::new("fix", "gps", "fix")],
            ..SystemConfig::new("fixes", "fixes")
        });
        config
    }

    #[test]
    fn dylib_driver_reads_the_wire() {
        let pack = open();
        let seen = Fixes::default();
        let mut c = sim_target().build(&io_table(&pack, &seen)).unwrap();
        for t in 1..6 {
            c.step(Timestamp(t));
        }
        assert_eq!(panicked(&c).len(), 0);
        assert!(*seen.borrow() > 0, "the dylib driver read a fix");
    }

    #[test]
    fn wasm_driver_reads_the_wire() {
        let pack = Arc::new(Pack::Wasm(common::open_wasm()));
        let seen = Fixes::default();
        let mut c = sim_target().build(&io_table(&pack, &seen)).unwrap();
        for t in 1..6 {
            c.step(Timestamp(t));
        }
        let ended: Vec<_> = c.systems().filter(|(_, end)| end.is_some()).collect();
        assert!(ended.is_empty(), "{ended:?}");
        assert!(*seen.borrow() > 0, "the guest's read bytes reached it");
    }

    fn model_serves_a_host_driver(pack: Arc<Pack>) {
        let seen = Fixes::default();
        let mut config = sim_target();
        config.systems[0].ty = "gps".into();
        config.systems[2].ty = "echo.uart_script".into();
        let mut c = config.build(&io_table(&pack, &seen)).unwrap();
        for t in 1..6 {
            c.step(Timestamp(t));
        }
        let ended: Vec<_> = c.systems().filter(|(_, end)| end.is_some()).collect();
        assert!(ended.is_empty(), "{ended:?}");
        assert!(*seen.borrow() > 0, "the guest model wrote the driver's fix");
    }

    #[test]
    fn dylib_model_serves_a_host_driver() {
        model_serves_a_host_driver(open());
    }

    #[test]
    fn wasm_model_serves_a_host_driver() {
        model_serves_a_host_driver(Arc::new(Pack::Wasm(common::open_wasm())));
    }
}

/// A guest's `Reset` waits for its ops in flight, as a host system's does.
mod quiesce {
    use metor_fsw_3::coordinator::{Adapter, ChannelConfig, CoordinatorConfig, RunState};
    use metor_fsw_3::io::ResourceStatus;
    use metor_fsw_3::metor_proto_wkt::{
        SequenceChannelEvent, SequenceCommand, SequenceCommandKind, SequenceEventKind,
    };
    use metor_fsw_3::{ChannelState, ChannelStatus};

    use super::*;
    use crate::gps;

    type Log<T> = Rc<RefCell<Vec<(i64, T)>>>;

    #[system]
    fn states(#[state] seen: &mut Log<u32>, now: Timestamp, st: &mut Input<ChannelStatus>) {
        let got = st.drain().flatten().map(|s| (now.0, s.state));
        seen.borrow_mut().extend(got);
    }

    #[system]
    fn statuses(#[state] seen: &mut Log<u64>, now: Timestamp, st: &mut Input<ResourceStatus>) {
        let got = st.drain().flatten().map(|s| (now.0, s.submitted));
        seen.borrow_mut().extend(got);
    }

    /// Keeps each failure's reason.
    #[system]
    fn failures(
        #[state] seen: &mut Rc<RefCell<Vec<String>>>,
        events: &mut Input<SequenceChannelEvent>,
    ) {
        for event in events.drain().flatten() {
            if let SequenceEventKind::Failed { reason } = event.kind {
                seen.borrow_mut().push(reason);
            }
        }
    }

    #[system]
    fn cmds(
        #[state] queue: &mut Rc<RefCell<Vec<SequenceCommandKind>>>,
        commands: &mut Output<SequenceCommand>,
    ) {
        for command in queue.borrow_mut().drain(..) {
            let channel = "gps".into();
            let _ = commands.write(&SequenceCommand { channel, command });
        }
    }

    #[derive(Default)]
    struct Seen {
        failures: Rc<RefCell<Vec<String>>>,
        states: Log<u32>,
        submitted: Log<u64>,
        queue: Rc<RefCell<Vec<SequenceCommandKind>>>,
    }

    fn table(pack: &Arc<Pack>, seen: &Seen) -> SystemTable {
        let mut table = SystemTable::new();
        metor_fsw_3::builtins::register_builtins(&mut table).unwrap();
        table.register_pack("echo", pack.clone()).unwrap();
        table
            .register("uart_script", gps::uart_script(), gps::UartScript::new)
            .unwrap();
        let s = seen.states.clone();
        table
            .register("states", states(), move || s.clone())
            .unwrap();
        let s = seen.submitted.clone();
        table
            .register("statuses", statuses(), move || s.clone())
            .unwrap();
        let s = seen.queue.clone();
        table.register("cmds", cmds(), move || s.clone()).unwrap();
        let s = seen.failures.clone();
        table
            .register("failures", failures(), move || s.clone())
            .unwrap();
        table
    }

    /// The sim target with `gps` behind channel `gps` and a silent model, so
    /// its read stays in flight.
    fn target() -> CoordinatorConfig {
        let mut config: CoordinatorConfig =
            serde_json::from_str(include_str!("golden/sim.json")).unwrap();
        config.systems[2].ty = "uart_script".into();
        config.systems[2].params = serde_json::json!({ "script": [] });
        let gps = &mut config.systems[0];
        gps.adapter = Some(Adapter::Channel(ChannelConfig {
            name: metor_fsw_3::ChannelName::try_from("gps".to_string()).expect("a short name"),
            start: RunState::Running,
        }));
        gps.inputs
            .push(InputConfig::new("commands", "cmds", "commands"));
        config.systems.insert(0, SystemConfig::new("cmds", "cmds"));
        config.systems.extend([
            SystemConfig {
                inputs: vec![InputConfig::new("st", "gps", "channel_status")],
                ..SystemConfig::new("states", "states")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("st", "io_complete", "io_status")],
                ..SystemConfig::new("statuses", "statuses")
            },
        ]);
        config
    }

    fn reset_waits_for_the_cancel(pack: Arc<Pack>) {
        let seen = Seen::default();
        let mut c = target().build(&table(&pack, &seen)).unwrap();
        let mut step = |from, to| (from..to).for_each(|t| c.step(Timestamp(t)));
        step(1, 4);
        seen.queue
            .borrow_mut()
            .extend([SequenceCommandKind::Stop, SequenceCommandKind::Reset]);
        step(4, 6);
        let quiescing = ChannelState::Quiescing as u32;
        let states = seen.states.borrow().clone();
        let waited = states.iter().find(|(_, s)| *s == quiescing);
        let (t, _) = *waited.unwrap_or_else(|| panic!("no wait: {states:?}"));
        let after = states.iter().find(|(at, _)| *at > t);
        let stopped = (t + 1, ChannelState::Stopped as u32);
        assert_eq!(
            after,
            Some(&stopped),
            "remade once the cancel landed: {states:?}"
        );
        let before = seen.submitted.borrow().last().map_or(0, |s| s.1);
        seen.queue.borrow_mut().push(SequenceCommandKind::Start);
        step(6, 10);
        let after = seen.submitted.borrow().last().map_or(0, |s| s.1);
        assert!(after > before, "the remade system submits again");
        assert_eq!(panicked(&c).len(), 0);
    }

    /// A remade instance whose ctor asks for more than the region holds
    /// fails, and the channel reports it. Only a dylib keeps the state that
    /// makes its ctor grow: a wasm guest starts fresh on every make.
    #[test]
    fn dylib_remake_past_the_region_fails() {
        let pack = open();
        let seen = Seen::default();
        let mut config = target();
        let at = config.systems.iter().position(|s| s.id == "gps").unwrap();
        config.systems[at].ty = "echo.grower".into();
        config.systems.push(SystemConfig {
            inputs: vec![InputConfig::new("events", "gps", "events")],
            ..SystemConfig::new("failures", "failures")
        });
        let mut c = config.build(&table(&pack, &seen)).unwrap();
        c.step(Timestamp(1));
        seen.queue
            .borrow_mut()
            .extend([SequenceCommandKind::Stop, SequenceCommandKind::Reset]);
        (2..5).for_each(|t| c.step(Timestamp(t)));
        let last = seen.states.borrow().last().map(|s| s.1);
        assert_eq!(last, Some(ChannelState::Failed as u32));
        let failures = seen.failures.borrow();
        assert!(
            failures.iter().any(|r| r.contains("IO memory")),
            "{failures:?}"
        );
    }

    #[test]
    fn dylib_reset_waits_for_the_cancel() {
        reset_waits_for_the_cancel(open());
    }

    #[test]
    fn wasm_reset_waits_for_the_cancel() {
        reset_waits_for_the_cancel(Arc::new(Pack::Wasm(common::open_wasm())));
    }
}
