//! The typed Python module `pack dev` renders from a pack's descriptor.
//!
//! `module.py.jinja` is the shape of the output. This module builds the view
//! it renders from: one `Record` class per record name the pack's ports
//! carry, one `@dataclass` per nested params object, and one `System`
//! subclass per entry, with every Python type and literal already spelled.
//! The text is deterministic and holds no absolute path.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::config::PackRef;
use super::pack_dev::PackDevError;
use crate::pack::def::{PackDef, PackSystemDef};
use crate::system::PortDef;

const TEMPLATE: &str = include_str!("module.py.jinja");

/// The records every system has whatever it declared, named after their Rust types.
const FRAMEWORK: [(&str, &str); 2] = [("log", "LogEvent"), ("status", "SystemStatus")];

/// What the template renders.
#[derive(Serialize)]
struct Module {
    pack_id: String,
    lib: String,
    abi_version: u32,
    /// Record class names, sorted.
    records: Vec<String>,
    /// Nested params objects, by name.
    dataclasses: Vec<Class>,
    systems: Vec<Entry>,
}

#[derive(Serialize)]
struct Class {
    name: String,
    /// Docstring body, escaped and indented, or empty.
    doc: String,
    fields: Vec<Field>,
}

/// A dataclass field or an `__init__` keyword.
#[derive(Serialize)]
struct Field {
    name: String,
    annotation: String,
    /// ` = <literal>`, or empty for a required field.
    default: String,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    ty: String,
    doc: String,
    /// Declared output names, without the framework's.
    outputs: Vec<String>,
    inputs: Vec<Port>,
    params: Vec<Field>,
    /// Declared outputs with their record classes.
    ports: Vec<Port>,
}

#[derive(Serialize)]
struct Port {
    name: String,
    record: String,
}

/// Renders `<module>/__init__.py` for `def`.
pub fn render(pack: &PackRef, abi_version: u32, def: &PackDef<'_>) -> Result<String, PackDevError> {
    let module = view(pack, abi_version, def)?;
    let mut env = minijinja::Environment::new();
    env.add_template("module", TEMPLATE)?;
    Ok(env.get_template("module")?.render(module)?)
}

fn view(pack: &PackRef, abi_version: u32, def: &PackDef<'_>) -> Result<Module, PackDevError> {
    let schemas: Vec<Value> = def
        .systems
        .iter()
        .map(parse_schema)
        .collect::<Result<_, _>>()?;
    let mut defs = BTreeMap::new();
    for schema in &schemas {
        collect_defs(schema, &mut defs);
    }
    let systems = def
        .systems
        .iter()
        .zip(&schemas)
        .map(|(system, schema)| entry(system, schema, &defs))
        .collect::<Result<_, _>>()?;
    Ok(Module {
        pack_id: pack.id.clone(),
        lib: pack.lib.clone(),
        abi_version,
        records: records(def),
        dataclasses: defs
            .iter()
            .map(|(name, object)| dataclass(name, object, &defs))
            .collect(),
        systems,
    })
}

/// Every record class the pack's ports name, plus the framework's two, sorted.
fn records(def: &PackDef<'_>) -> Vec<String> {
    let mut names: Vec<&str> = FRAMEWORK.iter().map(|(name, _)| *name).collect();
    for port in def.systems.iter().flat_map(ports) {
        if !names.contains(&port.record) {
            names.push(port.record);
        }
    }
    let mut classes: Vec<String> = names.into_iter().map(class_of).collect();
    classes.sort_unstable();
    classes
}

fn ports<'a>(system: &'a PackSystemDef<'_>) -> impl Iterator<Item = &'a PortDef> {
    system.def.inputs.iter().chain(&system.def.outputs)
}

fn is_framework(port: &str) -> bool {
    FRAMEWORK.iter().any(|(name, _)| *name == port)
}

/// The Python class of a record name: the framework's two, else PascalCase.
fn class_of(record: &str) -> String {
    match FRAMEWORK.iter().find(|(name, _)| *name == record) {
        Some((_, class)) => (*class).to_string(),
        None => pascal(record),
    }
}

fn pascal(name: &str) -> String {
    name.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// The params schema as an object, or an empty one for a system without params.
fn parse_schema(system: &PackSystemDef<'_>) -> Result<Value, PackDevError> {
    let Some(raw) = system.params.as_deref() else {
        return Ok(Value::Object(serde_json::Map::new()));
    };
    serde_json::from_str(raw.get()).map_err(|source| PackDevError::Schema {
        system: system.ty.to_string(),
        source,
    })
}

/// Adds every object in the schema's `$defs` to the module's dataclasses.
fn collect_defs(schema: &Value, defs: &mut BTreeMap<String, Value>) {
    let Some(entries) = schema.get("$defs").and_then(Value::as_object) else {
        return;
    };
    for (name, object) in entries {
        if object.get("properties").is_some() {
            defs.insert(name.clone(), object.clone());
        }
    }
}

/// One nested params object as a dataclass of its properties.
fn dataclass(name: &str, object: &Value, defs: &BTreeMap<String, Value>) -> Class {
    Class {
        name: name.to_string(),
        doc: object
            .get("description")
            .and_then(Value::as_str)
            .map(|doc| doc_body(doc, "    "))
            .unwrap_or_default(),
        fields: fields(object, defs),
    }
}

/// A schema object's properties as fields, each with its annotation and default.
fn fields(schema: &Value, defs: &BTreeMap<String, Value>) -> Vec<Field> {
    properties(schema)
        .into_iter()
        .map(|(name, property)| Field {
            annotation: python_type(&property, defs),
            default: default_of(&property, is_required(schema, &name)),
            name,
        })
        .collect()
}

/// One system as its `System` subclass.
fn entry(
    system: &PackSystemDef<'_>,
    schema: &Value,
    defs: &BTreeMap<String, Value>,
) -> Result<Entry, PackDevError> {
    let params = fields(schema, defs);
    for field in &params {
        if system.def.inputs.iter().any(|port| port.name == field.name) {
            return Err(PackDevError::NameClash {
                system: system.ty.to_string(),
                name: field.name.clone(),
            });
        }
    }
    let port = |port: &PortDef| Port {
        name: port.name.to_string(),
        record: class_of(port.record),
    };
    let declared = || system.def.outputs.iter().filter(|p| !is_framework(p.name));
    Ok(Entry {
        name: pascal(system.ty),
        ty: system.ty.to_string(),
        doc: doc_body(&system.doc, "    "),
        outputs: declared().map(|p| p.name.to_string()).collect(),
        inputs: system.def.inputs.iter().map(port).collect(),
        params,
        ports: declared().map(port).collect(),
    })
}

/// A schema object's properties, in the order the schema lists them.
fn properties(schema: &Value) -> Vec<(String, Value)> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|fields| {
            fields
                .iter()
                .map(|(name, schema)| (name.clone(), schema.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn is_required(schema: &Value, name: &str) -> bool {
    schema
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|names| names.iter().any(|entry| entry == name))
}

/// The Python annotation for one property's schema.
fn python_type(schema: &Value, defs: &BTreeMap<String, Value>) -> String {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or(reference);
        if defs.contains_key(name) {
            return name.to_string();
        }
        return "object".to_string();
    }
    match schema.get("type") {
        Some(Value::String(ty)) => scalar(ty, schema, defs),
        Some(Value::Array(types)) => {
            let names: Vec<String> = types
                .iter()
                .filter_map(Value::as_str)
                .map(|ty| scalar(ty, schema, defs))
                .collect();
            names.join(" | ")
        }
        _ => "object".to_string(),
    }
}

fn scalar(ty: &str, schema: &Value, defs: &BTreeMap<String, Value>) -> String {
    match ty {
        "number" => "float".to_string(),
        "integer" => "int".to_string(),
        "boolean" => "bool".to_string(),
        "string" => "str".to_string(),
        "null" => "None".to_string(),
        "array" => match schema.get("items") {
            Some(items) => format!("list[{}]", python_type(items, defs)),
            None => "list[object]".to_string(),
        },
        _ => "object".to_string(),
    }
}

/// The keyword default: the schema's `default`, or none for a required field.
fn default_of(schema: &Value, required: bool) -> String {
    match schema.get("default") {
        Some(value) => format!(" = {}", literal(value)),
        None if required => String::new(),
        None => " = None".to_string(),
    }
}

/// One JSON value as the Python literal a default is spelled with.
fn literal(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => format!("\"{}\"", escape(text)),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(literal).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, item)| format!("\"{}\": {}", escape(key), literal(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A doc comment as a docstring body: escaped, with its lines kept and indented.
fn doc_body(doc: &str, indent: &str) -> String {
    let body = doc
        .lines()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join(&format!("\n{indent}"));
    body.replace('\\', "\\\\").replace("\"\"\"", "\\\"\\\"\\\"")
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use metor_proto::types::ComponentId;
    use serde_json::value::RawValue;

    use super::*;
    use crate::system::SystemDef;

    fn port(name: &'static str, record: &'static str) -> PortDef {
        PortDef {
            name,
            record,
            id: ComponentId::new(record),
            max_len: 8,
            alignment: 4,
            depth: 8,
        }
    }

    fn system(
        ty: &'static str,
        inputs: Vec<PortDef>,
        outputs: Vec<PortDef>,
        params: Option<&'static str>,
    ) -> PackSystemDef<'static> {
        PackSystemDef {
            ty,
            def: SystemDef {
                name: ty,
                inputs,
                outputs,
            },
            doc: Cow::Borrowed(""),
            params: params
                .map(|text| Cow::Owned(RawValue::from_string(text.into()).expect("json"))),
        }
    }

    fn rendered(systems: Vec<PackSystemDef<'static>>) -> Result<String, PackDevError> {
        let reference = PackRef {
            id: "test".into(),
            lib: "test_pack".into(),
            libs: "unused".into(),
        };
        render(&reference, 1, &PackDef { systems })
    }

    #[test]
    fn every_param_kind_renders_with_its_default() {
        let schema = r##"{"type":"object","properties":{
            "rate":{"type":"number","default":0.5},
            "count":{"type":"integer","default":3},
            "armed":{"type":"boolean","default":false},
            "mode":{"type":"string","default":"nadir"},
            "gains":{"type":"array","items":{"type":"number"},"default":[1.0,2.0]},
            "limits":{"$ref":"#/$defs/Limits"},
            "slack":{"type":["number","null"]}},
            "required":["limits"],
            "$defs":{"Limits":{"description":"Bounds on the actuator.",
                "type":"object","properties":{"max":{"type":"number","default":1.0}}}}}"##;
        let text = rendered(vec![system(
            "ctrl",
            vec![port("est", "est")],
            vec![port("cmd", "motor_cmd"), port("log", "log")],
            Some(schema),
        )])
        .expect("renders");

        assert!(text.contains("from dataclasses import dataclass\n"));
        assert!(text.contains("@dataclass\nclass Limits:\n    \"\"\"Bounds on the actuator.\"\"\"\n    max: float = 1.0\n"));
        assert!(text.contains("armed: bool = False"));
        assert!(text.contains("mode: str = \"nadir\""));
        assert!(text.contains("gains: list[float] = [1.0, 2.0]"));
        assert!(text.contains("count: int = 3"));
        assert!(
            text.contains("limits: Limits,"),
            "a required param has no default: {text}"
        );
        assert!(text.contains("slack: float | None = None"));
        assert!(text.contains("cmd: OutPort[MotorCmd]"));
        assert!(text.contains("_outputs = (\"cmd\",)"));
    }

    #[test]
    fn a_param_named_like_an_input_is_a_clash() {
        let clash = rendered(vec![system(
            "nav",
            vec![port("gain", "imu")],
            vec![],
            Some(r#"{"type":"object","properties":{"gain":{"type":"number"}}}"#),
        )]);
        let Err(PackDevError::NameClash { system, name }) = clash else {
            panic!("the clash is reported")
        };
        assert_eq!((system.as_str(), name.as_str()), ("nav", "gain"));
    }

    #[test]
    fn a_system_with_no_ports_still_has_the_framework_records() {
        let text = rendered(vec![system("mode", vec![], vec![], None)]).expect("renders");
        assert!(!text.contains("dataclass"));
        assert!(text.contains("class LogEvent(Record): ..."));
        assert!(text.contains("class SystemStatus(Record): ..."));
        assert!(
            text.contains("    def __init__(self) -> None:\n        super().__init__({}, {})\n")
        );
        assert!(text.contains("_outputs = ()"));
        assert!(text.contains("    log: OutPort[LogEvent]\n    status: OutPort[SystemStatus]\n"));
    }
}
