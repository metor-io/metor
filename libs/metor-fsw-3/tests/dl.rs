//! The host side against a real pack: build `echo-pack`, open it, run it.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::OnceLock;

use metor_fsw_3::cli::build::cargo_build;
use metor_fsw_3::coordinator::{
    BuildError, Coordinator, CoordinatorConfig, InputConfig, ParamError, PortRef, SystemConfig,
    SystemTable,
};
use metor_fsw_3::{ABI_VERSION, Input, Output, Pack, PackError, Record, Timestamp, system};
use serde::{Deserialize, Serialize};

/// The pack's record, redeclared here: the component id is the name's hash.
#[derive(Record, Serialize, Deserialize, Clone, Copy, Debug)]
#[record(max_len = 8)]
struct Ping {
    n: u32,
}

/// Counts up and publishes the count.
#[derive(Default)]
struct Source(u32);

#[system]
impl Source {
    fn execute(&mut self, output: &mut Output<Ping>) {
        self.0 += 1;
        let _ = output.write(&Ping { n: self.0 });
    }
}

/// Records every ping it is wired to.
struct Sink(Rc<RefCell<Vec<u32>>>);

#[system]
impl Sink {
    fn execute(&mut self, input: &mut Input<Ping>) {
        for ping in input.drain() {
            let Ok(ping) = ping else { continue };
            self.0.borrow_mut().push(ping.n);
        }
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

/// Opens the fixture, which is a pack built against this ABI.
fn open() -> Pack {
    // SAFETY: the fixture is a metor-fsw-3 pack this workspace just built.
    unsafe { Pack::open(fixture()) }.expect("the fixture opens")
}

/// `source -> <pack>.<ty> -> sink`, with the pack system's params.
fn pipeline(ty: &str, params: serde_json::Value) -> (CoordinatorConfig, Rc<RefCell<Vec<u32>>>) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig::new("source", "source"),
            SystemConfig {
                params,
                inputs: vec![InputConfig {
                    port: "input".into(),
                    from: vec![PortRef::new("source", "output")],
                }],
                ..SystemConfig::new("middle", format!("echo.{ty}"))
            },
            SystemConfig {
                inputs: vec![InputConfig {
                    port: "input".into(),
                    from: vec![PortRef::new("middle", "output")],
                }],
                ..SystemConfig::new("sink", "sink")
            },
        ],
        ..Default::default()
    };
    (config, seen)
}

fn table(pack: &Pack, seen: &Rc<RefCell<Vec<u32>>>) -> SystemTable {
    let mut table = SystemTable::new();
    table.register("source", Source::default);
    let sink = seen.clone();
    table.register("sink", move || Sink(sink.clone()));
    table.register_pack("echo", pack);
    table
}

fn built(ty: &str, params: serde_json::Value) -> (Coordinator, Rc<RefCell<Vec<u32>>>, Pack) {
    let pack = open();
    let (config, seen) = pipeline(ty, params);
    let coordinator = config
        .build(&table(&pack, &seen))
        .expect("the config builds");
    (coordinator, seen, pack)
}

#[test]
fn a_pack_reports_its_systems_with_their_ports_and_schemas() {
    let pack = open();
    let types: Vec<_> = pack.systems().map(|s| s.ty).collect();
    assert_eq!(types, vec!["echo", "boom", "gain"]);

    let echo = pack.systems().next().expect("one system");
    assert_eq!(echo.def.inputs[0].name, "input");
    assert_eq!(echo.def.inputs[0].record, Ping::NAME);
    assert_eq!(echo.def.inputs[0].id, Ping::ID);
    // The `log` output every fn system declares comes last.
    let outputs: Vec<_> = echo.def.outputs.iter().map(|p| p.name).collect();
    assert_eq!(outputs, vec!["output", "log"]);
    assert_eq!(echo.doc, "Copies the newest ping.");

    let gain = pack.systems().find(|s| s.ty == "gain").expect("gain");
    let schema = gain.params.as_deref().expect("gain takes params").get();
    assert!(schema.contains(r#""description":"Scales every ping.""#));
    assert!(pack.systems().next().expect("echo").params.is_none());
}

#[test]
fn a_library_that_is_no_pack_names_the_missing_export() {
    #[cfg(target_os = "macos")]
    let path = Path::new("/usr/lib/libSystem.B.dylib");
    #[cfg(not(target_os = "macos"))]
    let path = Path::new("libc.so.6");
    // SAFETY: a system library, loaded to check that it exports nothing of ours.
    let Err(error) = (unsafe { Pack::open(path) }) else {
        panic!("a system library is no pack")
    };
    assert!(matches!(
        error,
        PackError::MissingSymbol("metor_fsw_abi_version")
    ));
}

#[test]
fn a_version_mismatch_is_reported_with_both_numbers() {
    // SAFETY: the fixture is a pack; only the expected version is wrong.
    let Err(error) = (unsafe { Pack::open_with(fixture(), ABI_VERSION + 1) }) else {
        panic!("the versions differ")
    };
    assert!(matches!(
        error,
        PackError::AbiMismatch { found, expected }
            if found == ABI_VERSION && expected == ABI_VERSION + 1
    ));
}

#[test]
fn a_pack_system_moves_a_record_within_one_cycle() {
    let (mut coordinator, seen, _pack) = built("echo", serde_json::Value::Null);
    coordinator.step(Timestamp(1));
    assert_eq!(*seen.borrow(), vec![1]);
    coordinator.step(Timestamp(2));
    assert_eq!(*seen.borrow(), vec![1, 2]);
    assert_eq!(coordinator.latched().count(), 0);
}

#[test]
fn a_pack_systems_params_reach_it() {
    let (mut coordinator, seen, _pack) = built("gain", serde_json::json!({ "gain": 3.0 }));
    coordinator.step(Timestamp(1));
    assert_eq!(*seen.borrow(), vec![3]);
}

#[test]
fn a_bad_params_key_names_the_system() {
    let pack = open();
    let (config, seen) = pipeline("gain", serde_json::json!({ "gain": 1.0, "gian": 2.0 }));
    let Err(error) = config.build(&table(&pack, &seen)) else {
        panic!("a bad key is rejected")
    };
    assert!(matches!(
        error,
        BuildError::Params { ref id, source: ParamError::UnknownKey(ref key) }
            if id == "middle" && key == "gian"
    ));
}

#[test]
fn a_panicking_pack_system_latches_and_the_cycle_goes_on() {
    let pack = open();
    let (mut config, seen) = pipeline("boom", serde_json::Value::Null);
    // `boom` writes on its own, so it takes no input.
    config.systems[1].inputs.clear();
    let mut coordinator = config.build(&table(&pack, &seen)).expect("builds");

    coordinator.step(Timestamp(1));
    assert_eq!(coordinator.latched().count(), 0);
    coordinator.step(Timestamp(2));
    assert_eq!(coordinator.latched().collect::<Vec<_>>(), vec!["middle"]);
    coordinator.step(Timestamp(3));
    // The sink ran on every cycle, including the two after the panic.
    assert_eq!(*seen.borrow(), vec![1]);
    assert_eq!(coordinator.cycle(), 3);
}

#[test]
fn the_library_unloads_after_the_last_instance() {
    let (coordinator, _seen, pack) = built("echo", serde_json::Value::Null);
    let weak = Rc::downgrade(pack.library());
    drop(pack);
    assert!(weak.upgrade().is_some(), "an instance still holds it");
    drop(coordinator);
    assert!(weak.upgrade().is_none(), "the last instance freed it");
}
