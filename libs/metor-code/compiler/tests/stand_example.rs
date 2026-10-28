//! Builds `examples/metor-code-stand` the way `metor code build` does, with
//! its frames imported from the Rust `stand_io` pack, and runs both packs
//! in one coordinator.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use metor_code::cli::{Build, Source, build};
use metor_fsw_3::coordinator::{
    Adapter, ChannelConfig, CoordinatorConfig, InputConfig, OutputConfig, RunState, SystemConfig,
    SystemTable,
};
use metor_fsw_3::dl::PackRef;
use metor_fsw_3::metor_proto_wkt::SequenceChannelState;
use metor_fsw_3::{ChannelName, ChannelStatus, Input, Pack, Timestamp, system};

const EXAMPLE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../examples/metor-code-stand");

/// Builds the `stand` pack into a temp dir once, and returns its module dir.
fn stand_module() -> &'static Path {
    static BUILT: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let (_, module) = BUILT.get_or_init(|| {
        let out = tempfile::tempdir().expect("a temp dir");
        let args = Build {
            source: Source {
                file: Path::new(EXAMPLE).join("stand.mc"),
                pack_dirs: Vec::new(),
            },
            name: None,
            out: Some(out.path().to_path_buf()),
            target: None,
            runtime: None,
            linker: "cc".into(),
        };
        if let Err(error) = build(&args) {
            panic!("{error}");
        }
        let module = out.path().join("stand");
        (out, module)
    });
    module
}

fn open(id: &str, module: &Path) -> Arc<Pack> {
    let reference = PackRef::dylib(id.into(), id.into(), module.join("_libs"));
    Arc::new(reference.open().expect("the pack opens"))
}

/// The last channel state the probe saw.
#[derive(Clone, Default)]
struct Last(std::rc::Rc<std::cell::Cell<Option<u32>>>);

#[system]
fn probe(#[state] last: &mut Last, status: &mut Input<ChannelStatus>) {
    if let Ok(Some(status)) = status.latest() {
        last.0.set(Some(status.state));
    }
}

/// Runs the stand with chamber params `plant` and returns the leak check's
/// final channel state.
fn run(plant: serde_json::Value, cycles: i64) -> Option<u32> {
    // Building first runs `metor pack dev` on stand_io, which refuses a
    // library this process has loaded.
    let stand = stand_module();
    let io_module = Path::new(EXAMPLE).join("systems/stand-io/.metor/stand_io");
    let mut table = SystemTable::new();
    table.register_pack("stand_io", open("stand_io", &io_module)).expect("registers");
    table.register_pack("stand", open("stand", stand)).expect("registers");
    let last = Last::default();
    let probe_state = last.clone();
    table.register("probe", probe(), move || probe_state.clone()).expect("registers");
    let channel = Adapter::Channel(ChannelConfig {
        name: ChannelName::try_from("leak_check".to_string()).expect("short"),
        start: RunState::Running,
    });
    let config = CoordinatorConfig {
        systems: vec![
            SystemConfig {
                params: plant,
                inputs: vec![
                    InputConfig::new("valve", "leak_check", "valve"),
                    InputConfig::new("heater", "heater", "heater"),
                ],
                ..SystemConfig::new("sim", "stand_io.chamber_sim")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("chamber", "sim", "chamber")],
                adapter: Some(channel),
                ..SystemConfig::new("leak_check", "stand.leak_check")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("chamber", "sim", "chamber")],
                ..SystemConfig::new("heater", "stand.heater")
            },
            SystemConfig {
                inputs: vec![InputConfig::new("status", "leak_check", "channel_status")],
                outputs: Vec::<OutputConfig>::new(),
                ..SystemConfig::new("probe", "probe")
            },
        ],
        ..Default::default()
    };
    let mut coordinator = config.build(&table).expect("the stand builds");
    for cycle in 0..cycles {
        coordinator.step(Timestamp(1_000_000 + cycle * 10_000));
    }
    last.0.get()
}

#[test]
fn test_stand_leak_check_completes() {
    let state = run(serde_json::json!({ "fill": 10.0 }), 32_000);
    assert_eq!(state, Some(SequenceChannelState::Completed.code()));
}

#[test]
fn test_stand_redline_aborts() {
    let state = run(serde_json::json!({ "fill": 4000.0 }), 100);
    assert_eq!(state, Some(SequenceChannelState::Aborted.code()));
    let state = run(serde_json::json!({ "fill": 10.0, "stuck": true }), 2_000);
    assert_eq!(state, Some(SequenceChannelState::Aborted.code()));
}

#[test]
fn test_stand_leak_fails() {
    let state = run(serde_json::json!({ "fill": 10.0, "leak": 0.05 }), 32_000);
    assert_eq!(state, Some(SequenceChannelState::Failed.code()));
}
