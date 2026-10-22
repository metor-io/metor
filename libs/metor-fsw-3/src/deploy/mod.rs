//! Resolves the references between targets in a deployment.
//!
//! A deployment's config can refer to a target's peers: a resource can dial
//! another target's link by name, and an output port can take its definition
//! from a peer's port. `metor build` replaces each of these references with
//! the concrete address or port definition it stands for. After that, each
//! target can be built from its own packs, without knowing about the others.

mod config;
mod error;
mod index;
mod slot;

use std::collections::{BTreeMap, HashMap};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::Path;

use serde_json::Value;

use crate::coordinator::{OutputSource, SystemTable};
use crate::dl::SLOT_TYPE;
use crate::io::{Backing, RealKind};

pub use config::{
    CONFIG_VERSION, ConfigError, DeployBacking, DeploySource, DeploySystem, DeploymentConfig,
    PeerLink, PeerPort, ResolvedDeployment, TargetConfig,
};
pub use error::DeployError;
use index::{DefIndex, SystemIndex};
use slot::DeploySlot;

/// The host a peer is dialed on when the deployment gives it no host.
pub const LOOPBACK: &str = "127.0.0.1";

/// A resource: the index of its target, and its id.
type ResourceKey = (usize, String);

/// What a deployment still has to resolve: the address each system listens
/// on, and the slots whose candidates point at packs, each decoded once.
pub struct Pending {
    listen: Listeners,
    slots: Vec<(SystemIndex, DeploySlot)>,
}

/// Decodes every slot's params in `envelope` once, and returns them if a
/// peer reference or a slot candidate that points at a pack is left.
///
/// # Errors
/// Returns an error if a slot's params do not decode.
pub fn pending(envelope: &DeploymentConfig) -> Result<Option<Pending>, DeployError> {
    let mut pending = Pending {
        listen: Listeners(HashMap::new()),
        slots: Vec::new(),
    };
    for (index, system) in SystemIndex::iter(envelope) {
        if let Some(addr) = listen_addr(envelope, index, system) {
            pending.listen.0.insert(index, addr);
        }
        if system.ty == SLOT_TYPE {
            let slot = DeploySlot::read(envelope, index, system)?;
            if slot.pending() {
                pending.slots.push((index, slot));
            }
        }
    }
    let peer_output = SystemIndex::iter(envelope).any(|(_, system)| {
        let mut outputs = system.outputs.iter();
        outputs.any(|output| matches!(output.source, DeploySource::Peer(_)))
    });
    let peer_dial = envelope.targets.iter().any(|target| {
        let mut resources = target.coordinator.resources.values();
        resources.any(|r| matches!(r, DeployBacking::Peer(_)))
    });
    let left = peer_output || peer_dial || !pending.slots.is_empty();
    Ok(left.then_some(pending))
}

/// The address `system` listens on: its `listen` param, or the
/// `tcp_listen` resource it is granted.
fn listen_addr(
    envelope: &DeploymentConfig,
    index: SystemIndex,
    system: &DeploySystem,
) -> Option<String> {
    if let Some(addr) = system.params.get("listen").and_then(Value::as_str) {
        return Some(addr.to_string());
    }
    let resources = &envelope.targets[index.target].coordinator.resources;
    system
        .grants
        .values()
        .find_map(|id| match resources.get(id)? {
            DeployBacking::Runtime(Backing::Real(RealKind::TcpListen { addr, .. })) => {
                Some(addr.to_string())
            }
            _ => None,
        })
}

impl Pending {
    /// Resolves every slot, copying candidate libraries into `store`, then
    /// every peer reference.
    ///
    /// `tables` holds one system table per target. Peer dials become
    /// addresses, and peer outputs become port definitions.
    pub fn resolve(
        self,
        envelope: &mut DeploymentConfig,
        tables: &[SystemTable],
        store: &Path,
    ) -> Result<(), DeployError> {
        slot::resolve_slots(envelope, tables, store, self.slots)?;
        resolve_peers(envelope, tables, &self.listen)
    }
}

/// Resolves every peer reference in `envelope`.
fn resolve_peers(
    envelope: &mut DeploymentConfig,
    tables: &[SystemTable],
    listen: &Listeners,
) -> Result<(), DeployError> {
    let plan = plan(envelope, tables, listen)?;
    check_ports(envelope, listen)?;
    for ((target, id), addr) in plan.dials {
        let dial = Backing::Real(RealKind::TcpDial { addr });
        let resources = &mut envelope.targets[target].coordinator.resources;
        resources.insert(id, DeployBacking::Runtime(dial));
    }
    for ((index, output), def) in plan.outputs {
        let source = DeploySource::Runtime(OutputSource::Def(def));
        index.get_mut(envelope).outputs[output].source = source;
    }
    Ok(())
}

/// The address each system listens on, decoded once.
struct Listeners(HashMap<SystemIndex, String>);

impl Listeners {
    /// The port the system at `index` listens on, unless it is zero or the
    /// system does not listen.
    fn fixed_port(&self, index: SystemIndex) -> Option<u16> {
        let port: u16 = self.0.get(&index)?.rsplit_once(':')?.1.parse().ok()?;
        (port != 0).then_some(port)
    }
}

struct Plan {
    dials: BTreeMap<ResourceKey, SocketAddr>,
    outputs: Vec<((SystemIndex, usize), crate::PortDef)>,
}

fn plan(
    envelope: &DeploymentConfig,
    tables: &[SystemTable],
    listen: &Listeners,
) -> Result<Plan, DeployError> {
    let mut defs = DefIndex::new(envelope, tables);
    let mut plan = Plan {
        dials: BTreeMap::new(),
        outputs: Vec::new(),
    };
    for (index, system) in SystemIndex::iter(envelope) {
        plan.add(envelope, &mut defs, listen, index)
            .map_err(|e| e.at(system.location.as_ref()))?;
    }
    Ok(plan)
}

impl Plan {
    /// Plans the peer outputs of the system at `index`, and the peer dials
    /// it is granted.
    fn add(
        &mut self,
        envelope: &DeploymentConfig,
        defs: &mut DefIndex<'_>,
        listen: &Listeners,
        index: SystemIndex,
    ) -> Result<(), DeployError> {
        let system = index.get(envelope);
        for (output_index, output) in system.outputs.iter().enumerate() {
            if let DeploySource::Peer(peer) = &output.source {
                let def = defs.peer_port(index, &output.port, peer)?;
                self.outputs.push(((index, output_index), def));
            }
        }
        let resources = &envelope.targets[index.target].coordinator.resources;
        for id in system.grants.values() {
            if let Some(DeployBacking::Peer(peer)) = resources.get(id) {
                let addr = dial_addr(envelope, defs, listen, index, peer)?;
                self.dials.insert((index.target, id.clone()), addr);
            }
        }
        Ok(())
    }
}

/// The address the system at `from` dials to reach `peer`.
fn dial_addr(
    envelope: &DeploymentConfig,
    defs: &DefIndex<'_>,
    listen: &Listeners,
    from: SystemIndex,
    peer: &PeerLink,
) -> Result<SocketAddr, DeployError> {
    let from = defs.name(from);
    let peer_index = defs.locate(&from, &peer.target, &peer.link)?;
    let port = listen
        .fixed_port(peer_index)
        .ok_or_else(|| DeployError::NotAListener {
            from: from.clone(),
            target: peer.target.clone(),
            link: peer.link.clone(),
        })?;
    let addr = address(host(envelope, &peer.target), port);
    let unresolved = |message: String| DeployError::Address {
        from: from.clone(),
        addr: addr.clone(),
        message,
    };
    let resolved: Vec<SocketAddr> = addr
        .to_socket_addrs()
        .map_err(|e| unresolved(e.to_string()))?
        .collect();
    first_v4(&resolved).ok_or_else(|| unresolved("no address".into()))
}

/// The first IPv4 address of `addrs`, else the first: a name such as
/// `localhost` often lists `::1` first, which a listener on `0.0.0.0` does not
/// take.
fn first_v4(addrs: &[SocketAddr]) -> Option<SocketAddr> {
    let v4 = addrs.iter().find(|a| a.is_ipv4());
    v4.or(addrs.first()).copied()
}

/// Checks that no two listeners on the same host bind the same port.
fn check_ports(envelope: &DeploymentConfig, listen: &Listeners) -> Result<(), DeployError> {
    let mut bound: HashMap<(String, u16), String> = HashMap::new();
    for (index, system) in SystemIndex::iter(envelope) {
        let Some(port) = listen.fixed_port(index) else {
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
