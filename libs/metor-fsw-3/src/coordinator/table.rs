//! The registry of system types a config may name.

use std::collections::HashMap;

use metor_fsw_3_ring::{NoWake, View, Writer};

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
    pub make: Box<SystemMakeFn>,
}

/// A `SystemTable` maps each type name a config may use to its definition and constructor.
#[derive(Default)]
pub struct SystemTable {
    entries: HashMap<String, TableEntry>,
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
        let entry = TableEntry {
            def: S::def(),
            make: Box::new(move |params, views, writers| {
                let (system, state) = make(params)?;
                Ok(Box::new(Runner {
                    system,
                    state,
                    inputs: S::Inputs::bind(views),
                    outputs: S::Outputs::bind(writers),
                }))
            }),
        };
        self.entries.insert(ty.to_string(), entry);
    }

    pub(crate) fn get(&self, ty: &str) -> Option<&TableEntry> {
        self.entries.get(ty)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Record;
    use crate::tests::utils::{Imu, ImuSource, NavFilter};

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
    }
}
