//! The registry of system types a config may name.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

use metor_fsw_3_ring::{NoWake, RingBuffer};
use serde_json::value::RawValue;

use crate::RecordSchema;
use crate::async_system::AsyncSystem;
use crate::def::{DefCx, DefError};
use crate::fn_system::{AsyncSystemFn, Ctor, FnAsyncSystem, FnSystem, SystemFn};
use crate::system::{
    InputBinding, OutputBinding, PortDef, System, SystemDef, SystemInputs, SystemOutputs,
};
use crate::thread::{DEFAULT_THREAD, Threads};

use super::BuildError;
use super::params::{ParamError, Params};
use super::run::{Runner, Step};

static STATUS: LazyLock<PortDef> =
    LazyLock::new(|| crate::Output::<super::SystemStatus>::def("status"));

/// Arguments for constructing a system using [`MakeFn`].
pub struct MakeCx<'a> {
    pub id: &'a str,
    pub thread: &'a str,
    pub def: &'a SystemDef,
    pub params: Params<'a>,
    pub inputs: Vec<Vec<&'a RingBuffer>>,
    pub outputs: Vec<&'a RingBuffer>,
    pub threads: &'a mut Threads,
}

/// Builds a system from [`MakeCx`].
pub(crate) type MakeFn = dyn Fn(MakeCx<'_>) -> Result<Box<dyn Step>, ParamError>;

pub(crate) struct TableEntry {
    pub def: SystemDef,
    /// The doc comment on the type's `execute`, empty for the trait path.
    pub doc: Cow<'static, str>,
    /// The JSON Schema of the type's params, `None` when it takes none.
    pub schema: Option<Box<RawValue>>,
    pub make: Box<MakeFn>,
}

/// A `SystemTable` maps systems names to their definitions and constructors.
#[derive(Default)]
pub struct SystemTable {
    entries: Vec<(String, TableEntry)>,
    index: HashMap<String, usize>,
}

impl SystemTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a `System` under `ty`, replacing any system registered under the same name.
    pub fn register_system<S: System + 'static>(
        &mut self,
        ty: &str,
        make: impl Fn(Params<'_>) -> Result<(S, S::State), ParamError> + 'static,
    ) -> Result<(), BuildError> {
        self.insert(
            ty,
            entry::<S>(make, Cow::Borrowed(""), None).map_err(def_error(ty))?,
        )
    }

    /// Registers a `#[system]` with `ty`
    pub fn register<S: SystemFn, M, C: Ctor<S, M> + 'static>(
        &mut self,
        ty: &str,
        ctor: C,
    ) -> Result<(), BuildError> {
        let make = move |params: Params<'_>| Ok((FnSystem::default(), ctor.make(params)?));
        let entry = entry::<FnSystem<S>>(make, Cow::Borrowed(S::DOC), C::schema())
            .map_err(def_error(ty))?;
        self.insert(ty, entry)
    }

    /// Registers an `AsyncSystem` under `ty`, constructed on its own thread.
    pub fn register_async_system<A: AsyncSystem + 'static>(
        &mut self,
        ty: &str,
        make: impl Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
    ) -> Result<(), BuildError> {
        let entry = async_entry::<A, _>(make, Cow::Borrowed(""), None).map_err(def_error(ty))?;
        self.insert(ty, entry)
    }

    /// Registers a `#[system]` type with an `async run` under `ty`.
    pub fn register_async<S: AsyncSystemFn, M, C: Ctor<S, M> + Send + Sync + 'static>(
        &mut self,
        ty: &str,
        ctor: C,
    ) -> Result<(), BuildError> {
        let make = move |params: Params<'_>| Ok((FnAsyncSystem::default(), ctor.make(params)?));
        let entry = async_entry::<FnAsyncSystem<S>, _>(make, Cow::Borrowed(S::DOC), C::schema())
            .map_err(def_error(ty))?;
        self.insert(ty, entry)
    }

    /// Adds an entry, replacing a same-named one in place so order is registration order.
    pub(crate) fn insert(&mut self, ty: &str, entry: TableEntry) -> Result<(), BuildError> {
        check_definition(&entry.def)?;
        for (name, known) in &self.entries {
            if name != ty {
                check_definitions(&known.def, &entry.def)?;
            }
        }
        self.put(ty, entry);
        Ok(())
    }

    /// Inserts entries in order, stopping at the first conflict. Earlier insertions remain.
    pub(crate) fn insert_all(
        &mut self,
        entries: Vec<(String, TableEntry)>,
    ) -> Result<(), BuildError> {
        for (ty, entry) in entries {
            self.insert(&ty, entry)?;
        }
        Ok(())
    }

    fn put(&mut self, ty: &str, entry: TableEntry) {
        match self.index.get(ty) {
            Some(&at) => self.entries[at].1 = entry,
            None => {
                self.index.insert(ty.to_string(), self.entries.len());
                self.entries.push((ty.to_string(), entry));
            }
        }
    }

    pub(crate) fn get(&self, ty: &str) -> Option<&TableEntry> {
        let &at = self.index.get(ty)?;
        Some(&self.entries[at].1)
    }

    /// Every registered type and its entry, in registration order.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&str, &TableEntry)> {
        self.entries.iter().map(|(ty, entry)| (ty.as_str(), entry))
    }

    /// Finds a record among the registered ports and the implicit status port.
    pub(crate) fn record(&self, name: &str) -> Option<&PortDef> {
        core::iter::once(&*STATUS)
            .chain(self.entries().flat_map(|(_, entry)| ports(&entry.def)))
            .find(|port| port.record == name)
    }
}

/// Names the type whose definition was refused.
fn def_error(ty: &str) -> impl FnOnce(DefError) -> BuildError + '_ {
    |source| BuildError::Def {
        id: ty.to_string(),
        source,
    }
}

fn ports(def: &SystemDef) -> impl Iterator<Item = &PortDef> {
    def.inputs.iter().chain(&def.outputs)
}

fn check_definition(def: &SystemDef) -> Result<(), BuildError> {
    for (at, port) in ports(def).enumerate() {
        check_record(&STATUS, port)?;
        for known in ports(def).take(at) {
            check_record(known, port)?;
        }
    }
    Ok(())
}

fn check_definitions(known: &SystemDef, incoming: &SystemDef) -> Result<(), BuildError> {
    for port in ports(incoming) {
        for known in ports(known) {
            check_record(known, port)?;
        }
    }
    Ok(())
}

fn check_record(known: &PortDef, port: &PortDef) -> Result<(), BuildError> {
    if known.record == port.record && !same_record(known, port) {
        return Err(BuildError::RecordConflict {
            record: port.record.to_string(),
        });
    }
    if known.record != port.record && known.id == port.id {
        return Err(BuildError::RecordIdConflict {
            id: port.id,
            first: known.record.to_string(),
            second: port.record.to_string(),
        });
    }
    if let (RecordSchema::Msg { id: a, .. }, RecordSchema::Msg { id: b, .. }) =
        (&known.schema, &port.schema)
        && a == b
        && known.schema != port.schema
    {
        return Err(BuildError::MessageIdConflict {
            id: *a,
            first: known.record.to_string(),
            second: port.record.to_string(),
        });
    }
    Ok(())
}

fn same_record(a: &PortDef, b: &PortDef) -> bool {
    (a.id, a.max_len, a.alignment, a.depth) == (b.id, b.max_len, b.alignment, b.depth)
        && a.schema == b.schema
}

/// Builds one entry, binding the system's bundles from the rings' handles.
fn entry<S: System + 'static>(
    make: impl Fn(Params<'_>) -> Result<(S, S::State), ParamError> + 'static,
    doc: Cow<'static, str>,
    schema: Option<Box<RawValue>>,
) -> Result<TableEntry, DefError> {
    Ok(TableEntry {
        def: S::def(&DefCx::empty())?,
        doc,
        schema,
        make: Box::new(move |cx| {
            if cx.thread != DEFAULT_THREAD {
                return Err(ParamError::Thread {
                    thread: cx.thread.to_string(),
                });
            }
            let (system, state) = make(cx.params)?;
            let views = cx
                .def
                .inputs
                .iter()
                .zip(cx.inputs)
                .map(|(def, rings)| InputBinding {
                    def: def.clone(),
                    views: rings
                        .into_iter()
                        // PANIC Safety: the build pass counts one reader slot per edge.
                        .map(|ring| ring.view(NoWake).expect("a counted reader slot"))
                        .collect(),
                })
                .collect();
            let writers = cx
                .def
                .outputs
                .iter()
                .zip(cx.outputs)
                .map(|(def, ring)| OutputBinding {
                    def: def.clone(),
                    // PANIC Safety: one ring is allocated per output port.
                    writer: ring.writer(NoWake).expect("one writer per output ring"),
                })
                .collect();
            Ok(Box::new(Runner {
                system,
                state,
                inputs: S::Inputs::bind(views),
                outputs: S::Outputs::bind(writers),
            }))
        }),
    })
}

/// Builds one async entry, whose `make` places the system on its group's thread.
fn async_entry<A, M>(
    make: M,
    doc: Cow<'static, str>,
    schema: Option<Box<RawValue>>,
) -> Result<TableEntry, DefError>
where
    A: AsyncSystem + 'static,
    M: Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
{
    let make = std::sync::Arc::new(make);
    Ok(TableEntry {
        def: A::def(&DefCx::empty())?,
        doc,
        schema,
        make: Box::new(move |cx| crate::thread::place::<A, M>(make.clone(), cx)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Record;
    use crate::async_system::Stop;
    use crate::tests::utils::{Imu, ImuOffset, ImuSource, NavFilter};
    use crate::{MsgCodec, Output};
    use metor_proto::types::ComponentId;

    /// A type's definition under the empty context.
    fn static_def<S: System>() -> SystemDef {
        S::def(&DefCx::empty()).expect("a static definition")
    }

    fn message(name: &'static str, codec: MsgCodec) -> PortDef {
        PortDef {
            name: "out".into(),
            record: name.into(),
            id: ComponentId::new(name),
            max_len: 32,
            alignment: 1,
            depth: 1,
            schema: RecordSchema::msg(name, codec),
        }
    }

    fn declared(outputs: Vec<PortDef>) -> TableEntry {
        let mut entry = entry::<ImuSource>(|_| Ok((ImuSource, 0)), Cow::Borrowed(""), None)
            .expect("a static definition");
        entry.def.outputs = outputs;
        entry
    }

    #[test]
    fn record_lookup_includes_implicit_status_and_repeated_ports() {
        let mut table = SystemTable::new();
        assert_eq!(table.record("status"), Some(&*STATUS));
        assert!(table.record("missing").is_none());
        let first = Output::<Imu>::def("first");
        let mut second = first.clone();
        second.name = "second".into();
        table
            .insert("one", declared(vec![first.clone(), second]))
            .expect("same record");
        table
            .insert("two", declared(vec![first.clone()]))
            .expect("same record");
        assert_eq!(table.record(Imu::NAME), Some(&first));
    }

    #[test]
    fn conflicting_codecs_are_rejected_without_adding_the_entry() {
        let mut table = SystemTable::new();
        let first = message("command", MsgCodec::Json);
        table
            .insert("json", declared(vec![first.clone()]))
            .expect("first record");
        assert_eq!(
            table.insert("bytes", declared(vec![message("command", MsgCodec::Bytes)])),
            Err(BuildError::RecordConflict {
                record: "command".into()
            })
        );
        assert!(table.get("bytes").is_none());
        assert_eq!(table.record("command"), Some(&first));
    }

    #[test]
    fn inputs_and_frame_schemas_are_checked_during_registration() {
        let mut table = SystemTable::new();
        let first = Output::<Imu>::def("imu");
        table
            .insert("source", declared(vec![first.clone()]))
            .expect("first record");
        let mut changed = first;
        let RecordSchema::Frame { metadata, .. } = &mut changed.schema else {
            panic!("frame")
        };
        metadata[0].name = "imu.other".into();
        let mut input = declared(Vec::new());
        input.def.inputs.push(changed);
        assert_eq!(
            table.insert("sink", input),
            Err(BuildError::RecordConflict {
                record: "imu".into()
            })
        );
        assert!(table.get("sink").is_none());
    }

    #[test]
    fn conflicts_within_an_entry_leave_no_partial_registration() {
        let mut table = SystemTable::new();
        let ports = vec![
            message("unrelated", MsgCodec::Json),
            message("command", MsgCodec::Json),
            message("command", MsgCodec::Bytes),
        ];
        assert!(matches!(
            table.insert("bad", declared(ports)),
            Err(BuildError::RecordConflict { .. })
        ));
        assert_eq!(table.entries().count(), 0);
        assert!(table.record("unrelated").is_none());
        assert!(table.record("command").is_none());
    }

    #[test]
    fn pack_registration_stops_at_the_first_conflict() {
        let mut table = SystemTable::new();
        let original = message("command", MsgCodec::Json);
        table
            .insert("existing", declared(vec![original.clone()]))
            .expect("first");
        let pack = vec![
            (
                "new".into(),
                declared(vec![message("new", MsgCodec::Bytes)]),
            ),
            (
                "bad".into(),
                declared(vec![message("command", MsgCodec::Bytes)]),
            ),
            (
                "later".into(),
                declared(vec![message("later", MsgCodec::Json)]),
            ),
        ];
        assert!(matches!(
            table.insert_all(pack),
            Err(BuildError::RecordConflict { .. })
        ));
        assert!(table.get("new").is_some());
        assert!(table.get("bad").is_none());
        assert!(table.get("later").is_none());
        assert_eq!(table.record("command"), Some(&original));
        assert!(table.record("new").is_some());
    }

    #[test]
    fn pack_registration_replaces_same_named_entries_in_order() {
        let mut table = SystemTable::new();
        let pack = vec![
            (
                "same".into(),
                declared(vec![message("first", MsgCodec::Json)]),
            ),
            (
                "same".into(),
                declared(vec![message("second", MsgCodec::Bytes)]),
            ),
        ];
        table.insert_all(pack).expect("compatible replacement");
        assert_eq!(table.entries().count(), 1);
        assert!(table.record("first").is_none());
        assert!(table.record("second").is_some());
    }

    #[test]
    fn pack_replacements_cannot_conflict_with_existing_records() {
        let mut table = SystemTable::new();
        let old = message("command", MsgCodec::Json);
        for name in ["one", "two"] {
            table
                .insert(name, declared(vec![old.clone()]))
                .expect("initial");
        }
        let new = message("command", MsgCodec::Bytes);
        let pack = ["one", "two"]
            .into_iter()
            .map(|name| (name.to_string(), declared(vec![new.clone()])))
            .collect();
        assert!(matches!(
            table.insert_all(pack),
            Err(BuildError::RecordConflict { .. })
        ));
        assert_eq!(table.record("command"), Some(&old));
        assert_eq!(
            table.entries().map(|(name, _)| name).collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn distinct_record_names_cannot_share_a_record_id() {
        let mut table = SystemTable::new();
        let first = message("first", MsgCodec::Json);
        let mut second = message("second", MsgCodec::Json);
        second.id = first.id;
        table
            .insert("first", declared(vec![first]))
            .expect("first record");
        assert!(matches!(
            table.insert("second", declared(vec![second])),
            Err(BuildError::RecordIdConflict { .. })
        ));
    }

    #[test]
    fn incompatible_wire_ids_are_rejected_during_registration() {
        let mut table = SystemTable::new();
        let first = message("command_3", MsgCodec::Json);
        let second = message("command_121", MsgCodec::Bytes);
        assert_eq!(first.schema.packet_id(), second.schema.packet_id());
        let id = first.schema.packet_id();
        table
            .insert("first", declared(vec![first]))
            .expect("first record");
        assert_eq!(
            table.insert("second", declared(vec![second])),
            Err(BuildError::MessageIdConflict {
                id,
                first: "command_3".into(),
                second: "command_121".into(),
            })
        );
    }

    #[test]
    fn the_implicit_status_definition_cannot_be_replaced() {
        let mut table = SystemTable::new();
        let mut status = STATUS.clone();
        status.schema = RecordSchema::msg("status", MsgCodec::Bytes);
        assert!(matches!(
            table.insert("bad", declared(vec![status])),
            Err(BuildError::RecordConflict { .. })
        ));
    }

    #[test]
    fn replacement_removes_obsolete_records_and_keeps_shared_ones() {
        let mut table = SystemTable::new();
        let old = message("old", MsgCodec::Bytes);
        let shared = message("shared", MsgCodec::Json);
        table
            .insert("one", declared(vec![old, shared.clone()]))
            .expect("first");
        table
            .insert("two", declared(vec![shared.clone()]))
            .expect("shared");
        let new = message("new", MsgCodec::Bytes);
        table
            .insert("one", declared(vec![new.clone()]))
            .expect("replacement");
        assert!(table.record("old").is_none());
        assert_eq!(table.record("shared"), Some(&shared));
        assert_eq!(table.record("new"), Some(&new));
        assert_eq!(
            table.entries().map(|(name, _)| name).collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn replacement_checks_the_retained_entries_and_preserves_the_original_on_failure() {
        let mut table = SystemTable::new();
        let json = message("command", MsgCodec::Json);
        let bytes = message("command", MsgCodec::Bytes);
        table
            .insert("one", declared(vec![json.clone()]))
            .expect("first");
        table
            .insert("one", declared(vec![bytes.clone()]))
            .expect("sole definition can change");
        table
            .insert("two", declared(vec![bytes.clone()]))
            .expect("shared definition");
        assert!(matches!(
            table.insert("one", declared(vec![json])),
            Err(BuildError::RecordConflict { .. })
        ));
        assert_eq!(
            table.get("one").expect("original entry").def.outputs,
            [bytes]
        );
    }

    #[test]
    fn register_records_the_definition() {
        let mut table = SystemTable::new();
        table
            .register_system("imu", |_| Ok((ImuSource, 0)))
            .expect("valid records");
        let entry = table.get("imu").expect("registered");
        assert_eq!(entry.def.outputs[0].id, Imu::ID);
    }

    #[test]
    fn registering_a_type_twice_replaces_it() {
        let mut table = SystemTable::new();
        table
            .register_system("shared", |_| Ok((ImuSource, 0)))
            .expect("valid records");
        table
            .register_system("shared", |_| Ok((NavFilter, ())))
            .expect("valid records");
        let entry = table.get("shared").expect("registered");
        assert_eq!(entry.def.name, static_def::<NavFilter>().name);
    }

    #[test]
    fn an_unregistered_type_is_absent() {
        let table = SystemTable::new();
        assert!(table.get("imu").is_none());
        assert_eq!(table.entries().count(), 0);
    }

    #[test]
    fn entries_keep_registration_order_across_a_replacement() {
        let mut table = SystemTable::new();
        table
            .register_system("imu", |_| Ok((ImuSource, 0)))
            .expect("valid records");
        table
            .register_system("nav", |_| Ok((NavFilter, ())))
            .expect("valid records");
        table
            .register_system("imu", |_| Ok((ImuOffset, 0)))
            .expect("valid records");
        let seen: Vec<_> = table.entries().map(|(ty, _)| ty).collect();
        assert_eq!(seen, vec!["imu", "nav"]);
        assert_eq!(
            table.get("imu").expect("registered").def.name,
            static_def::<ImuOffset>().name
        );
    }

    /// An async system with no ports, which returns when its stop is set.
    struct Idle;

    impl AsyncSystem for Idle {
        type State = ();
        type Inputs = ();
        type Outputs = ();

        fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
            SystemDef::new_async::<(), ()>("idle", cx)
        }

        async fn run(&self, _state: &mut (), _in: &mut (), _out: &mut (), stop: Stop) {
            stop.wait().await;
        }
    }

    #[test]
    fn an_async_registration_keeps_its_definition_and_launches_on_its_thread() {
        let mut table = SystemTable::new();
        table
            .register_async_system("idle", |_| Ok((Idle, ())))
            .expect("valid records");
        let entry = table.get("idle").expect("registered");
        assert_eq!(entry.def.name, "idle");
        assert!(entry.def.inputs.is_empty() && entry.def.outputs.is_empty());
        let mut threads = Threads::new();
        let def = entry.def.clone();
        let step = (entry.make)(MakeCx {
            id: "idle",
            thread: DEFAULT_THREAD,
            def: &def,
            params: Params(&serde_json::Value::Null),
            inputs: Vec::new(),
            outputs: Vec::new(),
            threads: &mut threads,
        })
        .expect("no params");
        let started = std::time::Instant::now();
        drop(step);
        // A group the adapter still owned would take the whole join timeout.
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    #[test]
    fn the_trait_path_registers_no_doc_and_no_schema() {
        let mut table = SystemTable::new();
        table
            .register_system("imu", |_| Ok((ImuSource, 0)))
            .expect("valid records");
        let entry = table.get("imu").expect("registered");
        assert_eq!(entry.doc, "");
        assert!(entry.schema.is_none());
    }
}
