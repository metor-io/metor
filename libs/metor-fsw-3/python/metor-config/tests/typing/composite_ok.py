"""Composites pyright accepts."""

from __future__ import annotations

from typing import NamedTuple

from sensors_stub import Body, Plant, Sensors

from metor_config import (
    Composite,
    Listen,
    Loop,
    OutPort,
    Publish,
    Scope,
    Sources,
    Target,
    composite,
)


class Out(NamedTuple):
    plant: Plant
    sensors: OutPort[Sensors]


class World(NamedTuple):
    body: Loop[Body]


@composite
def sensing(s: Scope, *, truth: Sources[Body] | None = None) -> None:
    if truth is not None:
        s.sim("plant", Plant(body=truth))


@composite
def gnc(s: Scope, *, body: Sources[Body], gain: float = 0.1) -> Out:
    plant = s.add("plant", Plant(body=body))
    return Out(plant, plant.sensors)


@composite
def outer(s: Scope, *, body: Sources[Body]) -> Out:
    return s.add("inner", gnc(body=body))


target = Target(10.0)
pending: Composite[Out] = gnc(body=target.loop(Body), gain=0.2)
out: Out = target.add("gnc", pending)
sensors: OutPort[Sensors] = out.plant.sensors
plant, _ = target.add("outer", outer(body=target.loop(Body)))
target.add("pub", Publish(out, transport=Listen("127.0.0.1:0")))
target.add("with_truth", sensing(truth=target.loop(Body)))
target.add("without_truth", sensing(truth=None))
target.add("by_default", sensing())


def world_sensing(world: World | None) -> Composite[None]:
    return sensing(truth=world and world.body)


target.add("in_world", world_sensing(World(target.loop(Body))))
target.add("no_world", world_sensing(None))
