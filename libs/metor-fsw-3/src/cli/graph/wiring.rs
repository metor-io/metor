//! The edges of a deployment: inputs within a target, and links between
//! targets.

use crate::coordinator::SystemConfig;
use crate::deploy::{ResolvedDeployment, TargetConfig, address, host};
use crate::io::{Backing, RealKind};
use crate::link::{PUBLISH_TYPE, SUBSCRIBE_TYPE};

/// One target's systems in step order, and the edges between them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wiring<'a> {
    pub nodes: Vec<Node<'a>>,
    pub edges: Vec<Edge>,
}

impl<'a> Wiring<'a> {
    /// The output and input port names `edge` joins.
    pub fn ports(&self, edge: &Edge) -> (&'a str, &'a str) {
        let output = self.nodes[edge.from].outputs[edge.output];
        (output, self.nodes[edge.to].inputs[edge.input])
    }
}

/// A system, with the ports that carry an edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node<'a> {
    pub id: &'a str,
    pub kind: &'a str,
    pub inputs: Vec<&'a str>,
    /// Each in the order its first consumer runs.
    pub outputs: Vec<&'a str>,
}

/// An output feeding an input, each as a node and a port index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub from: usize,
    pub output: usize,
    pub to: usize,
    pub input: usize,
}

/// The wiring of `target`. A source outside the target is left out.
pub fn wiring(target: &TargetConfig) -> Wiring<'_> {
    let systems = &target.coordinator.systems;
    let mut nodes: Vec<Node<'_>> = systems
        .iter()
        .map(|system| Node {
            id: &system.id,
            kind: &system.ty,
            inputs: Vec::new(),
            outputs: Vec::new(),
        })
        .collect();
    let mut edges = Vec::new();
    for (to, system) in systems.iter().enumerate() {
        for input in &system.inputs {
            for from in &input.from {
                let Some(source) = systems.iter().position(|s| s.id == from.system) else {
                    continue;
                };
                let input = slot(&mut nodes[to].inputs, &input.port);
                let output = slot(&mut nodes[source].outputs, &from.port);
                edges.push(Edge {
                    from: source,
                    output,
                    to,
                    input,
                });
            }
        }
    }
    Wiring { nodes, edges }
}

/// The index of `port` in `ports`, added if new.
fn slot<'a>(ports: &mut Vec<&'a str>, port: &'a str) -> usize {
    ports.iter().position(|p| *p == port).unwrap_or_else(|| {
        ports.push(port);
        ports.len() - 1
    })
}

/// A system as a target index and a system index.
pub type At = (usize, usize);

/// A link between targets: the dialing system and the listening one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    pub dial: At,
    pub listen: At,
}

/// Each link in `config`. Resolving a deployment turns a link's peer into
/// the address of its listener, `host:port` with the host from
/// `config.hosts`, so a dial joins the listener with that address.
pub fn links(config: &ResolvedDeployment) -> Vec<Link> {
    let ends: Vec<(At, Endpoint<'_>)> = config
        .targets
        .iter()
        .enumerate()
        .flat_map(|(t, target)| {
            let systems = target.coordinator.systems.iter().enumerate();
            systems.filter_map(move |(s, system)| Some(((t, s), endpoint(target, system)?)))
        })
        .collect();
    let listener = |target: usize, addr: &str| {
        ends.iter().find_map(|(at, end)| match end {
            Endpoint::Listen(port) if at.0 != target && addr == peer_addr(config, at.0, *port) => {
                Some(*at)
            }
            _ => None,
        })
    };
    ends.iter()
        .filter_map(|(at, end)| match end {
            Endpoint::Dial(addr) => Some(Link {
                dial: *at,
                listen: listener(at.0, addr)?,
            }),
            Endpoint::Listen(_) => None,
        })
        .collect()
}

/// The address a peer dials to reach the listener on `port` of `target`.
fn peer_addr(config: &ResolvedDeployment, target: usize, port: u16) -> String {
    let ns = config.targets[target].namespace();
    address(host(config, ns), port)
}

/// The address a link system dials, or the port it listens on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endpoint<'a> {
    Dial(&'a str),
    Listen(u16),
}

fn endpoint<'a>(target: &'a TargetConfig, system: &SystemConfig) -> Option<Endpoint<'a>> {
    if system.ty != PUBLISH_TYPE && system.ty != SUBSCRIBE_TYPE {
        return None;
    }
    let resources = &target.coordinator.resources;
    system
        .grants
        .values()
        .find_map(|id| match resources.get(id)? {
            Backing::Real(RealKind::TcpDial { addr }) => Some(Endpoint::Dial(addr)),
            Backing::Real(RealKind::TcpListen { addr, .. }) => Some(Endpoint::Listen(addr.port())),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::{InputConfig, PortRef};

    fn system(id: &str, ty: &str, inputs: &[(&str, &[(&str, &str)])]) -> SystemConfig {
        let mut system = SystemConfig::new(id, ty);
        system.inputs = inputs
            .iter()
            .map(|(port, from)| InputConfig {
                port: (*port).into(),
                from: from.iter().map(|(s, p)| PortRef::new(*s, *p)).collect(),
            })
            .collect();
        system
    }

    #[test]
    fn test_wiring_numbers_ports_by_first_use() {
        let mut target = TargetConfig::default();
        target.coordinator.systems = vec![
            system("a", "p.A", &[]),
            system("b", "p.B", &[("x", &[("a", "out")])]),
            system(
                "c",
                "p.C",
                &[("x", &[("a", "out"), ("b", "y")]), ("z", &[("gone", "o")])],
            ),
        ];
        let wiring = wiring(&target);
        assert_eq!(wiring.nodes[0].outputs, ["out"]);
        assert_eq!(wiring.nodes[1].outputs, ["y"]);
        assert_eq!(
            wiring.nodes[2].inputs,
            ["x"],
            "a missing source draws nothing"
        );
        assert_eq!(
            wiring.edges,
            [
                Edge {
                    from: 0,
                    output: 0,
                    to: 1,
                    input: 0
                },
                Edge {
                    from: 0,
                    output: 0,
                    to: 2,
                    input: 0
                },
                Edge {
                    from: 1,
                    output: 0,
                    to: 2,
                    input: 0
                },
            ]
        );
        assert_eq!(super::wiring(&TargetConfig::default()), Wiring::default());
    }

    fn link_target(ns: &str, id: &str, backing: RealKind) -> TargetConfig {
        let mut target = TargetConfig::default();
        target.coordinator.namespace = Some(ns.into());
        let mut system = system(id, SUBSCRIBE_TYPE, &[]);
        system.grants.insert("ep".into(), "r".into());
        target.coordinator.systems = vec![system];
        target
            .coordinator
            .resources
            .insert("r".into(), Backing::Real(backing));
        target
    }

    #[test]
    fn test_links_join_dials_to_listeners_by_port() {
        let mut config = ResolvedDeployment {
            config_version: crate::deploy::CONFIG_VERSION,
            hosts: Default::default(),
            targets: Vec::new(),
        };
        let listen = |port: u16| RealKind::TcpListen {
            addr: ([0, 0, 0, 0], port).into(),
            allow: Vec::new(),
        };
        let dial = |addr: &str| RealKind::TcpDial { addr: addr.into() };
        config.targets = vec![
            link_target("plant", "pub", listen(2240)),
            link_target("twin", "pub", listen(2240)),
            link_target("fsw", "plant", dial("10.0.0.2:2240")),
            link_target("gw", "local", dial("127.0.0.1:2240")),
            link_target("ext", "far", dial("ground.example:2240")),
        ];
        config.hosts.insert("plant".into(), "10.0.0.1".into());
        config.hosts.insert("twin".into(), "10.0.0.2".into());
        assert_eq!(
            links(&config),
            [Link {
                dial: (2, 0),
                listen: (1, 0)
            }],
            "the host picks between listeners on one port; no listener is on loopback"
        );
        config.hosts.remove("plant");
        assert_eq!(
            links(&config)[1],
            Link {
                dial: (3, 0),
                listen: (0, 0)
            },
            "an unlisted host is loopback"
        );
    }
}
