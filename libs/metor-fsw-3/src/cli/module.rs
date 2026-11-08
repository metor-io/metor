//! Generates typed Python modules.
//!
//! `metor pack dev` reads a pack's descriptor and renders one Python class
//! per system and record, so target files can wire the pack with type
//! checking. The same renderer emits `metor_config`'s module of the systems
//! built into the host.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use metor_proto_wkt::{LogEvent, SequenceCommand};

use super::pack_dev::PackDevError;
use super::port_class::Accessors;
use super::port_stub::{Attr, GroupClass, Keyword, Leaf, Stubs};
use super::port_tree::{ShapeError, port_tree};
use crate::channel::{ABORT_PORT, COMMANDS_PORT, ChannelMessage, ChannelStatus};
use crate::coordinator::{STATUS_PORT, SystemStatus, SystemTable};
use crate::def::DefCx;
use crate::dl::{PackKind, PackRef};
use crate::fn_system::LOG_PORT;
use crate::frame_fields::FieldTable;
use crate::io::{Cqe, IO_CQ, IO_SQ, ResourceStatus, Sqe};
use crate::pack::ABI_VERSION;
use crate::pack::def::{PackDef, PackSystemDef};
use crate::port::Output;
use crate::record::{Record, RecordSchema};
use crate::system::PortDef;

const TEMPLATE: &str = include_str!("module.py.jinja");
const BUILTINS_TEMPLATE: &str = include_str!("builtins.py.jinja");
const RECORD_TEMPLATE: &str = include_str!("record.py.jinja");

/// The prefix of every built-in system type.
const BUILTIN_PACK: &str = "fsw";

/// The output ports every system has, whether or not it declares them, with
/// the record class each carries.
fn framework_ports() -> [(&'static str, RecordClass); 2] {
    [
        (LOG_PORT, RecordClass::of::<LogEvent>()),
        (STATUS_PORT, RecordClass::of::<SystemStatus>()),
    ]
}

/// The outputs a channel adds to the system behind it.
fn channel_ports() -> [(&'static str, RecordClass); 2] {
    [
        (
            crate::channel::STATUS_PORT,
            RecordClass::of::<ChannelStatus>(),
        ),
        (
            crate::channel::MESSAGE_PORT,
            RecordClass::of::<ChannelMessage>(),
        ),
    ]
}

/// The records whose class name is their Rust type's, not their name in
/// Pascal case.
fn named_records() -> [RecordClass; 3] {
    [
        RecordClass::of::<LogEvent>(),
        RecordClass::of::<SystemStatus>(),
        RecordClass::of::<ResourceStatus>(),
    ]
}

/// The data the template renders.
#[derive(Serialize)]
struct Module {
    pack_id: String,
    lib: String,
    abi_version: u32,
    /// Whether the pack also ships as a wasm module.
    wasm: bool,
    /// One class per record the pack uses, sorted by class name.
    records: Vec<RecordClass>,
    /// One dataclass per nested params object, sorted by name.
    dataclasses: Vec<Class>,
    systems: Vec<Entry>,
    /// Whether any system takes a port list from its config.
    takes_ports: bool,
    /// Whether the module names `Sequence`.
    takes_sequences: bool,
    /// Whether any system has a group of ports.
    takes_groups: bool,
    /// Whether any system takes a `Cap`, so the module imports `ResourceHandle`.
    takes_caps: bool,
}

/// A record class in the generated module.
///
/// `name` is the record's name on the host, `class` is the Python class name,
/// and `kind` is `"frame"` or `"msg"`. A frame also carries its leaves'
/// `(path, signature)` pairs in vtable order.
#[derive(Clone, Serialize)]
struct RecordClass {
    name: String,
    class: String,
    kind: &'static str,
    fields: Vec<(String, String)>,
    /// A frame's `__init__` keywords and port classes.
    accessors: Accessors,
    /// The frame's port class name.
    port: String,
}

impl RecordClass {
    /// The class for record `R`, named after its Rust type.
    fn of<R: Record>() -> Self {
        let def = Output::<R>::def(&DefCx::empty(), R::NAME);
        Self::from_def(&def, type_name::<R>().to_string())
    }

    /// The class for the record `def` carries, with schema rooted at `def.name`.
    fn from_def(def: &PortDef, class: String) -> Self {
        let frame = is_frame(def);
        let fields = if frame {
            leaf_signatures(def)
        } else {
            Vec::new()
        };
        Self {
            name: def.record.to_string(),
            accessors: Accessors::of(&class, &fields),
            port: annotation(&class, true),
            class,
            kind: kind(frame),
            fields,
        }
    }
}

/// Each leaf's path and signature. A frame no `FieldTable` reads has none.
fn leaf_signatures(def: &PortDef) -> Vec<(String, String)> {
    let Ok(table) = FieldTable::of(def) else {
        return Vec::new();
    };
    table
        .leaves
        .iter()
        .map(|leaf| (leaf.path.clone(), FieldTable::signature(leaf)))
        .collect()
}

/// The data the built-in module's template renders.
#[derive(Serialize)]
struct BuiltinModule {
    pack_id: &'static str,
    abi_version: u32,
    config_version: u32,
    /// One class per record the framework or a built-in system uses, sorted
    /// by class name.
    records: Vec<RecordClass>,
    framework_ports: Vec<Port>,
    channel_ports: Vec<Port>,
    commands_port: &'static str,
    /// The records on the IO ports, which the build wires.
    io_records: [&'static str; 2],
    systems: Vec<BuiltinClass>,
}

/// The base class of one built-in system.
#[derive(Serialize)]
struct BuiltinClass {
    name: String,
    ty: String,
    takes_inputs: bool,
    takes_outputs: bool,
    ports: Vec<Port>,
}

#[derive(Serialize)]
struct Class {
    name: String,
    /// The class docstring, or empty.
    doc: String,
    fields: Vec<Field>,
}

/// A dataclass field or an `__init__` keyword.
#[derive(Serialize)]
struct Field {
    name: String,
    annotation: String,
    /// The default, rendered as ` = <literal>`. Empty for a required field.
    default: String,
    /// The property's description, or empty.
    doc: String,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    ty: String,
    doc: String,
    /// The `Args:` section of the `__init__` docstring. Empty when no param
    /// has a doc.
    args_doc: String,
    /// Whether the type takes an `items` port list.
    takes_inputs: bool,
    /// Whether the type takes a `records` list.
    takes_outputs: bool,
    /// One keyword per input, list, or group of inputs.
    inputs: Vec<Keyword>,
    /// Every wired input with its record class, when any is in a list or
    /// group.
    input_ports: Vec<Leaf>,
    params: Vec<Field>,
    /// The name of each `Cap` parameter.
    caps: Vec<String>,
    /// Every output port with its record class. The framework's ports come
    /// last.
    ports: Vec<Port>,
    /// One attribute per output, list, or group of outputs.
    attrs: Vec<Attr>,
    /// The classes of the system's groups of inputs and outputs.
    groups: Vec<GroupClass>,
}

impl Entry {
    /// Whether the system's stub names `Sequence`.
    fn names_sequence(&self) -> bool {
        let groups = self.groups.iter().flat_map(|group| &group.keywords);
        let mut keywords = self.inputs.iter().chain(groups);
        let lists = keywords.any(|k| k.annotation.starts_with("Sequence"));
        lists || self.takes_inputs || self.takes_outputs
    }
}

#[derive(Serialize)]
struct Port {
    name: String,
    record: String,
    /// The type of the port on a handle: the frame's port class, or
    /// `OutPort[...]` for a message.
    annotation: String,
}

impl Port {
    fn of((name, record): (&str, RecordClass)) -> Self {
        Self {
            name: name.to_string(),
            annotation: annotation(&record.class, record.kind == "frame"),
            record: record.class,
        }
    }

    fn from_def(port: &PortDef) -> Self {
        let record = class_of(&port.record);
        Self {
            name: port.name.to_string(),
            annotation: annotation(&record, is_frame(port)),
            record,
        }
    }
}

fn annotation(class: &str, frame: bool) -> String {
    if frame {
        format!("{class}Port")
    } else {
        format!("OutPort[{class}]")
    }
}

fn is_frame(port: &PortDef) -> bool {
    matches!(port.schema, RecordSchema::Frame { .. })
}

/// Renders `metor_config/_generated.py` from the built-in systems in `table`.
pub fn render_builtins(table: &SystemTable) -> Result<String, PackDevError> {
    let def = PackDef::from_table(table);
    let mut env = minijinja::Environment::new();
    env.add_filter("python_string", python_string);
    env.add_template("record", RECORD_TEMPLATE)?;
    env.add_template("builtins", BUILTINS_TEMPLATE)?;
    Ok(env.get_template("builtins")?.render(builtin_view(&def))?)
}

fn builtin_view(def: &PackDef) -> BuiltinModule {
    let mut records = records(def);
    let channel = channel_ports().into_iter().map(|(_, record)| record);
    for record in channel.chain([RecordClass::of::<SequenceCommand>()]) {
        if !records.iter().any(|known| known.name == record.name) {
            records.push(record);
        }
    }
    records.sort_unstable_by(|a, b| a.class.cmp(&b.class));
    let prefix = format!("{BUILTIN_PACK}.");
    let systems = def
        .systems
        .iter()
        .map(|system| {
            let ty = system.ty.strip_prefix(&prefix).unwrap_or(&system.ty);
            BuiltinClass {
                name: pascal(ty),
                ty: ty.to_string(),
                takes_inputs: system.takes_inputs,
                takes_outputs: system.takes_outputs,
                ports: output_ports(system),
            }
        })
        .collect();
    BuiltinModule {
        pack_id: BUILTIN_PACK,
        abi_version: ABI_VERSION,
        config_version: crate::deploy::CONFIG_VERSION,
        records,
        framework_ports: framework_ports().into_iter().map(Port::of).collect(),
        channel_ports: channel_ports().into_iter().map(Port::of).collect(),
        commands_port: COMMANDS_PORT,
        io_records: [Sqe::NAME, Cqe::NAME],
        systems,
    }
}

/// Renders `<module>/__init__.py` for `def`.
pub fn render(pack: &PackRef, abi_version: u32, def: &PackDef) -> Result<String, PackDevError> {
    let module = view(pack, abi_version, def)?;
    let mut env = minijinja::Environment::new();
    env.add_filter("python_string", python_string);
    env.add_filter("docstring", docstring);
    env.add_template("record", RECORD_TEMPLATE)?;
    env.add_template("module", TEMPLATE)?;
    Ok(env.get_template("module")?.render(module)?)
}

fn view(pack: &PackRef, abi_version: u32, def: &PackDef) -> Result<Module, PackDevError> {
    let schemas: Vec<Value> = def
        .systems
        .iter()
        .map(parse_schema)
        .collect::<Result<_, _>>()?;
    let mut defs = BTreeMap::new();
    for schema in &schemas {
        collect_defs(schema, &mut defs);
    }
    let systems: Vec<Entry> = def
        .systems
        .iter()
        .zip(&schemas)
        .map(|(system, schema)| entry(system, schema, &defs))
        .collect::<Result<_, _>>()?;
    Ok(Module {
        takes_ports: systems
            .iter()
            .any(|entry| entry.takes_inputs || entry.takes_outputs),
        takes_caps: systems.iter().any(|entry| !entry.caps.is_empty()),
        takes_sequences: systems.iter().any(Entry::names_sequence),
        takes_groups: systems.iter().any(|entry| !entry.groups.is_empty()),
        pack_id: pack.id.clone(),
        lib: pack.lib.clone(),
        abi_version,
        wasm: matches!(pack.kind, PackKind::Wasm(_)),
        records: records(def),
        dataclasses: defs
            .iter()
            .map(|(name, object)| dataclass(name, object, &defs))
            .collect(),
        systems,
    })
}

fn records(def: &PackDef) -> Vec<RecordClass> {
    let mut records: Vec<RecordClass> = framework_ports()
        .into_iter()
        .map(|(_, record)| record)
        .collect();
    for port in def.systems.iter().flat_map(ports) {
        if !records.iter().any(|record| record.name == port.record) {
            records.push(RecordClass::from_def(port, class_of(&port.record)));
        }
    }
    records.sort_unstable_by(|a, b| a.class.cmp(&b.class));
    records
}

fn kind(frame: bool) -> &'static str {
    if frame { "frame" } else { "msg" }
}

/// The `__init__` keywords a generated class passes on itself.
const RESERVED_KEYWORDS: [&str; 3] = ["items", "records", "grants"];

/// Returns the ports a target can wire.
///
/// This is every port except `abort`, which only a channel writes to, and
/// the IO ports, which the builder wires.
fn ports(system: &PackSystemDef) -> impl Iterator<Item = &PortDef> {
    wired_inputs(system).chain(exposed_outputs(system))
}

fn wired_inputs(system: &PackSystemDef) -> impl Iterator<Item = &PortDef> {
    system
        .def
        .inputs
        .iter()
        .filter(|port| port.name != ABORT_PORT && !is_io(system, &port.name))
}

fn exposed_outputs(system: &PackSystemDef) -> impl Iterator<Item = &PortDef> {
    system
        .def
        .outputs
        .iter()
        .filter(|port| !is_io(system, &port.name))
}

/// Whether `port` is one of the IO ports a `Cap` adds.
fn is_io(system: &PackSystemDef, port: &str) -> bool {
    !system.def.grants.is_empty() && (port == IO_SQ || port == IO_CQ)
}

fn is_framework(port: &str) -> bool {
    framework_ports().iter().any(|(name, _)| *name == port)
}

fn class_of(record: &str) -> String {
    match named_records()
        .into_iter()
        .find(|known| known.name == record)
    {
        Some(known) => known.class,
        None => pascal(record),
    }
}

/// Every output port of `system` with its record class. The framework's
/// ports come last.
fn output_ports(system: &PackSystemDef) -> Vec<Port> {
    let declared = exposed_outputs(system).filter(|p| !is_framework(&p.name));
    let framework = framework_ports().into_iter().map(Port::of);
    declared.map(Port::from_def).chain(framework).collect()
}

/// The last path segment of `T`'s name.
fn type_name<T>() -> &'static str {
    let path = core::any::type_name::<T>();
    path.rsplit("::").next().unwrap_or(path)
}

pub(super) fn pascal(name: &str) -> String {
    name.split('_')
        .flat_map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .into_iter()
                .flat_map(char::to_uppercase)
                .chain(chars)
        })
        .collect()
}

/// Parses a system's params schema. A system without params gets an empty
/// object.
fn parse_schema(system: &PackSystemDef) -> Result<Value, PackDevError> {
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

/// Builds a dataclass for one nested params object.
fn dataclass(name: &str, object: &Value, defs: &BTreeMap<String, Value>) -> Class {
    Class {
        name: name.to_string(),
        doc: description(object),
        fields: fields(object, defs, true),
    }
}

/// Builds a field for each property of a schema object, with its type
/// annotation and default.
fn fields(schema: &Value, defs: &BTreeMap<String, Value>, dataclass: bool) -> Vec<Field> {
    properties(schema)
        .into_iter()
        .map(|(name, property)| Field {
            annotation: python_type(&property, defs),
            default: default_of(&property, is_required(schema, &name), dataclass),
            doc: description(&property),
            name,
        })
        .collect()
}

/// Builds the `System` subclass for one system.
///
/// Returns an error if a param has the same name as an input port, since both
/// become `__init__` keywords.
fn entry(
    system: &PackSystemDef,
    schema: &Value,
    defs: &BTreeMap<String, Value>,
) -> Result<Entry, PackDevError> {
    let params = fields(schema, defs, false);
    let caps: Vec<String> = system
        .def
        .grants
        .iter()
        .map(|g| g.param.to_string())
        .collect();
    let shape = |source: ShapeError| PackDevError::PortShape {
        system: system.ty.to_string(),
        port: source.port,
        other: source.other,
    };
    let stubs = port_stubs(system).map_err(shape)?;
    let input = |name: &str| stubs.inputs.iter().any(|keyword| keyword.name == name);
    let param = |name: &str| params.iter().any(|field| field.name == name);
    let clash = params.iter().map(|f| &f.name).find(|name| input(name));
    let taken = |name: &str| input(name) || param(name) || RESERVED_KEYWORDS.contains(&name);
    let clash = clash.or(caps.iter().find(|name| taken(name)));
    if let Some(name) = clash {
        return Err(PackDevError::NameClash {
            system: system.ty.to_string(),
            name: name.clone(),
        });
    }
    Ok(Entry {
        name: pascal(&system.ty),
        ty: system.ty.to_string(),
        doc: system.doc.to_string(),
        args_doc: args_doc(&params),
        takes_inputs: system.takes_inputs,
        takes_outputs: system.takes_outputs,
        input_ports: stubs.input_ports,
        inputs: stubs.inputs,
        params,
        caps,
        ports: output_ports(system),
        attrs: stubs.attrs,
        groups: stubs.groups,
    })
}

/// The keywords, attributes, and group classes of `system`'s ports.
struct PortStubs {
    inputs: Vec<Keyword>,
    input_ports: Vec<Leaf>,
    attrs: Vec<Attr>,
    groups: Vec<GroupClass>,
}

fn port_stubs(system: &PackSystemDef) -> Result<PortStubs, ShapeError> {
    let class = pascal(&system.ty);
    let record = |name: &str| class_of(name);
    let port = |def: &PortDef| Port::from_def(def).annotation;
    let mut stubs = Stubs::new(&class, &record, &port);
    let inputs = port_tree(wired_inputs(system))?;
    let declared = exposed_outputs(system).filter(|p| !is_framework(&p.name));
    let outputs = port_tree(declared)?;
    let keywords = stubs.keywords(&inputs, "")?;
    let framework = framework_ports().into_iter().map(|port| {
        let port = Port::of(port);
        Attr {
            name: port.name,
            annotation: port.annotation,
        }
    });
    let attrs = stubs
        .attrs(&outputs, "")?
        .into_iter()
        .chain(framework)
        .collect();
    let nested = wired_inputs(system).any(|port| port.name.contains('.'));
    let input_ports = wired_inputs(system)
        .filter(|_| nested)
        .map(|port| Leaf {
            name: port.name.to_string(),
            record: class_of(&port.record),
        })
        .collect();
    Ok(PortStubs {
        inputs: keywords,
        input_ports,
        attrs,
        groups: stubs.groups,
    })
}

fn description(schema: &Value) -> String {
    schema
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default()
}

/// Builds a Google-style `Args:` section for the params that have a doc.
fn args_doc(params: &[Field]) -> String {
    let args: Vec<String> = params
        .iter()
        .filter(|field| !field.doc.is_empty())
        .map(|field| {
            let lines: Vec<&str> = field
                .doc
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect();
            format!("    {}: {}", field.name, lines.join("\n        "))
        })
        .collect();
    if args.is_empty() {
        return String::new();
    }
    format!("Args:\n{}", args.join("\n"))
}

/// Formats `text` as a triple-quoted docstring, indenting continuation lines
/// by `indent` spaces.
fn docstring(text: &str, indent: usize) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
    if !escaped.contains('\n') {
        return format!("\"\"\"{escaped}\"\"\"");
    }
    let pad = " ".repeat(indent);
    let body: Vec<String> = escaped
        .lines()
        .map(|line| match line {
            "" => String::new(),
            line => format!("{pad}{line}"),
        })
        .collect();
    format!("\"\"\"\n{}\n{pad}\"\"\"", body.join("\n"))
}

/// Returns a schema object's properties, in the order the schema lists them.
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

/// Returns the Python type annotation for one property's schema.
fn python_type(schema: &Value, defs: &BTreeMap<String, Value>) -> String {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.rsplit('/').next().unwrap_or(reference);
        if defs.contains_key(name) {
            return name.to_string();
        }
        return "object".to_string();
    }
    match schema.get("type") {
        Some(Value::String(ty)) => python_ty(ty, schema, defs),
        Some(Value::Array(types)) => {
            let names: Vec<String> = types
                .iter()
                .filter_map(Value::as_str)
                .map(|ty| python_ty(ty, schema, defs))
                .collect();
            names.join(" | ")
        }
        _ => "object".to_string(),
    }
}

fn python_ty(ty: &str, schema: &Value, defs: &BTreeMap<String, Value>) -> String {
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

/// Returns the rendered default for a field.
///
/// This is the schema's `default` if it has one, nothing for a required
/// field, and ` = None` otherwise. Mutable defaults on a dataclass use a
/// `default_factory`.
fn default_of(schema: &Value, required: bool, dataclass: bool) -> String {
    match schema.get("default") {
        Some(value) if dataclass && (value.is_array() || value.is_object()) => {
            format!(" = field(default_factory=lambda: {})", literal(value))
        }
        Some(value) => format!(" = {}", literal(value)),
        None if required => String::new(),
        None => " = None".to_string(),
    }
}

fn literal(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => python_string(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(literal).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, item)| format!("{}: {}", python_string(key), literal(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn python_string(text: &str) -> String {
    Value::String(text.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use metor_proto::types::ComponentId;
    use serde_json::value::RawValue;

    use super::*;
    use crate::SystemKind;
    use crate::system::SystemDef;

    fn port(name: &'static str, record: &'static str) -> PortDef {
        PortDef {
            name: name.into(),
            record: record.into(),
            id: ComponentId::new(record),
            max_len: 8,
            alignment: 4,
            depth: 8,
            layout: None,
            schema: crate::RecordSchema::msg(record, crate::MsgCodec::Bytes),
        }
    }

    fn system(
        ty: &'static str,
        inputs: Vec<PortDef>,
        outputs: Vec<PortDef>,
        params: Option<&'static str>,
    ) -> PackSystemDef {
        PackSystemDef {
            ty: ty.into(),
            kind: SystemKind::Cyclic,
            takes_inputs: false,
            takes_outputs: false,
            def: SystemDef {
                name: ty.into(),
                inputs,
                outputs,
                log: None,
                grants: Vec::new(),
                io: None,
            },
            doc: "".into(),
            params: params.map(|text| RawValue::from_string(text.into()).expect("json")),
        }
    }

    fn rendered(systems: Vec<PackSystemDef>) -> Result<String, PackDevError> {
        let reference = PackRef::dylib("test".into(), "test_pack".into(), "unused".into());
        render(&reference, 1, &PackDef { systems })
    }

    #[test]
    fn test_render_param_defaults() {
        let schema = r##"{"type":"object","properties":{
            "rate":{"type":"number","default":0.5,"description":"Loop rate, Hz.\n\nClamped to \"max\"."},
            "count":{"type":"integer","default":3},
            "armed":{"type":"boolean","default":false},
            "mode":{"type":"string","default":"nadir"},
            "gains":{"type":"array","items":{"type":"number"},"default":[1.0,2.0]},
            "limits":{"$ref":"#/$defs/Limits"},
            "slack":{"type":["number","null"]}},
            "required":["limits"],
            "$defs":{"Limits":{"description":"Bounds on the actuator.",
                "type":"object","properties":{"max":{"type":"number","default":1.0,"description":"Upper bound."}}}}}"##;
        let text = rendered(vec![system(
            "ctrl",
            vec![port("est", "est")],
            vec![port("cmd", "motor_cmd"), port("log", "log")],
            Some(schema),
        )])
        .expect("renders");

        assert!(text.contains("from dataclasses import dataclass, field\n"));
        assert!(text.contains("@dataclass(kw_only=True)\nclass Limits:\n    \"Bounds on the actuator.\"\n    max: float = 1.0\n    \"\"\"Upper bound.\"\"\"\n"));
        assert!(
            text.contains(") -> None:\n        \"\"\"\n        Args:\n            rate: Loop rate, Hz.\n                Clamped to \\\"max\\\".\n        \"\"\"\n        super()"),
            "params with a description get an Args entry: {text}"
        );
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
        assert!(
            text.contains(
                "_ports = {\"cmd\": MotorCmd, \"log\": LogEvent, \"status\": SystemStatus}"
            )
        );
    }

    #[test]
    fn test_list_ports_render_as_sequences() {
        let inputs = vec![
            port("fss.0", "fss"),
            port("fss.1", "fss"),
            port("gain", "est"),
        ];
        let outputs = vec![
            port("est.0", "est"),
            port("est.1", "est"),
            port("grid.0.0", "est"),
            port("grid.0.1", "est"),
            port("log", "log"),
        ];
        let text = rendered(vec![system("sun", inputs, outputs, None)]).expect("renders");
        assert!(
            text.contains("from typing import Literal, Sequence\n"),
            "{text}"
        );
        assert!(
            text.contains("    _input_ports = {\"fss.0\": Fss, \"fss.1\": Fss, \"gain\": Est}\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "def __init__(self, *, fss: Sequence[Sources[Fss]] | None = None, gain: Sources[Est] = ()) -> None:"
            ),
            "{text}"
        );
        assert!(text.contains("super().__init__({\"fss\": fss, \"gain\": gain}, {})"));
        assert!(text.contains("_ports = {\"est.0\": Est, \"est.1\": Est, \"grid.0.0\": Est"));
        assert!(
            text.contains("    est: tuple[OutPort[Est], ...]\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "    grid: tuple[tuple[OutPort[Est], ...], ...]\n    log: OutPort[LogEvent]\n"
            ),
            "{text}"
        );
    }

    /// The adcs system of the nesting design: a list of wheel groups.
    fn adcs() -> PackSystemDef {
        let inputs = vec![
            port("wheels.0.speed", "wheel_speed"),
            port("wheels.1.speed", "wheel_speed"),
            port("mag", "mag"),
        ];
        let outputs = vec![
            port("wheels.0.cmd", "wheel_cmd"),
            port("wheels.1.cmd", "wheel_cmd"),
            port("log", "log"),
        ];
        system("adcs", inputs, outputs, None)
    }

    #[test]
    fn test_group_ports_render_classes() {
        let text = rendered(vec![adcs()]).expect("renders");
        assert!(
            text.contains("from typing import TYPE_CHECKING, Literal, Sequence\n"),
            "{text}"
        );
        assert!(
            text.contains("OutGroup, OutPort, Pack, PortGroup, Record"),
            "{text}"
        );
        let wheels = "    class Wheels(PortGroup):\n        _input_ports = {\"speed\": WheelSpeed}\n        if TYPE_CHECKING:\n\n            def __init__(self, *, speed: Sources[WheelSpeed] = ()) -> None: ...\n";
        assert!(text.contains(wheels), "{text}");
        assert!(
            text.contains("    class WheelsOut(OutGroup):\n        cmd: OutPort[WheelCmd]\n"),
            "{text}"
        );
        assert!(
            text.contains("def __init__(self, *, wheels: Sequence[Adcs.Wheels] | None = None, mag: Sources[Mag] = ()) -> None:"),
            "{text}"
        );
        assert!(
            text.contains("    wheels: tuple[Adcs.WheelsOut, ...]\n"),
            "{text}"
        );
    }

    #[test]
    fn test_reject_group_child_python_cannot_name() {
        let inputs = vec![port("a.1", "imu"), port("a.2", "imu")];
        let clash = rendered(vec![system("odd", inputs, vec![], None)]);
        let Err(PackDevError::PortShape { port, .. }) = clash else {
            panic!("the shape is reported")
        };
        assert_eq!(port, "a.1");
    }

    /// The paths a rendered `PortShape` error names.
    fn shape_error(inputs: Vec<PortDef>, outputs: Vec<PortDef>) -> (String, Option<String>) {
        match rendered(vec![system("adcs", inputs, outputs, None)]) {
            Err(PackDevError::PortShape { port, other, .. }) => (port, other),
            other => panic!("the shape is reported: {:?}", other.err()),
        }
    }

    #[test]
    fn test_groups_of_one_class_name_must_match() {
        let same = vec![port("wheels.0.cmd.a", "imu"), port("wheels_cmd.a", "imu")];
        let text = rendered(vec![system("adcs", same, vec![], None)]).expect("renders");
        assert_eq!(
            text.matches("class WheelsCmd(PortGroup)").count(),
            1,
            "{text}"
        );
        let apart = vec![port("wheels.0.cmd.a", "imu"), port("wheels_cmd.b", "imu")];
        let (port, other) = shape_error(apart, vec![]);
        assert_eq!(
            (port.as_str(), other.as_deref()),
            ("wheels_cmd", Some("wheels.0.cmd"))
        );
    }

    #[test]
    fn test_input_and_output_groups_of_one_class_name_clash() {
        let inputs = vec![port("motor_out.a", "imu")];
        let outputs = vec![port("motor.a", "imu")];
        let (port, other) = shape_error(inputs, outputs);
        assert_eq!(
            (port.as_str(), other.as_deref()),
            ("motor", Some("motor_out"))
        );
    }

    #[test]
    fn test_reject_python_keywords() {
        assert_eq!(shape_error(vec![port("in", "imu")], vec![]).0, "in");
        assert_eq!(
            shape_error(vec![], vec![port("a.class", "imu")]).0,
            "a.class"
        );
        assert_eq!(shape_error(vec![port("b.self", "imu")], vec![]).0, "b.self");
    }

    #[test]
    fn test_flat_inputs_list_no_input_ports() {
        let text =
            rendered(vec![system("nav", vec![port("est", "est")], vec![], None)]).expect("renders");
        assert!(!text.contains("_input_ports"), "{text}");
        assert!(!text.contains("Sequence"), "{text}");
    }

    #[test]
    fn test_reject_port_beside_its_children() {
        let inputs = vec![port("fss", "fss"), port("fss.0", "fss")];
        let clash = rendered(vec![system("sun", inputs, vec![], None)]);
        let Err(PackDevError::PortShape { system, port, .. }) = clash else {
            panic!("the clash is reported")
        };
        assert_eq!((system.as_str(), port.as_str()), ("sun", "fss"));
    }

    #[test]
    fn test_render_builtins() {
        let mut table = SystemTable::new();
        crate::builtins::register_builtins(&mut table).expect("the builtins register");
        let text = render_builtins(&table).expect("renders");
        assert!(
            text.contains(&format!("ABI_VERSION = {ABI_VERSION}\n")),
            "{text}"
        );
        assert!(
            text.contains("class ChannelRegistry(System):\n    _pack = PACK\n    _ty = \"channel_registry\"\n    _ports = {\"registry\": SequenceRegistry, \"log\": LogEvent, \"status\": SystemStatus}\n"),
            "{text}"
        );
        assert!(text.contains("FRAMEWORK_PORTS: dict[str, type[Record]] = {\"log\": LogEvent, \"status\": SystemStatus}\n"));
        assert!(text.contains(
            "CHANNEL_PORTS: dict[str, type[Record]] = {\"channel_status\": ChannelStatus, \"channel_message\": ChannelMessage}\n"
        ));
        assert!(!text.contains("class Interface"), "{text}");
        assert!(text.contains("IO_RECORDS = (\"io_sqe\", \"io_cqe\")\n"));
        assert!(text.contains("class AlarmRaised(Record):\n    _name = \"alarm_raised\"\n"));
        assert!(
            text.contains("_ports = {\"link_status\": LinkStatus, \"alarm_raised\": AlarmRaised, \"alarm_cleared\": AlarmCleared, \"log\": LogEvent, \"status\": SystemStatus}"),
            "a sequence gains the alarm ports: {text}"
        );
    }

    #[test]
    fn test_render_builtins_of_an_empty_table() {
        let text = render_builtins(&SystemTable::new()).expect("renders");
        assert!(!text.contains("(System):"), "{text}");
        assert!(text.contains("class SequenceCommand(Record):\n    _name = \"sequence_command\"\n    _kind = \"msg\"\n"));
        assert!(text.contains(
            "class SystemStatus(Record):\n    _name = \"status\"\n    _kind = \"frame\"\n"
        ));
    }

    #[test]
    fn test_render_wasm_flag() {
        let mut reference = PackRef::dylib("test".into(), "test_pack".into(), "unused".into());
        let def = PackDef {
            systems: Vec::new(),
        };
        let dylib = render(&reference, 1, &def).expect("renders");
        assert!(!dylib.contains("wasm"), "{dylib}");
        reference.kind = PackKind::Wasm(Default::default());
        let wasm = render(&reference, 1, &def).expect("renders");
        assert!(
            wasm.contains("    abi_version=1,\n    wasm=True,\n)"),
            "{wasm}"
        );
    }

    #[test]
    fn test_reject_param_input_collision() {
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
    fn test_empty_system_includes_framework_records() {
        let text = rendered(vec![system("mode", vec![], vec![], None)]).expect("renders");
        assert!(!text.contains("dataclass"));
        assert!(
            text.contains("class LogEvent(Record):\n    _name = \"log\"\n    _kind = \"msg\"\n")
        );
        assert!(text.contains(
            "class SystemStatus(Record):\n    _name = \"status\"\n    _kind = \"frame\"\n"
        ));
        assert!(
            text.contains("    def __init__(self) -> None:\n        super().__init__({}, {})\n"),
            "no params, no docstring: {text}"
        );
        assert!(text.contains("_ports = {\"log\": LogEvent, \"status\": SystemStatus}"));
        assert!(text.contains("    log: OutPort[LogEvent]\n    status: SystemStatusPort\n"));
    }

    #[test]
    fn test_abort_input_not_wired() {
        let text = rendered(vec![system(
            "arm",
            vec![port("abort", "abort_request"), port("est", "est")],
            vec![],
            None,
        )])
        .expect("renders");
        assert!(
            text.contains("def __init__(self, *, est: Sources[Est] = ()) -> None:"),
            "{text}"
        );
        assert!(!text.contains("AbortRequest"), "{text}");
    }

    fn capped(params: Option<&'static str>, caps: &[&'static str]) -> PackSystemDef {
        let mut def = system(
            "gps",
            vec![port("est", "est"), port(IO_CQ, "io_cqe")],
            vec![port("fix", "gps_fix"), port(IO_SQ, "io_sqe")],
            params,
        );
        def.def.grants = caps
            .iter()
            .map(|name| crate::io::GrantDef {
                param: (*name).into(),
                interface: "uart".into(),
            })
            .collect();
        def
    }

    #[test]
    fn test_caps_render_keywords_and_grants() {
        let text = rendered(vec![capped(None, &["main", "aux"])]).expect("renders");
        assert!(text.contains("Record, ResourceHandle, Sources"), "{text}");
        assert!(text.contains("    _caps = (\"main\", \"aux\")\n"), "{text}");
        assert!(
            text.contains("est: Sources[Est] = (), main: ResourceHandle, aux: ResourceHandle)"),
            "{text}"
        );
        assert!(
            text.contains("grants={\"main\": main, \"aux\": aux})"),
            "{text}"
        );
    }

    #[test]
    fn test_io_ports_are_not_wired_or_exposed() {
        let text = rendered(vec![capped(None, &["uart"])]).expect("renders");
        assert!(!text.contains("io_cq"), "{text}");
        assert!(!text.contains("io_sq"), "{text}");
        assert!(!text.contains("IoSqe") && !text.contains("IoCqe"), "{text}");
    }

    #[test]
    fn test_reject_cap_param_collision() {
        let params = r#"{"type":"object","properties":{"uart":{"type":"number"}}}"#;
        let clash = rendered(vec![capped(Some(params), &["uart"])]);
        let Err(PackDevError::NameClash { name, .. }) = clash else {
            panic!("the clash is reported")
        };
        assert_eq!(name, "uart");
        let clash = rendered(vec![capped(None, &["est"])]);
        assert!(matches!(clash, Err(PackDevError::NameClash { .. })));
        let clash = rendered(vec![capped(None, &["grants"])]);
        assert!(matches!(clash, Err(PackDevError::NameClash { .. })));
    }
}
