//! The build pass over a deployment: every reference to a peer becomes the
//! address or the definition it stands for, so a target then builds from its
//! own packs alone.

mod error;
mod index;

use std::collections::HashMap;

use metor_proto::types::PacketId;
use serde_json::{Value, json};

use crate::cli::config::DeploymentConfig;
use crate::coordinator::{OutputSource, PeerLink, SystemTable};
use crate::record::RecordSchema;

pub use error::DeployError;
use index::{At, DefIndex};

/// Where a peer is dialed when the deployment places its target nowhere.
pub const LOOPBACK: &str = "127.0.0.1";

/// The type a gateway's database registers as.
pub const DB_TYPE: &str = "fsw.db";

/// Resolves every peer reference in `envelope` against `tables`, one per
/// target: peer transports become addresses, peer outputs become
/// definitions, and each database learns the commands its subscribers take.
/// An envelope with nothing to resolve is left as it is.
pub fn resolve(envelope: &mut DeploymentConfig, tables: &[SystemTable]) -> Result<(), DeployError> {
    let plan = plan(envelope, tables)?;
    for (at, addr) in plan.transports {
        system(envelope, at).params["transport"] = json!({ "connect": { "addr": addr } });
    }
    for ((at, output), def) in plan.outputs {
        system(envelope, at).outputs[output].source = OutputSource::Def(def);
    }
    for (at, ids) in plan.commands {
        system(envelope, at).params["command_ids"] = json!(ids);
    }
    check_ports(envelope)
}

/// Whether anything in `envelope` still refers to a peer.
pub fn unresolved(envelope: &DeploymentConfig) -> bool {
    envelope.targets.iter().any(|target| {
        target.coordinator.systems.iter().any(|system| {
            peer_link(system.params.get("transport")).is_some()
                || system
                    .outputs
                    .iter()
                    .any(|output| matches!(output.source, OutputSource::Peer(_)))
        })
    })
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
            let at = (t, s);
            for (o, output) in system.outputs.iter().enumerate() {
                if let OutputSource::Peer(peer) = &output.source {
                    plan.outputs
                        .push(((at, o), index.peer_port(at, &output.port, peer)?));
                }
            }
            let Some(peer) = peer_link(system.params.get("transport")) else {
                continue;
            };
            let from = index.name(at);
            let peer_at = index.locate(&from, &peer.target, &peer.link)?;
            let port =
                listen_port(&envelope.targets[peer_at.0].coordinator.systems[peer_at.1].params)
                    .filter(|&port| port != 0)
                    .ok_or_else(|| DeployError::NotAListener {
                        from: from.clone(),
                        target: peer.target.clone(),
                        link: peer.link.clone(),
                    })?;
            plan.transports
                .push((at, address(host(envelope, &peer.target), port)));
            if envelope.targets[peer_at.0].coordinator.systems[peer_at.1].ty == DB_TYPE {
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
            let Some(port) = listen_port(&system.params).filter(|&port| port != 0) else {
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

fn system(envelope: &mut DeploymentConfig, at: At) -> &mut crate::SystemConfig {
    &mut envelope.targets[at.0].coordinator.systems[at.1]
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

/// The peer a `transport` param names, if it names one.
fn peer_link(transport: Option<&Value>) -> Option<PeerLink> {
    serde_json::from_value(transport?.get("peer")?.clone()).ok()
}

/// The port a link's `transport` param listens on.
fn listen_port(params: &Value) -> Option<u16> {
    let addr = params
        .get("transport")?
        .get("listen")?
        .get("addr")?
        .as_str()?;
    addr.rsplit_once(':')?.1.parse().ok()
}

#[cfg(test)]
mod tests;
