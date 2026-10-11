"""The fixture's deployment: `a` samples, `b` mirrors the samples and pings
their values, and `gw` records both into a db.

`METOR_DEPLOY_PORTS` names the three listen ports, so tests do not collide.
"""

import os

from echo_pack import Meter, Sampler
from metor_config import Deployment, Listen, Peer, Publish, Subscribe, Target, gateway

p_a, p_b, p_db = os.environ.get("METOR_DEPLOY_PORTS", "2240,2241,2250").split(",")

a = Target(cycle_rate=1000.0, namespace="a")
sampler = a.add("sampler", Sampler())
a_pub = a.add("pub", Publish(transport=Listen(f"127.0.0.1:{p_a}"), all=True))

b = Target(cycle_rate=1000.0, namespace="b")
mirror = b.add("mirror", Subscribe(transport=Peer(a_pub), items=[sampler]))
b.add("meter", Meter(sample=mirror.port(sampler.sample)))
b_pub = b.add("pub", Publish(transport=Listen(f"127.0.0.1:{p_b}"), all=True))

gw, gw_db = gateway("gw", peers=[a_pub, b_pub], listen=f"127.0.0.1:{p_db}", cycle_rate=1000.0)

Deployment(targets=[a, b, gw])
