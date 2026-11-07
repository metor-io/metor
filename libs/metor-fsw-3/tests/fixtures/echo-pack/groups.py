"""The fixture's tree: `flight` holds three strings, each a gateway and a
CPU, and `ground` records every gateway.

A gateway's sampler reaches its CPU, which muxes it into `relay`. The
string adds a subscriber to the gateway once the CPU exists, and muxes the
CPU's relay into `back`, so `back` holds values that went both ways.

`METOR_DEPLOY_PORTS` names ten ports: per string the gateway's uplink, the
CPU's link, and the gateway's telemetry link, then the ground's db."""

import os
from typing import NamedTuple

from echo_pack import Sample, Sampler
from metor_config import (
    Deployment,
    Group,
    Listen,
    Mux,
    OutPort,
    Peer,
    Publish,
    Subscribe,
    Target,
    group,
    recorder,
)

ports = os.environ.get("METOR_DEPLOY_PORTS", ",".join(str(2240 + i) for i in range(10)))
ports = [int(port) for port in ports.split(",")]


class Gateway(NamedTuple):
    target: Target
    link: Publish
    sample: OutPort[Sample]


class Cpu(NamedTuple):
    link: Publish
    relay: OutPort[Sample]


@group
def gateway(g: Group, *, port: int) -> Gateway:
    t = g.target(cycle_rate=1000.0)
    sampler = t.add("sampler", Sampler())
    link = t.add("up", Publish([sampler], transport=Listen(f"127.0.0.1:{port}")))
    return Gateway(t, link, sampler.sample)


@group
def cpu(g: Group, *, gateway: Publish, sample: OutPort[Sample], port: int) -> Cpu:
    t = g.target(cycle_rate=1000.0)
    up = t.add("uplink", Subscribe(transport=Peer(gateway), items=[sample]))
    seen = up.port(sample)
    relay = t.add("relay", Mux(sample=Sample(value=seen.value), on=seen))
    link = t.add("pub", Publish([relay], transport=Listen(f"127.0.0.1:{port}")))
    return Cpu(link, relay.sample)


@group
def string(g: Group, *, ports: list[int]) -> Publish:
    gw = g.add("gw", gateway(port=ports[0]))
    c = g.add("cpu", cpu(gateway=gw.link, sample=gw.sample, port=ports[1]))
    down = gw.target.add("down", Subscribe(transport=Peer(c.link), items=[c.relay]))
    seen = down.port(c.relay)
    gw.target.add("back", Mux(sample=Sample(value=seen.value), on=seen))
    return gw.target.add("tm", Publish(transport=Listen(f"127.0.0.1:{ports[2]}"), all=True))


@group
def flight(g: Group) -> list[Publish]:
    return [g.add(f"s{i}", string(ports=ports[3 * i - 3 : 3 * i])) for i in (1, 2, 3)]


ground = Target(cycle_rate=1000.0, namespace="ground")
d = Deployment(targets=[ground])
links = d.add("flight", flight())
ground.add("rec", recorder(peers=links, listen=f"127.0.0.1:{ports[9]}"))
