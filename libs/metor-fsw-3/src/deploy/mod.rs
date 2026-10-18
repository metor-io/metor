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

use serde::{Deserialize, Serialize};

use crate::coordinator::{OutputSource, SystemTable};
use crate::link::{LINK_TYPES, Transport};

pub use config::{
    CONFIG_VERSION, ConfigError, DeploySource, DeploySystem, DeploymentConfig, PeerLink, PeerPort,
    ResolvedDeployment, TargetConfig,
};
pub use error::DeployError;
use index::{DefIndex, SystemIndex};
use part::Part;
pub use slot::resolve_slots;

/// The host a peer is dialed on when the deployment gives it no host.
pub const LOOPBACK: &str = "127.0.0.1";

/// Resolves every peer reference in `envelope`.
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
        let mut params = links.0.remove(&index).expect("a decoded link");
        params.part.transport = DeployTransport::Runtime(Transport::Connect { addr });
        params.write(index.get_mut(envelope))?;
    }
    for ((index, output), def) in plan.outputs {
        let source = DeploySource::Runtime(OutputSource::Def(def));
        index.get_mut(envelope).outputs[output].source = source;
    }
    Ok(())
}

/// Returns true if `envelope` still has a peer reference or a slot candidate
/// that points at a pack.
///
/// # Errors
/// Returns an error if a link's or a slot's params do not decode.
pub fn unresolved(envelope: &DeploymentConfig) -> Result<bool, DeployError> {
    let peer_output = envelope.targets.iter().any(|target| {
        target.coordinator.systems.iter().any(|system| {
            system
                .outputs
                .iter()
                .any(|output| matches!(output.source, DeploySource::Peer(_)))
        })
    });
    let dials_peer = Links::read(envelope)?.dials_peer();
    Ok(slot::unresolved(envelope)? || dials_peer || peer_output)
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

/// The params of every link in a deployment, decoded once.
struct Links(HashMap<SystemIndex, Part<LinkPart>>);

impl Links {
    /// Decodes the params of every link in `envelope`.
    fn read(envelope: &DeploymentConfig) -> Result<Self, DeployError> {
        let mut links = HashMap::new();
        for (index, system) in SystemIndex::iter(envelope) {
            if LINK_TYPES.contains(&system.ty.as_str()) {
                links.insert(index, Part::read(system)?);
            }
        }
        Ok(Self(links))
    }

    fn get(&self, index: SystemIndex) -> Option<&LinkPart> {
        self.0.get(&index).map(|params| &params.part)
    }

    /// Whether any link dials a peer.
    fn dials_peer(&self) -> bool {
        self.0
            .values()
            .any(|params| matches!(params.part.transport, DeployTransport::Peer(_)))
    }

    /// The port the link at `index` listens on, unless it is zero or the
    /// system is no listening link.
    fn fixed_port(&self, index: SystemIndex) -> Option<u16> {
        let DeployTransport::Runtime(Transport::Listen { addr, .. }) = &self.get(index)?.transport
        else {
            return None;
        };
        let port: u16 = addr.rsplit_once(':')?.1.parse().ok()?;
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
        for (output_index, output) in system.outputs.iter().enumerate() {
            if let DeploySource::Peer(peer) = &output.source {
                let def = defs.peer_port(index, &output.port, peer)?;
                plan.outputs.push(((index, output_index), def));
            }
        }
        let Some(DeployTransport::Peer(peer)) = links.get(index).map(|params| &params.transport)
        else {
            continue;
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
        plan.transports
            .push((index, address(host(envelope, &peer.target), port)));
    }
    Ok(plan)
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
            return Err(DeployError::PortTaken {
                host,
                port,
                first,
                second: name,
            });
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
