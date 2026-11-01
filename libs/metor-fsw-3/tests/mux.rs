//! Tests `fsw.mux` from a Python target file to a running coordinator.

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use common::pair_source;
use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::cli::module::render;
use metor_fsw_3::coordinator::{CoordinatorConfig, SystemTable};
use metor_fsw_3::deploy::DeploymentConfig;
use metor_fsw_3::dl::PackRef;
use metor_fsw_3::pack::def::PackDef;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{ABI_VERSION, Frame, Input, Timestamp, system};

/// The split's output: `pair.a`.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "half")]
#[repr(C)]
struct Half {
    #[frame(timestamp)]
    timestamp: Timestamp,
    a: f64,
}

/// The combine's output: `pair.a` with the latest `imu.sample`.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes, Clone, Copy, Debug, PartialEq)]
#[frame(name = "both")]
#[repr(C)]
struct Both {
    #[frame(timestamp)]
    timestamp: Timestamp,
    a: f64,
    sample: f64,
}

/// What the sink read.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<(Vec<Half>, Vec<Both>)>>);

#[system]
fn sink(#[state] s: &mut Sink, half: &mut Input<Half>, both: &mut Input<Both>) {
    let Ok(mut seen) = s.0.lock() else { return };
    seen.0.extend(half.drain().filter_map(Result::ok).copied());
    seen.1.extend(both.drain().filter_map(Result::ok).copied());
}

const TARGET: &str = r#"
from test_pack import Both, Half, PairSource, Sink
from metor_config import Mux, Target

t = Target(1000.0)
src = t.add("src", PairSource())
split = t.add("split", Mux(half=Half(a=src.pair.a)))
est = t.add("est", Mux(both=Both(a=src.pair.a, sample=src.imu.sample), on=src.pair, max_age=0.002))
t.add("sink", Sink(half=split.half, both=est.both))
"#;

fn table(pack: &str, recorder: &Sink) -> SystemTable {
    let mut table = SystemTable::new();
    let ty = |name: &str| format!("{pack}{name}");
    let seen = recorder.clone();
    table
        .register(&ty("pair_source"), pair_source(), Default::default)
        .expect("registers");
    table
        .register(&ty("sink"), sink(), move || seen.clone())
        .expect("registers");
    table
}

/// Renders the pack module, runs the target file, and returns its config.
fn config() -> CoordinatorConfig {
    let dir = tempfile::tempdir().expect("a temp dir");
    let def = PackDef::from_table(&table("", &Sink::default()));
    let reference = PackRef::dylib("test".into(), "test".into(), "unused".into());
    let module = render(&reference, ABI_VERSION, &def).expect("renders");
    std::fs::write(dir.path().join("test_pack.py"), module).expect("writes");
    std::fs::write(dir.path().join("target.py"), TARGET).expect("writes");
    let out = dir.path().join("config.json");
    let python = std::env::var_os("METOR_PYTHON").unwrap_or_else(|| "python3".into());
    let config_py = Path::new(env!("CARGO_MANIFEST_DIR")).join("python/metor-config");
    let paths = std::env::join_paths([config_py.as_path(), dir.path()]).expect("joins");
    let output = Command::new(python)
        .arg("target.py")
        .env("PYTHONPATH", paths)
        .env("METOR_CONFIG_OUT", &out)
        .current_dir(dir.path())
        .output()
        .expect("python runs");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(&out).expect("the config");
    let deployment = DeploymentConfig::from_slice(&bytes).expect("parses");
    let mut resolved = deployment.into_runtime().expect("resolves");
    resolved.targets.remove(0).coordinator
}

fn host(sink: &Sink) -> SystemTable {
    let mut table = table("test.", sink);
    register_builtins(&mut table).expect("the builtins register");
    table
}

/// Steps cycles 1 to 12, one millisecond each.
fn run() -> (Vec<Half>, Vec<Both>) {
    let sink = Sink::default();
    let mut coordinator = config().build(&host(&sink)).expect("builds");
    for ms in 1..=12 {
        coordinator.step(Timestamp(ms * 1000));
    }
    let seen = sink.0.lock().expect("unpoisoned");
    seen.clone()
}

#[test]
fn test_split_and_combine() {
    let (halves, boths) = run();
    assert_eq!(halves.len(), 12);
    assert_eq!(
        halves[2],
        Half {
            timestamp: Timestamp(3000),
            a: 3000.0
        }
    );
    assert_eq!(
        boths[0],
        Both {
            timestamp: Timestamp(5000),
            a: 5000.0,
            sample: -5000.0,
        }
    );
    assert_eq!(boths[3].sample, -10000.0);
    let stamps: Vec<i64> = boths.iter().map(|b| b.timestamp.0 / 1000).collect();
    assert_eq!(
        stamps,
        [5, 6, 7, 10, 11, 12],
        "max_age withholds while stale"
    );
}
