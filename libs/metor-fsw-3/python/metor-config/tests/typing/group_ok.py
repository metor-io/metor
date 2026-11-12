"""Groups pyright accepts."""

from __future__ import annotations

from typing import NamedTuple

from sensors_stub import Body, Plant, Sensors

from metor_config import Deployment, Group, GroupBuild, Listen, OutPort, Publish, Target, group


class Node(NamedTuple):
    target: Target
    link: Publish
    sensors: OutPort[Sensors]


@group
def node(g: Group, *, port: int, gain: float = 0.1) -> Node:
    t = g.target(cycle_rate=10.0)
    plant = t.add("plant", Plant(body=t.loop(Body)))
    link = t.add("pub", Publish([plant], transport=Listen(f"127.0.0.1:{port}")))
    return Node(t, link, plant.sensors)


@group
def pair(g: Group) -> tuple[Node, Node]:
    return g.add("a", node(port=2240)), g.add("b", node(port=2241, gain=0.2))


d = Deployment(hosts={"flight": "10.0.0.1"})
pending: GroupBuild[Node] = node(port=2250)
ground: Node = d.add("ground", pending)
a, b = d.add("flight", pair())
sensors: OutPort[Sensors] = a.sensors
path: str = Group(d, "x").path
