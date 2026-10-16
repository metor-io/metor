//! Resolves the references between targets in a deployment.
//!
//! A deployment's config can refer to a target's peers: a link can dial
//! another target's link by name, and an output port can take its definition
//! from a peer's port. `metor build` replaces each of these references with
//! the concrete address or port definition it stands for. After that, each
//! target can be built from its own packs, without knowing about the others.

mod config;
mod error;
mod index;
mod part;
mod slot;

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::coordinator::{OutputSource, SystemTable};
use crate::link::{LINK_TYPES, Transport};

pub use config::{
    CONFIG_VERSION, ConfigError, DeploySource, DeploySystem, DeploymentConfig, PeerLink, PeerPort,
    ResolvedDeployment, TargetConfig,
};
pub use error::DeployError;
use index::{DefIndex, SystemIndex};
use part::Part;
use slot::DeploySlot;

/// The host a peer is dialed on when the deployment gives it no host.
pub const LOOPBACK: &str = "127.0.0.1";

/// A system's params, as the deployment passes read them.
enum DeployParams {
    Link(Part<LinkPart>),
    Slot(DeploySlot),
    Other,
}

impl DeployParams {
    /// Decodes the params of `system`, at `index`, by its type.
    fn read(
        envelope: &DeploymentConfig,
        index: SystemIndex,
        system: &DeploySystem,
    ) -> Result<Self, DeployError> {
        match system.ty.as_str() {
            ty if LINK_TYPES.contains(&ty) => Part::read(system).map(Self::Link),
            SLOT_TYPE => DeploySlot::read(envelope, index, system).map(Self::Slot),
            _ => Ok(Self::Other),
        }
    }
}

/// What a deployment still has to resolve: its links' params, and the slots
/// whose candidates point at packs, each decoded once.
pub struct Pending {
    links: Links,
    slots: Vec<(SystemIndex, DeploySlot)>,
}

/// Decodes every system's params in `envelope` once, and returns them if a
/// peer reference or a slot candidate that points at a pack is left.
///
/// `tables` holds one system table per target. Peer transports become
/// addresses, and peer outputs become port definitions. An envelope with
/// nothing to resolve is left unchanged.
pub fn resolve(envelope: &mut DeploymentConfig, tables: &[SystemTable]) -> Result<(), DeployError> {
    let mut links = Links::read(envelope)?;
    let plan = plan(envelope, tables, &links)?;
    check_ports(envelope, &links)?;
    for (index, addr) in plan.transports {
        // PANIC Safety: `plan` only names systems that `links` decoded.
        let mut params = links.links.remove(&index).expect("a decoded link");
        params.part.transport = DeployTransport::Runtime(Transport::Connect { addr });
        params.write(index.get_mut(envelope))?;
    }
    for ((index, output), def) in plan.outputs {
        let source = DeploySource::Runtime(OutputSource::Def(def));
        index.get_mut(envelope).outputs[output].source = source;
    }
    Ok(())
}

/// A link's transport as a deployment writes it: a runtime transport, or
/// another link to dial.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeployTransport {
    Peer(PeerLink),
    #[serde(untagged)]
    Runtime(Transport),
}

/// The part of a link's params that a deployment resolves.
#[derive(Serialize, Deserialize)]
struct LinkPart {
    transport: DeployTransport,
}

/// The params of every link in a deployment, and the address each system
/// listens on, decoded once.
struct Links {
    links: HashMap<SystemIndex, Part<LinkPart>>,
    listen: HashMap<SystemIndex, String>,
}

impl Links {
    /// Decodes the params of every link in `envelope`.
    ///
    /// A link listens on its transport's `listen` address. Any other system
    /// listens on its top-level `listen` param, if it has one.
    fn read(envelope: &DeploymentConfig) -> Result<Self, DeployError> {
        let (mut links, mut listen) = (HashMap::new(), HashMap::new());
        for (index, system) in SystemIndex::iter(envelope) {
            if LINK_TYPES.contains(&system.ty.as_str()) {
                let params = Part::<LinkPart>::read(system)?;
                if let DeployTransport::Runtime(Transport::Listen { addr, .. }) =
                    &params.part.transport
                {
                    listen.insert(index, addr.clone());
                }
                links.insert(index, params);
            } else if let Some(addr) = system.params.get("listen").and_then(Value::as_str) {
                listen.insert(index, addr.to_string());
            }
        }
        Ok(Self { links, listen })
    }

    fn get(&self, index: SystemIndex) -> Option<&LinkPart> {
        self.links.get(&index).map(|params| &params.part)
    }

    /// Whether any link dials a peer.
    fn dials_peer(&self) -> bool {
        self.links
            .values()
            .any(|params| matches!(params.part.transport, DeployTransport::Peer(_)))
    }

    /// The port the system at `index` listens on, unless it is zero or the
    /// system does not listen.
    fn fixed_port(&self, index: SystemIndex) -> Option<u16> {
        let port: u16 = self.listen.get(&index)?.rsplit_once(':')?.1.parse().ok()?;
        (port != 0).then_some(port)
    }
}

struct Plan {
    transports: Vec<(SystemIndex, String)>,
    outputs: Vec<((SystemIndex, usize), crate::PortDef)>,
}

fn plan(
    envelope: &DeploymentConfig,
    tables: &[SystemTable],
    links: &Links,
) -> Result<Plan, DeployError> {
    let mut defs = DefIndex::new(envelope, tables);
    let mut plan = Plan {
        transports: Vec::new(),
        outputs: Vec::new(),
    };
    for (index, system) in SystemIndex::iter(envelope) {
        plan.add(envelope, &mut defs, links, index)
            .map_err(|e| e.at(system.location.as_ref()))?;
    }
    Ok(plan)
}

impl Plan {
    /// Plans the peer outputs and the peer transport of the system at
    /// `index`.
    fn add(
        &mut self,
        envelope: &DeploymentConfig,
        defs: &mut DefIndex<'_>,
        links: &Links,
        index: SystemIndex,
    ) -> Result<(), DeployError> {
        let system = index.get(envelope);
        for (output_index, output) in system.outputs.iter().enumerate() {
            if let DeploySource::Peer(peer) = &output.source {
                let def = defs.peer_port(index, &output.port, peer)?;
                self.outputs.push(((index, output_index), def));
            }
        }
        let Some(DeployTransport::Peer(peer)) = links.get(index).map(|params| &params.transport)
        else {
            return Ok(());
        };
        let from = defs.name(index);
        let peer_index = defs.locate(&from, &peer.target, &peer.link)?;
        let port = links
            .fixed_port(peer_index)
            .ok_or_else(|| DeployError::NotAListener {
                from: from.clone(),
                target: peer.target.clone(),
                link: peer.link.clone(),
            })?;
        self.transports
            .push((index, address(host(envelope, &peer.target), port)));
        Ok(())
    }
}

/// Checks that no two listeners on the same host bind the same port.
fn check_ports(envelope: &DeploymentConfig, links: &Links) -> Result<(), DeployError> {
    let mut bound: HashMap<(String, u16), String> = HashMap::new();
    for (index, system) in SystemIndex::iter(envelope) {
        let Some(port) = links.fixed_port(index) else {
            continue;
        };
        let target = &envelope.targets[index.target];
        let host = host(envelope, target.namespace()).to_string();
        let name = format!("{}.{}", target.namespace(), system.id);
        if let Some(first) = bound.insert((host.clone(), port), name.clone()) {
            let taken = DeployError::PortTaken {
                host,
                port,
                first,
                second: name,
            };
            return Err(taken.at(system.location.as_ref()));
        }
    }
    Ok(())
}

fn host<'a>(envelope: &'a DeploymentConfig, target: &str) -> &'a str {
    envelope.hosts.get(target).map_or(LOOPBACK, String::as_str)
}

/// Formats `host:port`, wrapping an IPv6 host in brackets.
fn address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests;
