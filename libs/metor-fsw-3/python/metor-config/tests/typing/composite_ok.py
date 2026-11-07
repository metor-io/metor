"""Composites pyright accepts."""

from __future__ import annotations

from typing import NamedTuple

from sensors_stub import Body, Plant, Sensors

from metor_config import Composite, Listen, OutPort, Publish, Scope, Sources, Target, composite


class Out(NamedTuple):
    plant: Plant
    sensors: OutPort[Sensors]


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
