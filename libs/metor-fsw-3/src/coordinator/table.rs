//! The registry of system types a config may name.

use std::borrow::Cow;
use std::collections::HashMap;

use metor_fsw_3_ring::{NoWake, Notifier, RingBuffer};
use serde_json::value::RawValue;

use crate::async_system::{AsyncSystem, Running, Stop};
use crate::fn_system::{AsyncSystemFn, Ctor, FnAsyncSystem, FnSystem, SystemFn};
use crate::system::{
    InputBinding, OutputBinding, PortDef, System, SystemDef, SystemInputs, SystemOutputs,
};

use super::params::{ParamError, Params};
use super::run::{Runner, Step};

/// Builds one bound system from its params, the definition build settled on,
/// and the rings its ports sit on: one ring list per input port, in edge order,
/// and one ring per output, both in `def` order.
type SystemMakeFn = dyn Fn(
    Params<'_>,
    &SystemDef,
    Vec<Vec<&RingBuffer>>,
    Vec<&RingBuffer>,
) -> Result<Box<dyn Step>, ParamError>;

/// Builds one launchable async system from its params and the mirror rings'
/// bindings, both owned, so nothing borrowed crosses to the system's thread.
pub(crate) type AsyncMakeFn = dyn Fn(serde_json::Value, Vec<InputBinding<Notifier>>, Vec<OutputBinding>) -> Box<dyn Launch>
    + Send
    + Sync;

/// How a registered type is bound: on the cycle thread, or on its own.
pub(crate) enum Make {
    Cyclic(Box<SystemMakeFn>),
    Async(Box<AsyncMakeFn>),
}

/// A `Launch` constructs one async system on the thread that will run it.
pub trait Launch: Send {
    fn launch(self: Box<Self>, stop: Stop) -> Result<Running, ParamError>;
}

/// One async system's ingredients, owned until its thread constructs it.
struct AsyncLaunch<A: AsyncSystem, M> {
    make: std::sync::Arc<M>,
    params: serde_json::Value,
    inputs: Vec<InputBinding<Notifier>>,
    outputs: Vec<OutputBinding>,
    _a: std::marker::PhantomData<fn() -> A>,
}

impl<A, M> Launch for AsyncLaunch<A, M>
where
    A: AsyncSystem + 'static,
    M: Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
{
    fn launch(self: Box<Self>, stop: Stop) -> Result<Running, ParamError> {
        let (system, mut state) = (self.make)(Params(&self.params))?;
        let mut inputs = A::Inputs::bind(self.inputs);
        let mut outputs = A::Outputs::bind(self.outputs);
        Ok(Box::pin(async move {
            system
                .run(&mut state, &mut inputs, &mut outputs, stop)
                .await;
        }))
    }
}

pub(crate) struct TableEntry {
    pub def: SystemDef,
    /// Registered across a pack ABI, so where it runs is the pack's business.
    pub pack: bool,
    /// The doc comment on the type's `execute`, empty for the trait path.
    pub doc: Cow<'static, str>,
    /// The JSON Schema of the type's params, `None` when it takes none.
    pub schema: Option<Box<RawValue>>,
    pub make: Make,
}

/// A `SystemTable` maps each type name a config may use to its definition and constructor.
///
/// Entries keep their registration order, which the pack descriptor projects.
#[derive(Default)]
pub struct SystemTable {
    entries: Vec<(String, TableEntry)>,
    index: HashMap<String, usize>,
}

impl SystemTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a `System` under `ty`, replacing any type registered under the same name.
    pub fn register_system<S: System + 'static>(
        &mut self,
        ty: &str,
        make: impl Fn(Params<'_>) -> Result<(S, S::State), ParamError> + 'static,
    ) {
        self.insert(ty, entry::<S>(make, Cow::Borrowed(""), None));
    }

    /// Registers a `#[system]` type under `ty`, built by a plain `Fn() -> S` or `Fn(P) -> S`.
    pub fn register<S: SystemFn, M, C: Ctor<S, M> + 'static>(&mut self, ty: &str, ctor: C) {
        let make = move |params: Params<'_>| Ok((FnSystem::default(), ctor.make(params)?));
        self.insert(
            ty,
            entry::<FnSystem<S>>(make, Cow::Borrowed(S::DOC), C::schema()),
        );
    }

    /// Registers an `AsyncSystem` under `ty`, constructed on its own thread.
    pub fn register_async_system<A: AsyncSystem + 'static>(
        &mut self,
        ty: &str,
        make: impl Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
    ) {
        self.insert(ty, async_entry::<A, _>(make, Cow::Borrowed(""), None));
    }

    /// Registers a `#[system]` type with an `async run` under `ty`.
    pub fn register_async<S: AsyncSystemFn, M, C: Ctor<S, M> + Send + Sync + 'static>(
        &mut self,
        ty: &str,
        ctor: C,
    ) {
        let make = move |params: Params<'_>| Ok((FnAsyncSystem::default(), ctor.make(params)?));
        self.insert(
            ty,
            async_entry::<FnAsyncSystem<S>, _>(make, Cow::Borrowed(S::DOC), C::schema()),
        );
    }

    /// Adds an entry, replacing a same-named one in place so order is registration order.
    pub(crate) fn insert(&mut self, ty: &str, entry: TableEntry) {
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

    /// Every record a registered port declares, by name. `None` is a name two
    /// registrations define differently, which no config may resolve.
    pub(crate) fn records(&self) -> HashMap<&str, Option<&PortDef>> {
        let ports = self
            .entries()
            .flat_map(|(_, entry)| entry.def.inputs.iter().chain(&entry.def.outputs));
        let mut records: HashMap<&str, Option<&PortDef>> = HashMap::new();
        for port in ports {
            records
                .entry(&port.record)
                .and_modify(|known| {
                    if known.is_some_and(|known| !carries_alike(known, port)) {
                        *known = None;
                    }
                })
                .or_insert(Some(port));
        }
        records
    }
}

/// Whether two ports agree on everything but their names.
fn carries_alike(a: &PortDef, b: &PortDef) -> bool {
    (a.id, a.max_len, a.alignment, a.depth) == (b.id, b.max_len, b.alignment, b.depth)
}

/// Builds one entry, binding the system's bundles from the rings' handles.
fn entry<S: System + 'static>(
    make: impl Fn(Params<'_>) -> Result<(S, S::State), ParamError> + 'static,
    doc: Cow<'static, str>,
    schema: Option<Box<RawValue>>,
) -> TableEntry {
    TableEntry {
        def: S::def(),
        pack: false,
        doc,
        schema,
        make: Make::Cyclic(Box::new(move |params, def, inputs, outputs| {
            let (system, state) = make(params)?;
            let views = def
                .inputs
                .iter()
                .zip(inputs)
                .map(|(def, rings)| InputBinding {
                    def: def.clone(),
                    views: rings
                        .into_iter()
                        // PANIC Safety: the build pass counts one reader slot per edge.
                        .map(|ring| ring.view(NoWake).expect("a counted reader slot"))
                        .collect(),
                })
                .collect();
            let writers = def
                .outputs
                .iter()
                .zip(outputs)
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
        })),
    }
}

/// Builds one async entry, keeping the constructor and bindings for its thread.
fn async_entry<A, M>(make: M, doc: Cow<'static, str>, schema: Option<Box<RawValue>>) -> TableEntry
where
    A: AsyncSystem + 'static,
    M: Fn(Params<'_>) -> Result<(A, A::State), ParamError> + Send + Sync + 'static,
{
    let make = std::sync::Arc::new(make);
    TableEntry {
        def: A::def(),
        pack: false,
        doc,
        schema,
        make: Make::Async(Box::new(move |params, inputs, outputs| {
            Box::new(AsyncLaunch::<A, M> {
                make: make.clone(),
                params,
                inputs,
                outputs,
                _a: std::marker::PhantomData,
            })
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Record;
    use crate::tests::utils::{Imu, ImuOffset, ImuSource, NavFilter};

    #[test]
    fn register_records_the_definition() {
        let mut table = SystemTable::new();
        table.register_system("imu", |_| Ok((ImuSource, 0)));
        let entry = table.get("imu").expect("registered");
        assert_eq!(entry.def.outputs[0].id, Imu::ID);
    }

    #[test]
    fn registering_a_type_twice_replaces_it() {
        let mut table = SystemTable::new();
        table.register_system("shared", |_| Ok((ImuSource, 0)));
        table.register_system("shared", |_| Ok((NavFilter, ())));
        let entry = table.get("shared").expect("registered");
        assert_eq!(entry.def.name, NavFilter::def().name);
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
        table.register_system("imu", |_| Ok((ImuSource, 0)));
        table.register_system("nav", |_| Ok((NavFilter, ())));
        table.register_system("imu", |_| Ok((ImuOffset, 0)));
        let seen: Vec<_> = table.entries().map(|(ty, _)| ty).collect();
        assert_eq!(seen, vec!["imu", "nav"]);
        assert_eq!(
            table.get("imu").expect("registered").def.name,
            ImuOffset::def().name
        );
    }

    /// An async system with no ports, which returns when its stop is set.
    struct Idle;

    impl AsyncSystem for Idle {
        type State = ();
        type Inputs = ();
        type Outputs = ();

        fn def() -> SystemDef {
            SystemDef::new_async::<(), ()>("idle")
        }

        async fn run(&self, _state: &mut (), _in: &mut (), _out: &mut (), stop: Stop) {
            stop.wait().await;
        }
    }

    #[test]
    fn an_async_registration_keeps_its_definition_and_launches_on_its_thread() {
        let mut table = SystemTable::new();
        table.register_async_system("idle", |_| Ok((Idle, ())));
        let entry = table.get("idle").expect("registered");
        assert_eq!(entry.def.name, "idle");
        assert!(entry.def.inputs.is_empty() && entry.def.outputs.is_empty());
        let Make::Async(make) = &entry.make else {
            panic!("an async registration");
        };
        let launch = make(serde_json::Value::Null, Vec::new(), Vec::new());
        let (handle, stop) = crate::async_system::stop_pair();
        handle.stop();
        let running = launch.launch(stop).expect("no params");
        futures_lite::future::block_on(running);
    }

    #[test]
    fn the_trait_path_registers_no_doc_and_no_schema() {
        let mut table = SystemTable::new();
        table.register_system("imu", |_| Ok((ImuSource, 0)));
        let entry = table.get("imu").expect("registered");
        assert_eq!(entry.doc, "");
        assert!(entry.schema.is_none());
    }
}
