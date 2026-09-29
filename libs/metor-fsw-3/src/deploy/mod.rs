//! The build pass over a deployment: every reference to a peer becomes the
//! address or the definition it stands for, so a target then builds from its
//! own packs alone.

mod config;
mod error;
mod index;
mod part;
mod slot;

use std::collections::HashMap;

use metor_proto::types::PacketId;

use crate::coordinator::{OutputSource, PeerLink, SystemConfig, SystemTable};
use crate::link::{DB_TYPE, DbParams, LINK_TYPES, LinkParams, Transport};
use crate::record::RecordSchema;

pub use config::{CONFIG_VERSION, ConfigError, DeploymentConfig, TargetConfig};
pub use error::DeployError;
use index::{At, DefIndex};
use part::Part;
pub use slot::resolve_slots;

/// Default peer host, for a target the deployment places nowhere.
pub const LOOPBACK: &str = "127.0.0.1";

/// Resolves every peer reference in `envelope` against `tables`, one per
/// target: peer transports become addresses, peer outputs become
/// definitions, and each database learns the commands its subscribers take.
/// An envelope with nothing to resolve is left as it is.
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
    for (at, ids) in plan.commands {
        let system = at.get_mut(envelope);
        let mut params = Part::<DbParams>::read(system)?;
        params.part.command_ids = ids;
        params.write(system)?;
    }
    check_ports(envelope)
}

/// Whether anything in `envelope` still refers to a peer or a slot
/// candidate's pack.
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

/// Whether `system` is a link that dials a peer, or one whose params do not
/// decode, which `resolve` then reports.
fn dials_peer(system: &SystemConfig) -> bool {
    match link(system) {
        Ok(params) => params.is_some_and(|params| peer_link(&params.part).is_some()),
        Err(_) => true,
    }
}

struct Plan {
    transports: Vec<(At, String)>,
    outputs: Vec<((At, usize), crate::PortDef)>,
    commands: Vec<(At, Vec<PacketId>)>,
}

fn plan(envelope: &DeploymentConfig, tables: &[SystemTable]) -> Result<Plan, DeployError> {
    let mut index = DefIndex::new(envelope, tables);
    let mut plan = Plan {
        transports: Vec::new(),
        outputs: Vec::new(),
        commands: Vec::new(),
    };
    let mut commands: HashMap<At, Vec<PacketId>> = HashMap::new();
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
            if peer_at.get(envelope).ty == DB_TYPE {
                let ids = commands.entry(peer_at).or_default();
                for port in &index.def(at)?.outputs {
                    if let RecordSchema::Msg { id, .. } = &port.schema
                        && !ids.contains(id)
                    {
                        ids.push(*id);
                    }
                }
            }
        }
    }
    let mut commands: Vec<_> = commands.into_iter().collect();
    commands.sort_unstable_by_key(|(at, _)| *at);
    for (_, ids) in &mut commands {
        ids.sort_unstable();
    }
    plan.commands = commands;
    Ok(plan)
}

/// No two listeners on one host bind one port.
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

/// `host:port`, bracketing an IPv6 host.
fn address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// A link's params, or `None` for a system of any other type.
fn link(system: &SystemConfig) -> Result<Option<Part<LinkParams>>, DeployError> {
    if !LINK_TYPES.contains(&system.ty.as_str()) {
        return Ok(None);
    }
    Part::read(system).map(Some)
}

/// The peer a link dials.
fn peer_link(params: &LinkParams) -> Option<&PeerLink> {
    match &params.transport {
        Transport::Peer(peer) => Some(peer),
        _ => None,
    }
}

/// The nonzero port a link listens on.
fn fixed_port(system: &SystemConfig) -> Result<Option<u16>, DeployError> {
    let port = link(system)?.and_then(|params| listen_port(&params.part));
    Ok(port.filter(|&port| port != 0))
}

/// The port a link's transport listens on.
fn listen_port(params: &LinkParams) -> Option<u16> {
    let Transport::Listen { addr, .. } = &params.transport else {
        return None;
    };
    addr.rsplit_once(':')?.1.parse().ok()
}

#[cfg(test)]
mod tests;
