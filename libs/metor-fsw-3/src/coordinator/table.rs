//! The [`SystemTable`], a registry of the system types a config can use.

use std::borrow::Cow;
use std::sync::Arc;

use indexmap::IndexMap;
use metor_fsw_3_ring::{NoWake, Notifier, RingBuffer};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::RecordSchema;
use crate::async_system::AsyncSystem;
use crate::def::{DefCx, DefError, Records};
use crate::fn_system::{self, AsyncSystemFn, Ctor, FnSystem, SystemFn};
use crate::io::SharedIo;
use crate::system::{InputBinding, OutputBinding, PortDef, Ports, System, SystemDef};
use crate::thread::{DEFAULT_THREAD, LaunchCx, Launched, Threads};

use super::BuildError;
use super::SystemStatus;
use super::config::Adapter;
#[cfg(feature = "host")]
use super::config::Adapters;
use super::params::{MakeError, Params};
use super::step::Runner;
use super::step::Step;

/// The arguments a system type's constructor receives.
pub struct MakeCx<'a> {
    pub id: &'a str,
    pub adapter: Option<&'a Adapter>,
    pub def: &'a SystemDef,
    pub params: Params<'a>,
    pub inputs: Vec<Vec<&'a RingBuffer>>,
    pub outputs: Vec<&'a RingBuffer>,
    pub threads: &'a mut Threads,
    /// What the build's IO systems and io steps share.
    pub io: &'a SharedIo,
    /// The target seed every `Rng` parameter draws from.
    pub seed: u64,
    /// The target's namespace.
    pub namespace: Option<&'a str>,
    /// Every record the system table knows about.
    pub records: &'a Arc<Records>,
}

impl MakeCx<'_> {
    /// Returns the thread the system runs on.
    ///
    /// This is the thread named by the adapter, or the default thread.
    pub fn thread(&self) -> &str {
        match self.adapter {
            Some(Adapter::Thread(thread)) => thread,
            _ => DEFAULT_THREAD,
        }
    }

    /// Opens the ring handles a cyclic system needs, leaving `inputs` and
    /// `outputs` empty.
    ///
    /// That is one reader per input edge and one writer per output.
    pub(crate) fn take_bindings(&mut self) -> (Vec<InputBinding>, Vec<OutputBinding>) {
        let views = self
            .def
            .inputs
            .iter()
            .zip(core::mem::take(&mut self.inputs))
            .map(|(def, rings)| InputBinding {
                def: def.clone(),
                views: rings
                    .into_iter()
                    // PANIC Safety: the build pass counts one reader slot per edge.
                    .map(|ring| ring.view(NoWake).expect("a counted reader slot"))
                    .collect(),
            })
            .collect();
        let writers = self
            .def
            .outputs
            .iter()
            .zip(core::mem::take(&mut self.outputs))
            .map(|(def, ring)| OutputBinding {
                def: def.clone(),
                // PANIC Safety: one ring is allocated per output port.
                writer: ring.writer(NoWake).expect("one writer per output ring"),
            })
            .collect();
        (views, writers)
    }
}

/// Creates a system from a [`MakeCx`].
pub(crate) type MakeFn = dyn Fn(MakeCx<'_>) -> Result<Box<dyn Step>, MakeError>;

/// Computes the definition of one system instance from its config.
pub(crate) type DefFn = dyn Fn(&DefCx<'_>) -> Result<SystemDef, DefError>;

/// How the coordinator runs a registered system type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemKind {
    /// Executed once per cycle.
    #[default]
    Cyclic,
    /// An async system that runs on its own thread.
    Async,
}

#[cfg(feature = "host")]
impl SystemKind {
    /// The adapters a type of this kind runs behind.
    pub(crate) fn adapters(self) -> Adapters {
        match self {
            Self::Cyclic => Adapters {
                thread: false,
                ..Adapters::default()
            },
            Self::Async => Adapters::default(),
        }
    }
}

pub(crate) struct TableEntry {
    #[allow(dead_code)]
    pub kind: SystemKind,
    #[cfg(feature = "host")]
    pub adapters: Adapters,
    /// The ports the type always has, whatever the config says.
    pub def: SystemDef,
    pub def_fn: Box<DefFn>,
    /// The doc comment on the type's `execute`.
    ///
    /// Empty for types registered through the `System` or `AsyncSystem` traits.
    #[allow(dead_code)]
    pub doc: Cow<'static, str>,
    /// The JSON Schema of the type's params, or `None` if it takes no params.
    #[allow(dead_code)]
    pub schema: Option<Box<RawValue>>,
    pub make: Arc<MakeFn>,
}

/// A registry of the system types a config can use.
///
/// A config refers to each type by the name it was registered under.
#[derive(Default)]
pub struct SystemTable {
    entries: IndexMap<String, TableEntry>,
}

impl SystemTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a `System` under the name `ty`.
    ///
    /// Replaces any type already registered under that name.
    pub fn register_system<S: System + 'static>(
        &mut self,
        ty: &str,
        make: impl Fn(Params<'_>) -> Result<(S, S::State), MakeError> + 'static,
    ) -> Result<(), BuildError> {
        let entry = TableEntry::system::<S>(make, Cow::Borrowed(""), None).map_err(|source| {
            BuildError::TypeDef {
                ty: ty.to_string(),
                source,
            }
        })?;
        self.insert(ty, entry)
    }

    /// Registers a `#[system]` fn under the name `ty`.
    ///
    /// `ctor` builds its state.
    pub fn register<S: SystemFn, M, C: Ctor<S::State, M> + 'static>(
        &mut self,
        ty: &str,
        _system: S,
        ctor: C,
    ) -> Result<(), BuildError> {
        let entry =
            TableEntry::system_fn::<S, M, C>(ctor).map_err(|source| BuildError::TypeDef {
                ty: ty.to_string(),
                source,
            })?;
        self.insert(ty, entry)
    }

    /// Registers an `AsyncSystem` under the name `ty`.
    ///
    /// The system is constructed on the thread it runs on.
    pub fn register_async_system<A: AsyncSystem + 'static>(
        &mut self,
        ty: &str,
        make: impl Fn(Params<'_>) -> Result<(A, A::State), MakeError> + Send + Sync + 'static,
    ) -> Result<(), BuildError> {
        let make = move |cx: LaunchCx<'_>| {
            let (system, state) = make(cx.params)?;
            Ok(Launched::<A> {
                system,
                state,
                inputs: A::Inputs::bind(cx.inputs),
                outputs: A::Outputs::bind(cx.outputs),
            })
        };
        let entry =
            TableEntry::async_system::<A, _>(make, Cow::Borrowed(""), None).map_err(|source| {
                BuildError::TypeDef {
                    ty: ty.to_string(),
                    source,
                }
            })?;
        self.insert(ty, entry)
    }

    /// Registers an async `#[system]` fn under the name `ty`.
    ///
    /// `ctor` builds its state.
    pub fn register_async<S: AsyncSystemFn, M, C: Ctor<S::State, M> + Send + Sync + 'static>(
        &mut self,
        ty: &str,
        _system: S,
        ctor: C,
    ) -> Result<(), BuildError> {
        let make = move |params: Params<'_>| {
            let state = ctor.make(params, &crate::io::IoInit::empty())?;
            Ok((FnSystem::default(), state))
        };
        let entry =
            TableEntry::async_system::<FnSystem<S>, _>(make, Cow::Borrowed(S::DOC), C::schema())
                .map_err(|source| BuildError::TypeDef {
                    ty: ty.to_string(),
                    source,
                })?;
        self.insert(ty, entry)
    }

    /// Adds an entry if its records agree with every other entry.
    ///
    /// An entry with the same name is replaced in place, so iteration order
    /// stays the order types were first registered.
    pub(crate) fn insert(&mut self, ty: &str, entry: TableEntry) -> Result<(), BuildError> {
        entry.def.check_records()?;
        for (name, known) in &self.entries {
            if name != ty {
                entry.def.check_records_against(&known.def)?;
            }
        }
        self.entries.insert(ty.to_string(), entry);
        Ok(())
    }

    pub(crate) fn get(&self, ty: &str) -> Option<&TableEntry> {
        self.entries.get(ty)
    }

    /// Iterates over every registered type and its entry, in registration order.
    pub(crate) fn entries(&self) -> impl Iterator<Item = (&str, &TableEntry)> {
        self.entries.iter().map(|(ty, entry)| (ty.as_str(), entry))
    }
}

#[cfg(feature = "host")]
impl SystemTable {
    /// Collects every record the table's ports use.
    ///
    /// This includes the `status` port the coordinator adds.
    pub(crate) fn records(&self) -> crate::def::Records {
        let status = SystemStatus::port_def(&DefCx::empty());
        crate::def::Records::from_port_defs(
            core::iter::once(&status)
                .chain(self.entries().flat_map(|(_, entry)| entry.def.ports_iter())),
        )
    }
}

impl SystemDef {
    /// Iterates over the inputs, then the outputs.
    pub(crate) fn ports_iter(&self) -> impl Iterator<Item = &PortDef> {
        self.inputs.iter().chain(&self.outputs)
    }

    /// Checks that the ports agree on their records, with each other and with
    /// the `status` port the coordinator adds.
    fn check_records(&self) -> Result<(), BuildError> {
        let status = SystemStatus::port_def(&DefCx::empty());
        for (index, port) in self.ports_iter().enumerate() {
            status.check_compatible(port)?;
            for known in self.ports_iter().take(index) {
                known.check_compatible(port)?;
            }
        }
        Ok(())
    }

    /// Checks that the ports agree on their records with every port of `known`.
    fn check_records_against(&self, known: &SystemDef) -> Result<(), BuildError> {
        for port in self.ports_iter() {
            for known in known.ports_iter() {
                known.check_compatible(port)?;
            }
        }
        Ok(())
    }
}

impl PortDef {
    /// Checks that `self` and `port` do not define one record, record id, or
    /// message id two ways.
    fn check_compatible(&self, port: &PortDef) -> Result<(), BuildError> {
        if self.record == port.record && !self.same_record(port) {
            return Err(BuildError::RecordConflict {
                record: port.record.to_string(),
            });
        }
        if self.record != port.record && self.id == port.id {
            return Err(BuildError::RecordIdConflict {
                id: port.id,
                first: self.record.to_string(),
                second: port.record.to_string(),
            });
        }
        if let (RecordSchema::Msg { id: a, .. }, RecordSchema::Msg { id: b, .. }) =
            (&self.schema, &port.schema)
            && a == b
            && self.schema != port.schema
        {
            return Err(BuildError::MessageIdConflict {
                id: *a,
                first: self.record.to_string(),
                second: port.record.to_string(),
            });
        }
        Ok(())
    }
}

impl TableEntry {
    /// Builds the table entry for a cyclic `System`.
    ///
    /// The entry's `make` binds the system's inputs and outputs to the rings it is
    /// given.
    fn system<S: System + 'static>(
        make: impl Fn(Params<'_>) -> Result<(S, S::State), MakeError> + 'static,
        doc: Cow<'static, str>,
        schema: Option<Box<RawValue>>,
    ) -> Result<Self, DefError> {
        let def = S::def(&DefCx::empty())?;
        // Binding IO ports takes `fn_system::bind`, which this path skips.
        if !def.grants.is_empty() {
            let system = def.name.to_string();
            return Err(DefError::IoOnTraitPath { system });
        }
        Ok(Self {
            kind: SystemKind::Cyclic,
            #[cfg(feature = "host")]
            adapters: SystemKind::Cyclic.adapters(),
            def,
            def_fn: Box::new(S::def),
            doc,
            schema,
            make: Arc::new(move |mut cx| {
                let (system, state) = make(cx.params)?;
                let (views, writers) = cx.take_bindings();
                Ok(Box::new(Runner {
                    system,
                    state,
                    inputs: S::Inputs::bind(views),
                    outputs: S::Outputs::bind(writers),
                }))
            }),
        })
    }

    /// Builds the table entry for a `#[system]` fn.
    ///
    /// The entry's `make` binds the system's ports, including its queue pair,
    /// before `ctor` builds its state.
    fn system_fn<S: SystemFn, M, C: Ctor<S::State, M> + 'static>(
        ctor: C,
    ) -> Result<Self, DefError> {
        Ok(Self {
            kind: SystemKind::Cyclic,
            #[cfg(feature = "host")]
            adapters: SystemKind::Cyclic.adapters(),
            def: FnSystem::<S>::def(&DefCx::empty())?,
            def_fn: Box::new(FnSystem::<S>::def),
            doc: Cow::Borrowed(S::DOC),
            schema: C::schema(),
            make: Arc::new(move |mut cx| {
                let instance = fn_system::Instance::from_make_cx(&cx);
                let (views, writers) = cx.take_bindings();
                let (inputs, outputs, io) = fn_system::bind::<S>(views, writers, instance)?;
                let state = ctor.make_with_io(cx.id, cx.params, io.as_ref())?;
                Ok(Box::new(Runner {
                    system: FnSystem::default(),
                    state,
                    inputs,
                    outputs,
                }))
            }),
        })
    }

    /// Builds the table entry for an async system.
    ///
    /// The entry's `make` places the system on the thread its adapter asks for, or on
    /// the default thread.
    fn async_system<A, M>(
        make: M,
        doc: Cow<'static, str>,
        schema: Option<Box<RawValue>>,
    ) -> Result<Self, DefError>
    where
        A: AsyncSystem + 'static,
        M: Fn(LaunchCx<'_>) -> Result<Launched<A>, MakeError> + Send + Sync + 'static,
    {
        let make = Arc::new(make);
        Ok(Self {
            kind: SystemKind::Async,
            #[cfg(feature = "host")]
            adapters: SystemKind::Async.adapters(),
            def: A::def(&DefCx::empty())?,
            def_fn: Box::new(A::def),
            doc,
            schema,
            make: Arc::new(move |cx| crate::thread::place::<A, M>(make.clone(), cx)),
        })
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use crate::Record;
    use crate::async_system::Stop;
    use crate::tests::utils::{Imu, ImuPair, Trap, imu_offset, imu_source, nav_filter};
    use crate::{MsgCodec, Output};
    use metor_proto::types::ComponentId;

    /// Looks up the record `name` in the table and returns it on a port named `out`.
    fn record(table: &SystemTable, name: &str) -> Option<PortDef> {
        table.records().port_def("out", name).ok()
    }

    fn out(port: &PortDef) -> PortDef {
        PortDef {
            name: "out".into(),
            ..port.clone()
        }
    }

    fn message(name: &'static str, codec: MsgCodec) -> PortDef {
        PortDef {
            name: "out".into(),
            record: name.into(),
            id: ComponentId::new(name),
            max_len: 32,
            alignment: 1,
            depth: 1,
            layout: None,
            schema: RecordSchema::msg(name, codec),
        }
    }

    fn declared(outputs: Vec<PortDef>) -> TableEntry {
        let mut entry = TableEntry::system::<Trap>(|_| Ok((Trap, ())), Cow::Borrowed(""), None)
            .expect("a static definition");
        entry.def.outputs = outputs;
        entry
    }

    #[test]
    fn test_record_lookup() {
        let mut table = SystemTable::new();
        assert_eq!(
            record(&table, "status"),
            Some(out(&SystemStatus::port_def(&DefCx::empty())))
        );
        assert!(record(&table, "missing").is_none());
        let first = Output::<Imu>::def(&DefCx::empty(), "first");
        let mut second = first.clone();
        second.name = "second".into();
        table
            .insert("one", declared(vec![first.clone(), second]))
            .expect("same record");
        table
            .insert("two", declared(vec![first.clone()]))
            .expect("same record");
        assert_eq!(record(&table, Imu::NAME), Some(out(&first)));
    }

    #[test]
    fn test_reject_conflicting_codecs() {
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
        assert_eq!(record(&table, "command"), Some(first.clone()));
    }

    #[test]
    fn test_reject_record_layout_conflict() {
        let mut table = SystemTable::new();
        let first = Output::<Imu>::def(&DefCx::empty(), "imu");
        table
            .insert("source", declared(vec![first.clone()]))
            .expect("first record");
        let mut changed = first;
        changed.max_len += 8;
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

    /// Two frames with the same name but different fields conflict.
    ///
    /// This holds whichever ports they are on.
    #[test]
    fn test_reject_conflicting_frame_layout() {
        let mut table = SystemTable::new();
        table
            .insert(
                "source",
                declared(vec![Output::<Imu>::def(&DefCx::empty(), "imu")]),
            )
            .expect("first record");
        let mut input = declared(Vec::new());
        input
            .def
            .inputs
            .push(crate::Input::<ImuPair>::def(&DefCx::empty(), "other"));
        assert_eq!(
            table.insert("sink", input),
            Err(BuildError::RecordConflict {
                record: "imu".into()
            })
        );
        assert!(table.get("sink").is_none());
    }

    /// Two ports carrying the same frame agree on the record.
    ///
    /// Each port gets its own vtable, so they announce different names.
    #[test]
    fn test_accept_frame_schema_per_port() {
        let mut table = SystemTable::new();
        let first = Output::<Imu>::def(&DefCx::empty(), "imu");
        table
            .insert("source", declared(vec![first.clone()]))
            .expect("first record");
        let mut input = declared(Vec::new());
        input
            .def
            .inputs
            .push(crate::Input::<Imu>::def(&DefCx::empty(), "other"));
        assert_ne!(input.def.inputs[0].schema, first.schema);
        table.insert("sink", input).expect("one record, two roots");
        assert!(table.get("sink").is_some());
    }

    #[test]
    fn test_registration_conflict_rollback() {
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
        assert!(record(&table, "unrelated").is_none());
        assert!(record(&table, "command").is_none());
    }

    #[test]
    fn test_reject_duplicate_record_id() {
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
    fn test_reject_incompatible_wire_ids() {
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
    fn test_reject_status_replacement() {
        let mut table = SystemTable::new();
        let mut status = SystemStatus::port_def(&DefCx::empty());
        status.schema = RecordSchema::msg("status", MsgCodec::Bytes);
        assert!(matches!(
            table.insert("bad", declared(vec![status])),
            Err(BuildError::RecordConflict { .. })
        ));
    }

    #[test]
    fn test_replacement_record_cleanup() {
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
        assert!(record(&table, "old").is_none());
        assert_eq!(record(&table, "shared"), Some(shared.clone()));
        assert_eq!(record(&table, "new"), Some(new.clone()));
        assert_eq!(
            table.entries().map(|(name, _)| name).collect::<Vec<_>>(),
            ["one", "two"]
        );
    }

    #[test]
    fn test_replacement_conflict_rollback() {
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
    fn test_register_definition() {
        let mut table = SystemTable::new();
        table
            .register("imu", imu_source(), i64::default)
            .expect("valid records");
        let entry = table.get("imu").expect("registered");
        assert_eq!(entry.def.outputs[0].id, Imu::ID);
    }

    #[test]
    fn test_unregistered_type_lookup() {
        let table = SystemTable::new();
        assert!(table.get("imu").is_none());
        assert_eq!(table.entries().count(), 0);
    }

    #[test]
    fn test_replacement_preserves_order() {
        let mut table = SystemTable::new();
        let mut register = || -> Result<(), BuildError> {
            table.register("imu", imu_source(), i64::default)?;
            table.register("nav", nav_filter(), Default::default)?;
            table.register("imu", imu_offset(), i64::default)
        };
        register().expect("valid records");
        let seen: Vec<_> = table.entries().map(|(ty, _)| ty).collect();
        assert_eq!(seen, vec!["imu", "nav"]);
        assert_eq!(table.get("imu").expect("registered").def.name, "imu_offset");
    }

    /// An async system with no ports. It returns once it is told to stop.
    struct Idle;

    impl AsyncSystem for Idle {
        type State = ();
        type Inputs = ();
        type Outputs = ();

        fn def(cx: &DefCx<'_>) -> Result<SystemDef, DefError> {
            SystemDef::new::<(), (), metor_fsw_3_ring::Notifier>("idle", cx)
        }

        async fn run(&self, _state: &mut (), _in: &mut (), _out: &mut (), stop: Stop) {
            stop.wait().await;
        }
    }

    #[cfg(all(feature = "threads", not(target_family = "wasm")))]
    #[test]
    fn test_async_registration_and_launch() {
        let mut table = SystemTable::new();
        table
            .register_async_system("idle", |_| Ok((Idle, ())))
            .expect("valid records");
        let entry = table.get("idle").expect("registered");
        assert_eq!(entry.def.name, "idle");
        assert!(entry.def.inputs.is_empty() && entry.def.outputs.is_empty());
        let mut threads = Threads::default();
        let def = entry.def.clone();
        let step = (entry.make)(MakeCx {
            id: "idle",
            adapter: None,
            def: &def,
            params: Params(&serde_json::Value::Null),
            inputs: Vec::new(),
            outputs: Vec::new(),
            threads: &mut threads,
            io: &crate::io::SharedIo::default(),
            seed: 0,
            namespace: None,
            records: &Default::default(),
        })
        .expect("no params");
        let started = std::time::Instant::now();
        drop(step);
        // If the adapter still owned the thread group, the drop would wait out
        // the whole join timeout.
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    #[test]
    fn test_trait_registration_metadata() {
        let mut table = SystemTable::new();
        table
            .register_system("trap", |_| Ok((Trap, ())))
            .expect("valid records");
        let entry = table.get("trap").expect("registered");
        assert_eq!(entry.doc, "");
        assert!(entry.schema.is_none());
    }
}
