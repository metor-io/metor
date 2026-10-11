//! Computes system definitions on demand across a deployment's targets.

use std::collections::HashMap;

use super::DeploymentConfig;
use crate::coordinator::instance_def;
use crate::coordinator::{OutputSource, PeerPort, PortRef, SystemConfig, SystemTable};
use crate::def::Records;
use crate::system::{PortDef, SystemDef};

use super::error::DeployError;

/// The location of a system: the index of its target, and its index within
/// that target.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct At {
    pub(crate) target: usize,
    pub(crate) system: usize,
}

impl At {
    pub(crate) fn new(target: usize, system: usize) -> Self {
        Self { target, system }
    }

    pub(crate) fn get(self, envelope: &DeploymentConfig) -> &SystemConfig {
        &envelope.targets[self.target].coordinator.systems[self.system]
    }

    pub(crate) fn get_mut(self, envelope: &mut DeploymentConfig) -> &mut SystemConfig {
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
    memo: HashMap<At, SystemDef>,
    visiting: Vec<At>,
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
    pub(crate) fn locate(&self, from: &str, target: &str, link: &str) -> Result<At, DeployError> {
        let t = self
            .envelope
            .targets
            .iter()
            .position(|member| member.namespace() == target)
            .ok_or_else(|| DeployError::NoSuchTarget {
                from: from.to_string(),
                target: target.to_string(),
            })?;
        let s = self.envelope.targets[t]
            .coordinator
            .systems
            .iter()
            .position(|system| system.id == link)
            .ok_or_else(|| DeployError::NoSuchLink {
                from: from.to_string(),
                target: target.to_string(),
                link: link.to_string(),
            })?;
        Ok(At::new(t, s))
    }

    /// Returns the name used for a system in errors: `{namespace}.{id}`.
    pub(crate) fn name(&self, at: At) -> String {
        let namespace = self.envelope.targets[at.target].namespace();
        format!("{namespace}.{}", at.get(self.envelope).id)
    }

    /// Returns the definition of the peer input port that `peer` refers to,
    /// renamed to `port` on the system at `from`.
    pub(crate) fn peer_port(
        &mut self,
        from: At,
        port: &str,
        peer: &PeerPort,
    ) -> Result<PortDef, DeployError> {
        let name = self.name(from);
        let at = self.locate(&name, &peer.target, &peer.link)?;
        let def = self.def(at)?;
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

    /// Returns the definition of the system at `at`, computing it on the first
    /// call.
    pub(crate) fn def(&mut self, at: At) -> Result<SystemDef, DeployError> {
        if let Some(def) = self.memo.get(&at) {
            return Ok(def.clone());
        }
        if self.visiting.contains(&at) {
            let mut path: Vec<String> = self.visiting.iter().map(|&v| self.name(v)).collect();
            path.push(self.name(at));
            return Err(DeployError::Cycle {
                path: path.join(" -> "),
            });
        }
        self.visiting.push(at);
        let def = self.compute(at);
        self.visiting.pop();
        let def = def?;
        self.memo.insert(at, def.clone());
        Ok(def)
    }

    fn compute(&mut self, at: At) -> Result<SystemDef, DeployError> {
        let target = &self.envelope.targets[at.target];
        let system = at.get(self.envelope);
        let entry = self.tables[at.target].get(&system.ty).ok_or_else(|| {
            self.build_error(
                at,
                crate::BuildError::UnknownType {
                    id: system.id.clone(),
                    ty: system.ty.clone(),
                },
            )
        })?;
        let system = self.resolved_outputs(at, system)?;
        let producers = self.producers(at, &system, &entry.def)?;
        instance_def(
            &system,
            entry,
            target.coordinator.namespace.as_deref(),
            &self.records[at.target],
            |_, from| Ok(producers.get(from).cloned().flatten()),
        )
        .map_err(|source| self.build_error(at, source))
    }

    /// Returns the records known to the system table of target `target`.
    pub(crate) fn records(&self, target: usize) -> &Records {
        &self.records[target]
    }

    /// Returns every edge into the system at `at`, with the definition of the
    /// output that feeds it. A channel's `commands` input is skipped.
    pub(crate) fn edges(&mut self, at: At) -> Result<Vec<(String, PortDef)>, DeployError> {
        let system = at.get(self.envelope);
        let mut edges = Vec::new();
        for input in &system.inputs {
            if input.port == crate::channel::COMMANDS_PORT {
                continue;
            }
            for from in &input.from {
                edges.push((input.port.clone(), self.output(at, from)?));
            }
        }
        Ok(edges)
    }

    /// Returns a copy of the system's config with every peer output replaced
    /// by its definition.
    fn resolved_outputs(
        &mut self,
        at: At,
        system: &SystemConfig,
    ) -> Result<SystemConfig, DeployError> {
        let mut system = system.clone();
        for output in &mut system.outputs {
            if let OutputSource::Peer(peer) = &output.source {
                output.source = OutputSource::Def(self.peer_port(at, &output.port, peer)?);
            }
        }
        Ok(system)
    }

    /// Returns the definition feeding each input edge.
    ///
    /// Only ports the system type does not declare need their producer's
    /// definition. Skipping declared ports keeps a `loop()` edge from being
    /// reported as a cycle.
    fn producers(
        &mut self,
        at: At,
        system: &SystemConfig,
        declared: &SystemDef,
    ) -> Result<HashMap<PortRef, Option<PortDef>>, DeployError> {
        let mut producers = HashMap::new();
        for input in &system.inputs {
            for from in &input.from {
                let def = if declared.inputs.iter().any(|p| p.name == input.port) {
                    None
                } else {
                    Some(self.output(at, from)?)
                };
                producers.insert(from.clone(), def);
            }
        }
        Ok(producers)
    }

    /// Returns the definition of an output port on another system in the same
    /// target.
    fn output(&mut self, at: At, from: &PortRef) -> Result<PortDef, DeployError> {
        let target = &self.envelope.targets[at.target];
        let Some(s) = target
            .coordinator
            .systems
            .iter()
            .position(|system| system.id == from.system)
        else {
            return Err(self.build_error(
                at,
                crate::BuildError::UnknownSystem {
                    id: at.get(self.envelope).id.clone(),
                    port: from.port.clone(),
                    from: from.system.clone(),
                },
            ));
        };
        if from.port == "status" {
            return Ok(crate::coordinator::status_def(
                &from.system,
                target.coordinator.namespace.as_deref(),
            ));
        }
        let def = self.def(At::new(at.target, s))?;
        def.outputs
            .iter()
            .find(|port| port.name == from.port)
            .cloned()
            .ok_or_else(|| {
                self.build_error(
                    at,
                    crate::BuildError::UnknownOutput {
                        system: from.system.clone(),
                        port: from.port.clone(),
                    },
                )
            })
    }

    fn build_error(&self, at: At, source: crate::BuildError) -> DeployError {
        DeployError::Build {
            target: self.envelope.targets[at.target].namespace().to_string(),
            source,
        }
    }
}
