"""Composites pyright refuses: each `# expect-error` line, and no other."""

from __future__ import annotations

from typing import NamedTuple

from adcs_stub import Imu, Nav, Plant

from metor_config import Scope, Sources, Target, composite


class Out(NamedTuple):
    nav: Nav


@composite
def gnc(s: Scope, *, imu: Sources[Imu], gain: float = 0.1) -> Out:
    return Out(s.add("nav", Nav(imu=imu, gain=gain)))


target = Target(10.0)
plant = target.add("plant", Plant())
gnc()  # expect-error
gnc(imu=plant.imu, gain="high")  # expect-error
gnc(imu=plant.log)  # expect-error
target.add("gnc", gnc(imu=plant.imu)).missing  # expect-error
target.add("gnc", gnc(imu=plant.imu), thread="t")  # expect-error
target.add("gnc", gnc)  # expect-error
target.add("gnc", gnc(imu=plant.imu)).nav.est
