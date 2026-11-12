"""Composites pyright refuses: each `# expect-error` line, and no other."""

from __future__ import annotations

from typing import NamedTuple

from sensors_stub import Body, Plant

from metor_config import Scope, Sources, Target, composite


class Out(NamedTuple):
    plant: Plant


@composite
def gnc(s: Scope, *, body: Sources[Body], gain: float = 0.1) -> Out:
    return Out(s.add("plant", Plant(body=body)))


@composite
def sensing(s: Scope, *, truth: Sources[Body] | None = None) -> None:
    if truth is not None:
        s.sim("plant", Plant(body=truth))


target = Target(10.0)
body = target.loop(Body)
out = target.add("gnc", gnc(body=body))
gnc()  # expect-error
gnc(body=body, gain="high")  # expect-error
gnc(body=out.plant.sensors)  # expect-error
out.missing  # expect-error
target.add("a", gnc(body=body), thread="t")  # expect-error
target.add("b", gnc)  # expect-error
sensing(truth=out.plant.sensors)  # expect-error
out.plant.sensors
