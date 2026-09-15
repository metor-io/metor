//! The registry of system types a config may name.

use std::collections::HashMap;

use metor_fsw_3_ring::{NoWake, View, Writer};

use crate::system::{System, SystemDef, SystemInputs, SystemOutputs};

use super::run::{Runner, Step};

type SystemMakeFn = dyn Fn(Vec<Vec<View<NoWake>>>, Vec<Writer<NoWake>>) -> Box<dyn Step>;

pub(crate) struct TableEntry {
    pub def: SystemDef,
    pub make: Box<SystemMakeFn>,
}

/// A registry of system definitions, and their constructors
#[derive(Default)]
pub struct SystemTable {
    entries: HashMap<String, TableEntry>,
}

impl SystemTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register S under the type `system_ty`, replacing any type registered under the same name.
    pub fn register<S: System + 'static>(
        &mut self,
        system_ty: &str,
        make: impl Fn() -> (S, S::State) + 'static,
    ) {
        let entry = TableEntry {
            def: S::def(),
            make: Box::new(move |views, writers| {
                let (system, state) = make();
                Box::new(Runner {
                    system,
                    state,
                    inputs: S::Inputs::bind(views),
                    outputs: S::Outputs::bind(writers),
                })
            }),
        };
        self.entries.insert(system_ty.to_string(), entry);
    }

    pub(crate) fn get(&self, ty: &str) -> Option<&TableEntry> {
        self.entries.get(ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Frame;
    use crate::tests::utils::{Imu, ImuSource, NavFilter};

    #[test]
    fn register_records_the_definition() {
        let mut table = SystemTable::new();
        table.register("imu", || (ImuSource, 0));
        let entry = table.get("imu").expect("registered");
        assert_eq!(entry.def.outputs[0].frame, Imu::ID);
    }

    #[test]
    fn registering_a_type_twice_replaces_it() {
        let mut table = SystemTable::new();
        table.register("shared", || (ImuSource, 0));
        table.register("shared", || (NavFilter, ()));
        let entry = table.get("shared").expect("registered");
        assert_eq!(entry.def.name, NavFilter::def().name);
    }

    #[test]
    fn an_unregistered_type_is_absent() {
        let table = SystemTable::new();
        assert!(table.get("imu").is_none());
    }
}
