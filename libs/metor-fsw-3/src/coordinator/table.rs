//! The registry of system types a config may name.

use std::collections::HashMap;

use metor_fsw_3_ring::{NoWake, View, Writer};
use serde_json::value::RawValue;

use crate::fn_system::{Ctor, FnSystem, SystemFn};
use crate::system::{System, SystemDef, SystemInputs, SystemOutputs};

use super::params::{ParamError, Params};
use super::run::{Runner, Step};

type SystemMakeFn = dyn Fn(
    Params<'_>,
    Vec<Vec<View<NoWake>>>,
    Vec<Writer<NoWake>>,
) -> Result<Box<dyn Step>, ParamError>;

pub(crate) struct TableEntry {
    pub def: SystemDef,
    /// The doc comment on the type's `execute`, empty for the trait path.
    pub doc: &'static str,
    /// The JSON Schema of the type's params, `None` when it takes none.
    pub schema: Option<Box<RawValue>>,
    pub make: Box<SystemMakeFn>,
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
        self.insert(ty, entry::<S>(make, "", None));
    }

    /// Registers a `#[system]` type under `ty`, built by a plain `Fn() -> S` or `Fn(P) -> S`.
    pub fn register<S: SystemFn, M, C: Ctor<S, M> + 'static>(&mut self, ty: &str, ctor: C) {
        let make = move |params: Params<'_>| Ok((FnSystem::default(), ctor.make(params)?));
        self.insert(ty, entry::<FnSystem<S>>(make, S::DOC, C::schema()));
    }

    /// Adds an entry, replacing a same-named one in place so order is registration order.
    fn insert(&mut self, ty: &str, entry: TableEntry) {
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
}

/// Builds one entry, binding the system's bundles from the rings' handles.
fn entry<S: System + 'static>(
    make: impl Fn(Params<'_>) -> Result<(S, S::State), ParamError> + 'static,
    doc: &'static str,
    schema: Option<Box<RawValue>>,
) -> TableEntry {
    TableEntry {
        def: S::def(),
        doc,
        schema,
        make: Box::new(move |params, views, writers| {
            let (system, state) = make(params)?;
            Ok(Box::new(Runner {
                system,
                state,
                inputs: S::Inputs::bind(views),
                outputs: S::Outputs::bind(writers),
            }))
        }),
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

    #[test]
    fn the_trait_path_registers_no_doc_and_no_schema() {
        let mut table = SystemTable::new();
        table.register_system("imu", |_| Ok((ImuSource, 0)));
        let entry = table.get("imu").expect("registered");
        assert_eq!(entry.doc, "");
        assert!(entry.schema.is_none());
    }
}
