"""The fixture's tree: `flight` holds three strings, each a gateway and a
CPU, and `ground` records every gateway.

A gateway's sampler reaches its CPU, which muxes it into `relay`. The
string adds a subscriber to the gateway once the CPU exists, and muxes the
CPU's relay into `back`, so `back` holds values that went both ways.

Every link listens on port 0, so the build gives each a port, counting up
from `METOR_FIRST_PORT`. `METOR_DEPLOY_PORTS` names the ground db's port."""

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

db_port = os.environ.get("METOR_DEPLOY_PORTS", "2250")
ANY = Listen("127.0.0.1:0")


class Gateway(NamedTuple):
    target: Target
    link: Publish
    sample: OutPort[Sample]


class Cpu(NamedTuple):
    link: Publish
    relay: OutPort[Sample]


@group
def gateway(g: Group) -> Gateway:
    t = g.target(cycle_rate=1000.0)
    sampler = t.add("sampler", Sampler())
    link = t.add("up", Publish([sampler], transport=ANY))
    return Gateway(t, link, sampler.sample)


@group
def cpu(g: Group, *, gateway: Publish, sample: OutPort[Sample]) -> Cpu:
    t = g.target(cycle_rate=1000.0)
    up = t.add("uplink", Subscribe(transport=Peer(gateway), items=[sample]))
    seen = up.port(sample)
    relay = t.add("relay", Mux(sample=Sample(value=seen.value), on=seen))
    link = t.add("pub", Publish([relay], transport=ANY))
    return Cpu(link, relay.sample)


@group
def string(g: Group) -> Publish:
    gw = g.add("gw", gateway())
    c = g.add("cpu", cpu(gateway=gw.link, sample=gw.sample))
    down = gw.target.add("down", Subscribe(transport=Peer(c.link), items=[c.relay]))
    seen = down.port(c.relay)
    gw.target.add("back", Mux(sample=Sample(value=seen.value), on=seen))
    return gw.target.add("tm", Publish(transport=ANY, all=True))


@group
def flight(g: Group) -> list[Publish]:
    return [g.add(f"s{i}", string()) for i in (1, 2, 3)]


ground = Target(cycle_rate=1000.0, namespace="ground")
d = Deployment(targets=[ground])
links = d.add("flight", flight())
ground.add("rec", recorder(peers=links, listen=f"127.0.0.1:{db_port}"))
