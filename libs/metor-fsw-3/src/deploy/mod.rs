//! Resolves the references between targets in a deployment.
//!
//! A target's config can refer to its peers: a link can dial another
//! target's link by name, and an output port can take its definition from a
//! peer's port. `metor build` replaces each of these references with the
//! concrete address or port definition it stands for. After that, each target
//! can be built from its own packs, without knowing about the others.

mod config;
mod error;
mod index;
mod part;
mod slot;

use std::collections::HashMap;

use crate::coordinator::{OutputSource, PeerLink, SystemConfig, SystemTable};
use crate::link::{LINK_TYPES, LinkParams, Transport};

pub use config::{CONFIG_VERSION, ConfigError, DeploymentConfig, TargetConfig};
pub use error::DeployError;
use index::{At, DefIndex};
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
    let plan = plan(envelope, tables)?;
    for (at, addr) in plan.transports {
        let system = at.get_mut(envelope);
        let mut params = Part::<LinkParams>::read(system)?;
        params.part.transport = Transport::Connect { addr };
        params.write(system)?;
    }
    for ((at, output), def) in plan.outputs {
        at.get_mut(envelope).outputs[output].source = OutputSource::Def(def);
    }
    check_ports(envelope)
}

/// Returns true if `envelope` still has a peer reference or a slot candidate
/// that points at a pack.
pub fn unresolved(envelope: &DeploymentConfig) -> bool {
    slot::unresolved(envelope)
        || envelope.targets.iter().any(|target| {
            target.coordinator.systems.iter().any(|system| {
                dials_peer(system)
                    || system
                        .outputs
                        .iter()
                        .any(|output| matches!(output.source, OutputSource::Peer(_)))
            })
        })
}

/// Returns true if `system` is a link that dials a peer.
///
/// Also returns true if the link's params do not decode, so that `resolve`
/// runs and reports the error.
fn dials_peer(system: &SystemConfig) -> bool {
    match link(system) {
        Ok(params) => params.is_some_and(|params| peer_link(&params.part).is_some()),
        Err(_) => true,
    }
}

struct Plan {
    transports: Vec<(At, String)>,
    outputs: Vec<((At, usize), crate::PortDef)>,
}

fn plan(envelope: &DeploymentConfig, tables: &[SystemTable]) -> Result<Plan, DeployError> {
    let mut index = DefIndex::new(envelope, tables);
    let mut plan = Plan {
        transports: Vec::new(),
        outputs: Vec::new(),
    };
    for (t, target) in envelope.targets.iter().enumerate() {
        for (s, system) in target.coordinator.systems.iter().enumerate() {
            let at = At::new(t, s);
            for (o, output) in system.outputs.iter().enumerate() {
                if let OutputSource::Peer(peer) = &output.source {
                    plan.outputs
                        .push(((at, o), index.peer_port(at, &output.port, peer)?));
                }
            }
            let Some(params) = link(system)? else {
                continue;
            };
            let Some(peer) = peer_link(&params.part) else {
                continue;
            };
            let from = index.name(at);
            let peer_at = index.locate(&from, &peer.target, &peer.link)?;
            let port =
                fixed_port(peer_at.get(envelope))?.ok_or_else(|| DeployError::NotAListener {
                    from: from.clone(),
                    target: peer.target.clone(),
                    link: peer.link.clone(),
                })?;
            plan.transports
                .push((at, address(host(envelope, &peer.target), port)));
        }
    }
    Ok(plan)
}

/// Checks that no two listeners on the same host bind the same port.
fn check_ports(envelope: &DeploymentConfig) -> Result<(), DeployError> {
    let mut bound: HashMap<(String, u16), String> = HashMap::new();
    for target in &envelope.targets {
        let host = host(envelope, target.namespace()).to_string();
        for system in &target.coordinator.systems {
            let Some(port) = fixed_port(system)? else {
                continue;
            };
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

/// Reads a link's params. Returns `None` if `system` is not a link.
fn link(system: &SystemConfig) -> Result<Option<Part<LinkParams>>, DeployError> {
    if !LINK_TYPES.contains(&system.ty.as_str()) {
        return Ok(None);
    }
    Part::read(system).map(Some)
}

/// Returns the peer a link dials, if its transport is a peer reference.
fn peer_link(params: &LinkParams) -> Option<&PeerLink> {
    match &params.transport {
        Transport::Peer(peer) => Some(peer),
        _ => None,
    }
}

/// Returns the port a system listens on, unless it is zero.
fn fixed_port(system: &SystemConfig) -> Result<Option<u16>, DeployError> {
    let port = listen_addr(system)?.and_then(|addr| addr.rsplit_once(':')?.1.parse().ok());
    Ok(port.filter(|&port| port != 0))
}

/// Returns the address a system listens on.
///
/// A link listens on its transport's `listen` address. Any other system
/// listens on its top-level `listen` param, if it has one.
fn listen_addr(system: &SystemConfig) -> Result<Option<String>, DeployError> {
    if let Some(params) = link(system)? {
        return Ok(match params.part.transport {
            Transport::Listen { addr, .. } => Some(addr),
            _ => None,
        });
    }
    let listen = system.params.get("listen").and_then(|addr| addr.as_str());
    Ok(listen.map(str::to_string))
}

#[cfg(test)]
mod tests;
