//! Execute generated modules with the supported configuration interpreter.

use std::path::Path;
use std::process::Command;

use metor_fsw_3::ABI_VERSION;
use metor_fsw_3::cli::config::PackRef;
use metor_fsw_3::cli::module::render;
use metor_fsw_3::pack::def::PackDef;
use serde_json::{Value, json};

fn execute(schema: Value, text: &str, script: &str) {
    let descriptor: PackDef = serde_json::from_value(json!({"systems": [{
        "ty": "probe", "doc": text, "params": schema,
        "def": {"name": "probe", "inputs": [], "outputs": []}
    }]}))
    .expect("descriptor");
    let reference = PackRef {
        id: text.into(),
        lib: text.into(),
        libs: "unused".into(),
    };
    let module = render(&reference, ABI_VERSION, &descriptor).expect("renders");
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
fn defaults_are_independent_and_explicit_none_is_preserved() {
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
fn strings_and_dictionary_keys_round_trip_through_python() {
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
fn required_fields_are_still_required() {
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
fn a_subscription_keeps_the_generated_records_pack_dependency() {
    execute(
        json!({}),
        "probe",
        r#"
from generated import PACK, LogEvent
from metor_config import Subscribe, Target

target = Target(1)
target.add('logs', Subscribe([LogEvent], listen='127.0.0.1:0'))
config = target.to_config()
assert config['packs'] == [PACK.to_json()]
assert [system['ty'] for system in config['coordinator']['systems']] == ['fsw.subscribe']
"#,
    );
}
