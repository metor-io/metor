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

use common::{Ping, fixture_lib};
use metor_fsw_3::coordinator::{
    BuildError, Coordinator, CoordinatorConfig, InputConfig, ParamError, PortRef, SystemConfig,
    SystemTable,
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
fn test_abi_error_reports_versions() {
    // SAFETY: the fixture is a valid pack; only the expected version is wrong.
    let Err(error) = (unsafe { DlPack::open_with(fixture_lib(), ABI_VERSION + 1) }) else {
        panic!("the versions differ")
    };
    assert!(matches!(
        error,
        PackError::AbiMismatch { found, expected }
            if found == ABI_VERSION && expected == ABI_VERSION + 1
    ));
}

#[test]
fn test_pack_transfers_record() {
    let (mut coordinator, seen, _pack) = built("echo", serde_json::Value::Null);
    coordinator.step(Timestamp(1));
    assert_eq!(*seen.borrow(), vec![1]);
    coordinator.step(Timestamp(2));
    assert_eq!(*seen.borrow(), vec![1, 2]);
    assert_eq!(coordinator.latched().count(), 0);
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
    assert_eq!(coordinator.latched().count(), 0);
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
        BuildError::Params { ref id, source: ParamError::UnknownKey(ref key) }
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
    assert_eq!(coordinator.latched().count(), 0);
    coordinator.step(Timestamp(2));
    assert_eq!(coordinator.latched().collect::<Vec<_>>(), vec!["middle"]);
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
    config.ring_depth = 2;
    config.systems[2].inputs[0].from = vec![PortRef::new("source", "output")];
    let mut coordinator = config.build(&table(&pack, &seen)).expect("builds");

    for cycle in 1..=32 {
        coordinator.step(Timestamp(cycle));
    }
    assert_eq!(*seen.borrow(), (1..=32).collect::<Vec<_>>());
    assert_eq!(coordinator.latched().collect::<Vec<_>>(), vec!["middle"]);
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
    use metor_fsw_3::coordinator::{BuildError, CoordinatorConfig, ParamError, SystemConfig};
    use metor_fsw_3::io::{Backing, Grant, ResourceConfig, SimKind};

    use super::*;
    use crate::gps;

    type Fixes = Rc<RefCell<usize>>;

    #[system]
    fn fixes(#[state] seen: &mut Fixes, fix: &mut Input<gps::Gps>) {
        *seen.borrow_mut() += fix.drain().count();
    }

    fn io_table(pack: &Arc<Pack>, fixes_seen: &Fixes) -> SystemTable {
        let mut table = SystemTable::new();
        metor_fsw_3::link::register_builtins(&mut table).unwrap();
        table.register_pack("echo", pack.clone()).unwrap();
        table
            .register("uart_script", gps::uart_script(), gps::UartScript::new)
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
    fn dylib_driver_shares_its_region_with_the_host() {
        let pack = open();
        let seen = Fixes::default();
        let mut c = sim_target().build(&io_table(&pack, &seen)).unwrap();
        for t in 1..6 {
            c.step(Timestamp(t));
        }
        assert_eq!(c.latched().count(), 0);
        assert!(*seen.borrow() > 0, "the dylib driver read a fix");
    }

    #[test]
    fn wasm_driver_reads_through_the_mirror() {
        let pack = Arc::new(Pack::Wasm(common::open_wasm()));
        let seen = Fixes::default();
        let mut c = sim_target().build(&io_table(&pack, &seen)).unwrap();
        for t in 1..6 {
            c.step(Timestamp(t));
        }
        let ended: Vec<_> = c.ended().collect();
        assert!(ended.is_empty(), "{ended:?}");
        assert!(*seen.borrow() > 0, "the guest's read bytes reached it");
    }

    #[test]
    fn guest_device_side_is_refused() {
        let pack = open();
        let config = CoordinatorConfig {
            systems: vec![SystemConfig {
                grants: vec![Grant {
                    param: "uart".into(),
                    resource: "line".into(),
                }],
                ..SystemConfig::new("model", "echo.uart_script")
            }],
            resources: vec![ResourceConfig {
                id: "line".into(),
                backing: Backing::Sim(SimKind::Uart),
            }],
            ..CoordinatorConfig::default()
        };
        let err = config.build(&io_table(&pack, &Fixes::default())).err();
        let refused = |e: &BuildError| {
            matches!(e, BuildError::Params { source: ParamError::GuestDeviceSide { system }, .. }
                if system == "model")
        };
        assert!(err.as_ref().is_some_and(refused), "{err:?}");
    }
}
