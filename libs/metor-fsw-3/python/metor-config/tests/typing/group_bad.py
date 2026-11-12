"""Groups pyright refuses: each `# expect-error` line, and no other."""

from __future__ import annotations

from typing import NamedTuple

from sensors_stub import Body, Plant

from metor_config import Deployment, Group, Target, group


class Node(NamedTuple):
    target: Target
    plant: Plant


@group
def node(g: Group, *, port: int) -> Node:
    t = g.target(cycle_rate=10.0)
    return Node(t, t.add("plant", Plant(body=t.loop(Body))))


@group
def bad_target(g: Group) -> None:
    g.target(cycle_rate=10.0, namespace="x")  # expect-error


d = Deployment()
out = d.add("n", node(port=1))
node()  # expect-error
node(port="high")  # expect-error
out.missing  # expect-error
d.add("m", node)  # expect-error
d.add("t", Target(10.0))  # expect-error
out.plant.sensors
