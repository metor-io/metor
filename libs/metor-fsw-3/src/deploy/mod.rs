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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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

/// The first port a dialed listener on port 0 is given, unless
/// [`Pending::first_port`] sets another.
pub const FIRST_PORT: u16 = 2240;

/// A resource: the index of its target, and its id.
type ResourceKey = (usize, String);

/// What a deployment still has to resolve: the address each system listens
/// on, and the slots whose candidates point at packs, each decoded once.
pub struct Pending {
    listen: Listeners,
    slots: Vec<(SystemIndex, DeploySlot)>,
    first_port: u16,
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
        first_port: FIRST_PORT,
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
        let mut listen = self.listen;
        assign_ports(envelope, &mut listen, self.first_port)?;
        resolve_peers(envelope, tables, &listen)
    }

    /// Sets the first port a dialed listener on port 0 is given.
    #[must_use]
    pub fn first_port(self, port: u16) -> Self {
        Self {
            first_port: port,
            ..self
        }
    }
}

/// Gives each listener that a peer dials and that listens on port 0 the
/// first port, from `first`, that no other listener on its host uses. The
/// port is written into the listener's address, so its dialers resolve to
/// it.
fn assign_ports(
    envelope: &mut DeploymentConfig,
    listen: &mut Listeners,
    first: u16,
) -> Result<(), DeployError> {
    let mut taken: HashSet<(String, u16)> = listen
        .0
        .keys()
        .filter_map(|&index| Some((host_of(envelope, index), listen.fixed_port(index)?)))
        .collect();
    for index in dialed(envelope) {
        if listen.port(index) != Some(0) {
            continue;
        }
        let host = host_of(envelope, index);
        let port = (first..=u16::MAX)
            .find(|&port| !taken.contains(&(host.clone(), port)))
            .ok_or_else(|| DeployError::NoFreePort { host: host.clone() })?;
        taken.insert((host, port));
        listen.set_port(envelope, index, port);
    }
    Ok(())
}

/// Every system some peer dial names, in target and step order.
fn dialed(envelope: &DeploymentConfig) -> BTreeSet<SystemIndex> {
    let peers = envelope.targets.iter().flat_map(|target| {
        target
            .coordinator
            .resources
            .values()
            .filter_map(|r| match r {
                DeployBacking::Peer(peer) => Some(peer),
                _ => None,
            })
    });
    peers
        .filter_map(|peer| {
            let target = envelope
                .targets
                .iter()
                .position(|t| t.namespace() == peer.target)?;
            let systems = &envelope.targets[target].coordinator.systems;
            let system = systems.iter().position(|s| s.id == peer.link)?;
            Some(SystemIndex::new(target, system))
        })
        .collect()
}

/// The host the system at `index` is dialed on.
fn host_of(envelope: &DeploymentConfig, index: SystemIndex) -> String {
    host(envelope, envelope.targets[index.target].namespace()).to_string()
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
    /// The port the system at `index` listens on, if it listens.
    fn port(&self, index: SystemIndex) -> Option<u16> {
        self.0.get(&index)?.rsplit_once(':')?.1.parse().ok()
    }

    /// The port the system at `index` listens on, unless it is zero or the
    /// system does not listen.
    fn fixed_port(&self, index: SystemIndex) -> Option<u16> {
        self.port(index).filter(|&port| port != 0)
    }

    /// Makes the system at `index` listen on `port`, here and in its
    /// `listen` param or `tcp_listen` resource.
    fn set_port(&mut self, envelope: &mut DeploymentConfig, index: SystemIndex, port: u16) {
        let Some(addr) = self.0.get_mut(&index) else {
            return;
        };
        let Some((host, _)) = addr.rsplit_once(':') else {
            return;
        };
        *addr = format!("{host}:{port}");
        let system = index.get_mut(envelope);
        if let Some(listen) = system.params.get_mut("listen") {
            *listen = Value::String(addr.clone());
            return;
        }
        let grants: Vec<String> = system.grants.values().cloned().collect();
        let resources = &mut envelope.targets[index.target].coordinator.resources;
        for id in grants {
            if let Some(DeployBacking::Runtime(Backing::Real(RealKind::TcpListen {
                addr, ..
            }))) = resources.get_mut(&id)
            {
                addr.set_port(port);
            }
        }
    }
}

struct Plan {
    dials: BTreeMap<ResourceKey, String>,
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
) -> Result<String, DeployError> {
    let from = defs.name(from);
    let peer_index = defs.locate(&from, &peer.target, &peer.link)?;
    let port = listen
        .fixed_port(peer_index)
        .ok_or_else(|| DeployError::NotAListener {
            from: from.clone(),
            target: peer.target.clone(),
            link: peer.link.clone(),
        })?;
    Ok(address(host(envelope, &peer.target), port))
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

pub(crate) fn host<'a, S, R>(envelope: &'a DeploymentConfig<S, R>, target: &str) -> &'a str {
    envelope.hosts.get(target).map_or(LOOPBACK, String::as_str)
}

/// Formats `host:port`, wrapping an IPv6 host in brackets.
pub(crate) fn address(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests;
