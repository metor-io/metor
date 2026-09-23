//! Definitions computed on demand across a deployment's targets.

use std::collections::HashMap;

use crate::cli::config::DeploymentConfig;
use crate::coordinator::{OutputSource, PeerPort, PortRef, SystemConfig, SystemTable};
use crate::def::{Records, instance_def};
use crate::system::{PortDef, SystemDef};
use crate::{DefCx, Output, SystemStatus};

use super::error::DeployError;

/// Where a system sits: its target's index and its own.
pub(crate) type At = (usize, usize);

/// A `DefIndex` computes a system's definition when a reference needs it,
/// once per system, chasing edges to producers and peer references to other
/// targets. A reference that comes back to a system being computed is a cycle.
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

    /// The system `link` of the target named `target`, for a reference made by `from`.
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
        Ok((t, s))
    }

    /// The name a reference is reported by: `{namespace}.{id}`.
    pub(crate) fn name(&self, at: At) -> String {
        let target = &self.envelope.targets[at.0];
        format!(
            "{}.{}",
            target.namespace(),
            target.coordinator.systems[at.1].id
        )
    }

    /// The definition of the input port a peer reference names, as the port
    /// `port` of the system at `from`.
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

    /// The definition of the system at `at`, computed once.
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
        let (t, s) = at;
        let target = &self.envelope.targets[t];
        let system = &target.coordinator.systems[s];
        let entry = self.tables[t]
            .get(&system.ty)
            .ok_or_else(|| DeployError::Build {
                target: target.namespace().to_string(),
                source: crate::BuildError::UnknownType {
                    id: system.id.clone(),
                    ty: system.ty.clone(),
                },
            })?;
        let system = self.resolved_outputs(at, system)?;
        let producers = self.producers(at, &system, &entry.def)?;
        instance_def(
            &system,
            entry,
            target.coordinator.namespace.as_deref(),
            &self.records[t],
            |from| Ok(producers.get(from).cloned().flatten()),
        )
        .map_err(|source| DeployError::Build {
            target: target.namespace().to_string(),
            source,
        })
    }

    /// The system's config with every peer output replaced by its definition.
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

    /// The definition feeding each edge whose port the type does not declare;
    /// a declared port needs none, which keeps a `loop()` edge from cycling.
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

    /// The definition of an output port of a system on the same target.
    fn output(&mut self, at: At, from: &PortRef) -> Result<PortDef, DeployError> {
        let target = &self.envelope.targets[at.0];
        let namespace = target.namespace().to_string();
        let Some(s) = target
            .coordinator
            .systems
            .iter()
            .position(|system| system.id == from.system)
        else {
            return Err(self.build_error(
                at,
                crate::BuildError::UnknownSystem {
                    id: target.coordinator.systems[at.1].id.clone(),
                    port: from.port.clone(),
                    from: from.system.clone(),
                },
            ));
        };
        if from.port == "status" {
            return Ok(Output::<SystemStatus>::def(&DefCx::empty(), "status"));
        }
        let def = self.def((at.0, s))?;
        def.outputs
            .iter()
            .find(|port| port.name == from.port)
            .cloned()
            .ok_or_else(|| DeployError::Build {
                target: namespace,
                source: crate::BuildError::UnknownOutput {
                    system: from.system.clone(),
                    port: from.port.clone(),
                },
            })
    }

    fn build_error(&self, at: At, source: crate::BuildError) -> DeployError {
        DeployError::Build {
            target: self.envelope.targets[at.0].namespace().to_string(),
            source,
        }
    }
}
