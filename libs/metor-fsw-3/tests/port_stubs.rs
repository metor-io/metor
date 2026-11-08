//! Stubs for lists and groups of ports, rendered from a `#[system]` with
//! arrays and `SystemParams` structs, checked by Python and pyright.

use std::path::{Path, PathBuf};
use std::process::Command;

use metor_fsw_3::cli::module::render;
use metor_fsw_3::coordinator::SystemTable;
use metor_fsw_3::dl::PackRef;
use metor_fsw_3::pack::def::PackDef;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{ABI_VERSION, Frame, Input, Output, SystemParams, Timestamp, system};
use serde_json::{Value, json};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "fss")]
#[repr(C)]
struct Fss {
    #[frame(timestamp)]
    timestamp: Timestamp,
    sun: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "wheel_cmd")]
#[repr(C)]
struct WheelCmd {
    #[frame(timestamp)]
    timestamp: Timestamp,
    torque: f64,
}

/// Read only for its port names.
#[allow(dead_code)]
#[derive(SystemParams)]
struct WheelIo<'a> {
    speed: &'a mut Input<Fss>,
    cmd: &'a mut Output<WheelCmd>,
}

#[derive(SystemParams)]
struct Sensors<'a> {
    fss: [&'a mut Input<Fss>; 3],
    wheels: [WheelIo<'a>; 2],
}

/// Reads three sun sensors and drives two wheels.
#[system]
fn adcs(sensors: Sensors<'_>, est: [&mut Output<Fss>; 2]) {
    let _ = (sensors.fss, sensors.wheels, est);
}

/// The `ports_stub` module: stubgen's rendering of `adcs`.
fn ports_stub() -> String {
    let mut table = SystemTable::new();
    table.register("adcs", adcs(), || ()).expect("valid");
    let reference = PackRef::dylib("ports".into(), "ports".into(), "unused".into());
    render(&reference, ABI_VERSION, &PackDef::from_table(&table)).expect("renders")
}

fn config_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("python/metor-config")
}

/// The stub, written beside a copy of each named typing file.
fn stub_dir(files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temporary directory");
    std::fs::write(dir.path().join("ports_stub.py"), ports_stub()).expect("writes");
    for file in files {
        let text = std::fs::read_to_string(config_dir().join("tests/typing").join(file));
        std::fs::write(dir.path().join(file), text.expect("reads")).expect("writes");
    }
    dir
}

/// Building a target from the stub wires each element and group child to
/// its dotted port.
#[test]
fn test_stub_wires_lists_and_groups() {
    let dir = stub_dir(&["ports_ok.py"]);
    let script =
        "import json, ports_ok; json.dump(ports_ok.target.to_config(), open('c.json', 'w'))";
    let python = std::env::var_os("METOR_PYTHON").unwrap_or_else(|| "python3".into());
    let output = Command::new(python)
        .args(["-c", script])
        .env("PYTHONPATH", config_dir())
        .current_dir(dir.path())
        .output()
        .expect("run Python 3.11+ (set METOR_PYTHON)");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    let config = std::fs::read(dir.path().join("c.json")).expect("the config");
    let config: Value = serde_json::from_slice(&config).expect("json");
    let systems = config["coordinator"]["systems"]
        .as_array()
        .expect("systems");
    let b = systems.iter().find(|s| s["id"] == "b").expect("b");
    let inputs: Vec<_> = b["inputs"]
        .as_array()
        .expect("inputs")
        .iter()
        .map(|input| (input["port"].clone(), input["from"].clone()))
        .collect();
    let from = |port: &str| json!([{"system": "a", "port": port}]);
    assert_eq!(
        inputs,
        [
            (json!("fss.0"), from("est.0")),
            (json!("fss.1"), from("est.1")),
            (json!("fss.2"), json!([])),
            (json!("wheels.0.speed"), from("est.0")),
        ]
    );
}

/// Pyright reports an error on each `# expect-error` line of
/// `tests/typing/ports_bad.py`, and none in `ports_ok.py`.
#[test]
fn test_port_typing() {
    let Some(pyright) = pyright() else {
        return;
    };
    let files = ["ports_ok.py", "ports_bad.py"];
    let dir = stub_dir(&files);
    let pyright_config = json!({"extraPaths": [config_dir()], "typeCheckingMode": "standard"});
    let config_path = dir.path().join("pyrightconfig.json");
    std::fs::write(config_path, pyright_config.to_string()).expect("writes");
    for file in files {
        let path = dir.path().join(file);
        let text = std::fs::read_to_string(&path).expect("reads");
        let expected: Vec<u64> = (1..)
            .zip(text.lines())
            .filter(|(_, line)| line.ends_with("# expect-error"))
            .map(|(n, _)| n)
            .collect();
        assert_eq!(error_lines(&pyright, dir.path(), &path), expected, "{file}");
    }
}

/// The 1-based lines pyright reports errors on.
fn error_lines(pyright: &[String], cwd: &Path, file: &Path) -> Vec<u64> {
    let output = Command::new(&pyright[0])
        .args(&pyright[1..])
        .arg("--outputjson")
        .arg(file)
        .current_dir(cwd)
        .output()
        .expect("pyright runs");
    let report: Value = serde_json::from_slice(&output.stdout).expect("pyright json");
    let diagnostics = report["generalDiagnostics"].as_array().cloned();
    diagnostics
        .unwrap_or_default()
        .iter()
        .filter(|d| d["severity"] == "error")
        .filter_map(|d| d["range"]["start"]["line"].as_u64().map(|l| l + 1))
        .collect()
}

/// The command that runs pyright, or `None` if it is not available.
///
/// Under `CI`, a missing pyright fails the test instead of skipping it.
fn pyright() -> Option<Vec<String>> {
    let found = [vec!["pyright"], vec!["uvx", "pyright"]]
        .into_iter()
        .find(|command| {
            Command::new(command[0])
                .arg("--version")
                .output()
                .is_ok_and(|out| out.status.success())
        })
        .map(|command| command.iter().map(|part| (*part).to_string()).collect());
    if found.is_none() {
        assert!(
            std::env::var_os("CI").is_none(),
            "neither `pyright` nor `uvx` is on PATH"
        );
        println!("skipped: neither `pyright` nor `uvx` is on PATH");
    }
    found
}
