"""Composites pyright accepts."""

from __future__ import annotations

from typing import NamedTuple

from adcs_stub import Imu, LogEvent, Nav, Plant, SystemStatus

from metor_config import Composite, Listen, OutPort, Publish, Scope, Sources, Target, composite


class Out(NamedTuple):
    nav: Nav
    log: OutPort[LogEvent]


@composite
def gnc(s: Scope, *, imu: Sources[Imu], gain: float = 0.1) -> Out:
    nav = s.add("nav", Nav(imu=imu, gain=gain))
    return Out(nav, nav.log)


@composite
def outer(s: Scope, *, imu: Sources[Imu]) -> Out:
    return s.add("inner", gnc(imu=imu))


target = Target(10.0)
plant = target.add("plant", Plant())
pending: Composite[Out] = gnc(imu=plant.imu, gain=0.2)
out: Out = target.add("gnc", pending)
status: OutPort[SystemStatus] = out.nav.status
log: OutPort[LogEvent] = out.log
nav, _ = target.add("outer", outer(imu=target.loop(Imu)))
target.add("pub", Publish(out, transport=Listen("127.0.0.1:0")))
