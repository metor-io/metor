//! Computes system definitions on demand across a deployment's targets.

use std::collections::HashMap;

use super::{DeploySource, DeploySystem, DeploymentConfig, PeerPort};
use crate::coordinator::{OutputSource, PortRef, SystemConfig, SystemTable, TableEntry};
use crate::def::Records;
use crate::system::{PortDef, SystemDef};

use super::error::DeployError;

/// The location of a system: the index of its target, and its index within
/// that target.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct SystemIndex {
    pub(crate) target: usize,
    pub(crate) system: usize,
}

impl SystemIndex {
    pub(crate) fn new(target: usize, system: usize) -> Self {
        Self { target, system }
    }

    /// Every system in `envelope`, in target and step order, with its index.
    pub(crate) fn iter(
        envelope: &DeploymentConfig,
    ) -> impl Iterator<Item = (SystemIndex, &DeploySystem)> {
        envelope
            .targets
            .iter()
            .enumerate()
            .flat_map(|(target, member)| {
                member
                    .coordinator
                    .systems
                    .iter()
                    .enumerate()
                    .map(move |(system, config)| (Self::new(target, system), config))
            })
    }

    pub(crate) fn get(self, envelope: &DeploymentConfig) -> &DeploySystem {
        &envelope.targets[self.target].coordinator.systems[self.system]
    }

    pub(crate) fn get_mut(self, envelope: &mut DeploymentConfig) -> &mut DeploySystem {
        &mut envelope.targets[self.target].coordinator.systems[self.system]
    }
}

/// Computes and caches system definitions across a deployment.
///
/// A system's definition can depend on the systems that feed it, including
/// systems on other targets. The index computes each definition the first
/// time something asks for it, following those references as needed. If a
/// reference leads back to a system that is still being computed, that is
/// reported as a cycle.
pub(crate) struct DefIndex<'a> {
    envelope: &'a DeploymentConfig,
    tables: &'a [SystemTable],
    records: Vec<Records>,
    memo: HashMap<SystemIndex, SystemDef>,
    visiting: Vec<SystemIndex>,
}

impl<'a> DefIndex<'a> {
    pub(crate) fn new(envelope: &'a DeploymentConfig, tables: &'a [SystemTable]) -> Self {
        Self {
            envelope,
            tables,
            records: tables.iter().map(SystemTable::records).collect(),
            memo: HashMap::new(),
            visiting: Vec::new(),
        }
    }

    /// Finds the system `link` on the target named `target`. `from` is the
    /// system making the reference, used in error messages.
    pub(crate) fn locate(
        &self,
        from: &str,
        target: &str,
        link: &str,
    ) -> Result<SystemIndex, DeployError> {
        let target_index = self
            .envelope
            .targets
            .iter()
            .position(|member| member.namespace() == target)
            .ok_or_else(|| DeployError::NoSuchTarget {
                from: from.to_string(),
                target: target.to_string(),
            })?;
        let system_index = self.envelope.targets[target_index]
            .coordinator
            .systems
            .iter()
            .position(|system| system.id == link)
            .ok_or_else(|| DeployError::NoSuchLink {
                from: from.to_string(),
                target: target.to_string(),
                link: link.to_string(),
            })?;
        Ok(SystemIndex::new(target_index, system_index))
    }

    /// Returns the name used for a system in errors: `{namespace}.{id}`.
    pub(crate) fn name(&self, index: SystemIndex) -> String {
        let namespace = self.envelope.targets[index.target].namespace();
        format!("{namespace}.{}", index.get(self.envelope).id)
    }

    /// Returns the definition of the peer input port that `peer` refers to,
    /// renamed to `port` on the system at `from`.
    pub(crate) fn peer_port(
        &mut self,
        from: SystemIndex,
        port: &str,
        peer: &PeerPort,
    ) -> Result<PortDef, DeployError> {
        let name = self.name(from);
        let peer_index = self.locate(&name, &peer.target, &peer.link)?;
        let def = self.def(peer_index)?;
        def.inputs
            .iter()
            .find(|input| input.name == peer.port)
            .map(|input| PortDef {
                name: port.to_string().into(),
                ..input.clone()
            })
            .ok_or_else(|| DeployError::NoSuchPort {
                from: name,
                target: peer.target.clone(),
                link: peer.link.clone(),
                port: peer.port.clone(),
            })
    }

    /// Returns the definition of the system at `index`, computing it on the first
    /// call.
    pub(crate) fn def(&mut self, index: SystemIndex) -> Result<SystemDef, DeployError> {
        if let Some(def) = self.memo.get(&index) {
            return Ok(def.clone());
        }
        if self.visiting.contains(&index) {
            let mut path: Vec<String> = self
                .visiting
                .iter()
                .map(|&visited| self.name(visited))
                .collect();
            path.push(self.name(index));
            return Err(DeployError::Cycle {
                path: path.join(" -> "),
            });
        }
        self.visiting.push(index);
        let def = self.compute(index);
        self.visiting.pop();
        let def = def?;
        self.memo.insert(index, def.clone());
        Ok(def)
    }

    fn compute(&mut self, index: SystemIndex) -> Result<SystemDef, DeployError> {
        let target = &self.envelope.targets[index.target];
        let entry = self.entry(index)?;
        let system = self.runtime(index)?;
        let producers = self.producers(index, &system, &entry.def.inputs)?;
        SystemDef::instance(
            &system,
            entry,
            target.coordinator.namespace.as_deref(),
            &self.records[index.target],
            |_, from| Ok(producers.get(from).cloned().flatten()),
        )
        .map_err(|source| self.build_error(index, source))
    }

    /// Returns the records known to the system table of target `target`.
    pub(crate) fn records(&self, target: usize) -> &Records {
        &self.records[target]
    }

    /// Returns every edge the type of the system at `index` sees, with the
    /// definition of the output that feeds it.
    pub(crate) fn edges(
        &mut self,
        index: SystemIndex,
    ) -> Result<Vec<(String, PortDef)>, DeployError> {
        let system = self.runtime(index)?;
        let producers = self.producers(index, &system, &[])?;
        let edges = system
            .edges(&[], |_, from| Ok(producers.get(from).cloned().flatten()))
            .map_err(|source| self.build_error(index, source))?;
        let owned = edges.into_iter().map(|(port, def)| (port.to_string(), def));
        Ok(owned.collect())
    }

    /// Returns the table entry for the type of the system at `index`.
    fn entry(&self, index: SystemIndex) -> Result<&'a TableEntry, DeployError> {
        let system = index.get(self.envelope);
        self.tables[index.target].get(&system.ty).ok_or_else(|| {
            self.build_error(
                index,
                crate::BuildError::UnknownType {
                    id: system.id.clone(),
                    ty: system.ty.clone(),
                },
            )
        })
    }

    /// Returns the runtime form of the system at `index`, with every peer
    /// output replaced by its definition.
    pub(crate) fn runtime(&mut self, index: SystemIndex) -> Result<SystemConfig, DeployError> {
        let system = index.get(self.envelope).clone();
        system.try_map_outputs(|port, source| match source {
            DeploySource::Runtime(source) => Ok(source),
            DeploySource::Peer(peer) => self.peer_port(index, port, &peer).map(OutputSource::Def),
        })
    }

    /// Returns the definition feeding each input edge.
    ///
    /// Only ports outside `declared` need their producer's definition.
    /// Skipping declared ports keeps a `loop()` edge from being reported as a
    /// cycle.
    fn producers(
        &mut self,
        index: SystemIndex,
        system: &SystemConfig,
        declared: &[PortDef],
    ) -> Result<HashMap<PortRef, Option<PortDef>>, DeployError> {
        let mut producers = HashMap::new();
        for input in &system.inputs {
            for from in &input.from {
                let def = if declared.iter().any(|p| p.name == input.port) {
                    None
                } else {
                    Some(self.output(index, from)?)
                };
                producers.insert(from.clone(), def);
            }
        }
        Ok(producers)
    }

    /// Returns the definition of an output port on another system in the same
    /// target.
    fn output(&mut self, index: SystemIndex, from: &PortRef) -> Result<PortDef, DeployError> {
        let target = &self.envelope.targets[index.target];
        let Some(system_index) = target
            .coordinator
            .systems
            .iter()
            .position(|system| system.id == from.system)
        else {
            return Err(self.build_error(
                index,
                crate::BuildError::UnknownSystem {
                    id: index.get(self.envelope).id.clone(),
                    port: from.port.clone(),
                    from: from.system.clone(),
                },
            ));
        };
        let def = self.def(SystemIndex::new(index.target, system_index))?;
        def.outputs
            .iter()
            .find(|port| port.name == from.port)
            .cloned()
            .ok_or_else(|| {
                self.build_error(
                    index,
                    crate::BuildError::UnknownOutput {
                        system: from.system.clone(),
                        port: from.port.clone(),
                    },
                )
            })
    }

    fn build_error(&self, index: SystemIndex, source: crate::BuildError) -> DeployError {
        DeployError::Build {
            target: self.envelope.targets[index.target].namespace().to_string(),
            source,
        }
    }
}
