//! Runs generated pack modules under the Python interpreter that target files
//! use.

use std::path::Path;
use std::process::Command;

use metor_fsw_3::ABI_VERSION;
use metor_fsw_3::builtins::register_builtins;
use metor_fsw_3::cli::module::{render, render_builtins};
use metor_fsw_3::coordinator::SystemTable;
use metor_fsw_3::dl::PackRef;
use metor_fsw_3::pack::def::PackDef;
use metor_fsw_3::zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use metor_fsw_3::{DefCx, Frame, FrameArray, FrameMap, Input, Output, PortDef, Record, Timestamp};
use serde_json::{Value, json};

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "sample")]
#[repr(C)]
struct Sample {
    #[frame(timestamp)]
    timestamp: Timestamp,
    a: f64,
    b: FrameArray<f64, 3>,
}

#[derive(
    metor_component::AsVTable,
    metor_component::Metadatatize,
    metor_component::Componentize,
    metor_component::Decomponentize,
    IntoBytes,
    Immutable,
    KnownLayout,
    FromBytes,
)]
#[repr(C)]
struct Wheel {
    speed: f64,
    torque: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "wheels")]
#[repr(C)]
struct Wheels {
    #[frame(timestamp)]
    timestamp: Timestamp,
    #[frame(nest)]
    wheel: Wheel,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "procs", max_len = 128)]
#[repr(C)]
struct Procs {
    #[frame(timestamp)]
    timestamp: Timestamp,
    procs: FrameMap<u32, u64>,
}

/// A group whose leaves share names with a `Field`'s attributes.
#[derive(
    metor_component::AsVTable,
    metor_component::Metadatatize,
    metor_component::Componentize,
    metor_component::Decomponentize,
    IntoBytes,
    Immutable,
    KnownLayout,
    FromBytes,
)]
#[repr(C)]
struct Attrs {
    signature: f64,
    path: f64,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "signed")]
#[repr(C)]
struct Signed {
    #[frame(timestamp)]
    timestamp: Timestamp,
    #[frame(nest)]
    g: Attrs,
}

/// A frame whose field is a Python keyword.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "kw")]
#[repr(C)]
struct Kw {
    #[frame(timestamp)]
    timestamp: Timestamp,
    class: f64,
}

#[derive(Record, metor_fsw_3::Schema, serde::Serialize, serde::Deserialize)]
#[record(max_len = 32)]
struct Note {
    text: String,
}

/// A descriptor whose one system outputs `outputs`.
fn emitting(outputs: Vec<PortDef>) -> PackDef {
    let mut descriptor: PackDef = serde_json::from_value(json!({"systems": [{
        "ty": "probe", "doc": "", "params": null,
        "def": {"name": "probe", "inputs": [], "outputs": []}
    }]}))
    .expect("descriptor");
    descriptor.systems[0].def.outputs = outputs;
    descriptor
}

fn out<T: Record>(port: &'static str) -> PortDef {
    Output::<T>::def(&DefCx::empty(), port)
}

/// A frame `tick` with no leaves. `derive(Frame)` needs one, so it is built
/// by hand.
fn tick() -> PortDef {
    PortDef {
        name: "tick".into(),
        record: "tick".into(),
        id: metor_fsw_3::metor_proto::types::ComponentId::new("tick"),
        layout: Some(1),
        schema: metor_fsw_3::RecordSchema::Frame {
            vtable: metor_fsw_3::metor_proto::vtable::builder::vtable([]),
            metadata: Vec::new(),
        },
        ..out::<Kw>("tick")
    }
}

fn execute(schema: Value, text: &str, script: &str) {
    let descriptor: PackDef = serde_json::from_value(json!({"systems": [{
        "ty": "probe", "doc": text, "params": schema,
        "def": {"name": "probe", "inputs": [], "outputs": []}
    }]}))
    .expect("descriptor");
    run(&descriptor, text, script);
}

/// Renders `descriptor` to `generated.py` and runs `script` beside it.
fn run(descriptor: &PackDef, text: &str, script: &str) {
    let reference = PackRef::dylib(text.into(), text.into(), "unused".into());
    let module = render(&reference, ABI_VERSION, descriptor).expect("renders");
    let dir = tempfile::tempdir().expect("temporary directory");
    std::fs::write(dir.path().join("generated.py"), module).expect("module");
    std::fs::write(dir.path().join("expected.json"), json!(text).to_string()).expect("expected");
    let python = std::env::var_os("METOR_PYTHON").unwrap_or_else(|| "python3".into());
    let output = Command::new(python)
        .args(["-c", script])
        .env(
            "PYTHONPATH",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("python/metor-config"),
        )
        .current_dir(dir.path())
        .output()
        .expect("run Python 3.11+ (set METOR_PYTHON)");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn nested_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "nested": {"$ref": "#/$defs/Nested"},
            "values": {"default": [{"items": [1]}]},
            "mapping": {"default": {"items": [1]}}
        },
        "required": ["nested"],
        "$defs": {"Nested": {
            "type": "object",
            "properties": {
                "a_empty": {"default": []},
                "b_empty": {"default": {}},
                "c_values": {"default": [{"items": [1]}]},
                "d_mapping": {"default": {"items": [1]}},
                "z_required": {"type": "integer"}
            },
            "required": ["z_required"]
        }}
    })
}

#[test]
fn test_independent_defaults_preserve_none() {
    execute(
        nested_schema(),
        "probe",
        r#"
from generated import Nested, Probe
from metor_config import Target

a, b = Nested(z_required=1), Nested(z_required=2)
a.a_empty.append(3)
a.b_empty['key'] = 3
a.c_values[0]['items'].append(3)
a.d_mapping['items'].append(3)
assert b.a_empty == [] and b.b_empty == {}
assert b.c_values == [{'items': [1]}] and b.d_mapping == {'items': [1]}

x, y = Probe(nested=a), Probe(nested=b)
x._params['values'][0]['items'].append(3)
x._params['mapping']['items'].append(3)
assert y._params['values'] == [{'items': [1]}]
assert y._params['mapping'] == {'items': [1]}
assert Probe(nested=b)._params['values'] == [{'items': [1]}]
assert Probe(nested=b, values=None, mapping=None)._params['values'] is None
assert Nested(z_required=1, c_values=None).c_values is None

target = Target(1)
target.add('probe', y)
params = target.to_config()['coordinator']['systems'][0]['params']
assert params['nested']['z_required'] == 2
assert params['nested']['c_values'] == [{'items': [1]}]
assert params['mapping'] == {'items': [1]}
"#,
    );
}

#[test]
fn test_python_string_round_trip() {
    let text = "line\nreturn\rtab\t\0\u{1}\u{8}\u{c}\u{1f}\"\\ café 🚀";
    let schema = json!({
        "type": "object",
        "properties": {"text": {"default": text}, "mapping": {"default": {text: text}}},
        "$defs": {"Nested": {"type": "object", "description": text, "properties": {}}}
    });
    execute(
        schema,
        text,
        r#"
import inspect
import json
from generated import PACK, Nested, Probe
from metor_config import Target

with open('expected.json') as file:
    expected = json.load(file)
assert PACK.id == PACK.lib == expected
# Python 3.13 cleans docstrings at compile time; compare both sides cleaned.
for cls in (Probe, Nested):
    assert inspect.cleandoc(cls.__doc__) == inspect.cleandoc(expected)
target = Target(1)
target.add('probe', Probe())
config = target.to_config()
assert config['packs'][0]['id'] == config['packs'][0]['lib'] == expected
params = config['coordinator']['systems'][0]['params']
assert params == {'text': expected, 'mapping': {expected: expected}}
"#,
    );
}

#[test]
fn test_missing_required_fields() {
    execute(
        nested_schema(),
        "probe",
        r#"
from generated import Nested, Probe

for constructor in (Nested, Probe):
    try:
        constructor()
    except TypeError:
        pass
    else:
        raise AssertionError('missing required keyword was accepted')
try:
    Nested(1)
except TypeError:
    pass
else:
    raise AssertionError('nested fields must be keyword-only')
"#,
    );
}

#[test]
fn test_subscription_preserves_pack_dependency() {
    execute(
        json!({}),
        "probe",
        r#"
from generated import PACK, LogEvent
from metor_config import Listen, Subscribe, Target

target = Target(1)
target.add('logs', Subscribe([LogEvent], transport=Listen('127.0.0.1:0')))
config = target.to_config()
assert config['packs'] == [PACK.to_json()]
assert [system['ty'] for system in config['coordinator']['systems']] == ['fsw.subscribe', 'fsw.io', 'fsw.io']
"#,
    );
}

#[test]
fn test_record_fields() {
    let descriptor = emitting(vec![
        out::<Sample>("sample"),
        out::<Wheels>("wheels"),
        out::<Procs>("procs"),
        out::<Note>("note"),
    ]);
    let script = r#"
from generated import Note, Procs, Sample, Wheels
from metor_config._generated import ChannelStatus, SystemStatus

assert list(Sample._fields.items()) == [('a', 'f64'), ('b', 'f64[3]')], Sample._fields
assert list(Wheels._fields) == ['wheel.speed', 'wheel.torque'], Wheels._fields
assert SystemStatus._fields and ChannelStatus._fields
assert Procs._kind == 'frame' and Procs._fields == {}
assert '_fields' not in vars(Note) and Note._fields == {}
"#;
    run(&descriptor, "probe", script);
}

#[test]
fn test_typed_ports() {
    let descriptor = emitting(vec![
        out::<Sample>("sample"),
        out::<Wheels>("wheels"),
        out::<Signed>("signed"),
        out::<Kw>("kw"),
        tick(),
    ]);
    run(
        &descriptor,
        "probe",
        r#"
from generated import Kw, Probe, Sample, SamplePort, Tick, TickPort, WheelsPort_wheel
from metor_config import ConfigError, Deployment, Field, Listen, Peer, Publish, Subscribe, Target

a = Target(1, namespace='a')
probe = a.add('probe', Probe())
assert type(probe.sample) is SamplePort
b = probe.sample.b
assert isinstance(b, Field) and (b.path, b.signature) == ('b', 'f64[3]')
wheel = probe.wheels.wheel
assert type(wheel) is WheelsPort_wheel and wheel.signature == '{speed:f64,torque:f64}'
assert wheel.speed.path == 'wheel.speed'
group = probe.signed.g
assert group.signature_.path == 'g.signature' and group.path_.path == 'g.path'
source = probe.kw.class_
assert Kw(class_=source)._mapping == {'class': source}
assert type(probe.tick) is TickPort
Tick()
try:
    Sample(a=b)
except ConfigError:
    pass
else:
    raise AssertionError('a shape mismatch was accepted')
pub = a.add('pub', Publish([probe], transport=Listen('0.0.0.0:2240')))
other = Target(1, namespace='b')
mirror = other.add('mirror', Subscribe(transport=Peer(pub), items=[probe]))
assert type(mirror.port(probe.sample)) is SamplePort
Deployment(targets=[a, other])
"#,
    );
}

/// Frames for the pyright checks in `python/metor-config/tests/typing`.
#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "sensors")]
#[repr(C)]
struct Sensors {
    #[frame(timestamp)]
    timestamp: Timestamp,
    gyro_b: FrameArray<f64, 3>,
    css: FrameArray<f64, 6>,
    #[frame(nest)]
    wheel: Wheel,
}

#[derive(Frame, IntoBytes, Immutable, KnownLayout, FromBytes)]
#[frame(name = "body")]
#[repr(C)]
struct Body {
    #[frame(timestamp)]
    timestamp: Timestamp,
    omega_b: FrameArray<f64, 3>,
    sun: FrameArray<f64, 6>,
    #[frame(nest)]
    wheel: Wheel,
}

/// The `sensors_stub` module the typing files import: a `plant` system that
/// reads a `Body` and writes `Sensors`, rendered by stubgen.
fn sensors_stub() -> String {
    let mut descriptor = emitting(vec![out::<Sensors>("sensors")]);
    descriptor.systems[0].ty = "plant".into();
    descriptor.systems[0].def.inputs = vec![Input::<Body>::def(&DefCx::empty(), "body")];
    let reference = PackRef::dylib("sensors".into(), "sensors".into(), "unused".into());
    render(&reference, ABI_VERSION, &descriptor).expect("renders")
}

/// Pyright reports an error on each `# expect-error` line of
/// each `_bad.py` file in `tests/typing`, and none in the `_ok.py` files,
/// against a stub stubgen renders.
#[test]
fn test_field_typing() {
    let Some(pyright) = pyright() else {
        return;
    };
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("python/metor-config");
    let dir = tempfile::tempdir().expect("temporary directory");
    std::fs::write(dir.path().join("sensors_stub.py"), sensors_stub()).expect("writes");
    let pyright_config = json!({"extraPaths": [config], "typeCheckingMode": "standard"});
    std::fs::write(
        dir.path().join("pyrightconfig.json"),
        pyright_config.to_string(),
    )
    .expect("writes");
    for file in [
        "fields_ok.py",
        "fields_bad.py",
        "composite_ok.py",
        "composite_bad.py",
    ] {
        let text = std::fs::read_to_string(config.join("tests/typing").join(file)).expect("reads");
        let path = dir.path().join(file);
        std::fs::write(&path, &text).expect("writes");
        let expected: Vec<u64> = (1..)
            .zip(text.lines())
            .filter(|(_, line)| line.ends_with("# expect-error"))
            .map(|(n, _)| n)
            .collect();
        assert_eq!(error_lines(&pyright, dir.path(), &path), expected, "{file}");
    }
}

/// The 1-based lines pyright reports errors on, each once.
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
    let mut lines: Vec<u64> = diagnostics
        .unwrap_or_default()
        .iter()
        .filter(|d| d["severity"] == "error")
        .filter_map(|d| d["range"]["start"]["line"].as_u64().map(|l| l + 1))
        .collect();
    lines.dedup();
    lines
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

/// The checked-in `metor_config/_generated.py` is what stubgen renders for
/// the built-in systems. `METOR_BLESS=1` rewrites it.
#[cfg(unix)]
#[test]
fn test_builtins_module_is_current() {
    let mut table = SystemTable::new();
    register_builtins(&mut table).expect("the builtins register");
    let rendered = render_builtins(&table).expect("renders");
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("python/metor-config/metor_config/_generated.py");
    if std::env::var_os("METOR_BLESS").is_some() {
        std::fs::write(&path, &rendered).expect("writes the module");
    }
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        current == rendered,
        "{} is stale; run `METOR_BLESS=1 cargo test -p metor-fsw-3 --test module`",
        path.display()
    );
}
