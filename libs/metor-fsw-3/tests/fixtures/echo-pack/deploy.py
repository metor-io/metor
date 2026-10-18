"""The fixture's deployment: `a` samples, and `b` mirrors the samples and
pings their values.

`METOR_DEPLOY_PORTS` names the two listen ports, so tests do not collide.
"""

import os

from echo_pack import Meter, Sampler
from metor_config import Deployment, Listen, Peer, Publish, Subscribe, Target

p_a, p_b = os.environ.get("METOR_DEPLOY_PORTS", "2240,2241").split(",")

a = Target(cycle_rate=1000.0, namespace="a")
sampler = a.add("sampler", Sampler())
a_pub = a.add("pub", Publish(transport=Listen(f"127.0.0.1:{p_a}"), all=True))

b = Target(cycle_rate=1000.0, namespace="b")
mirror = b.add("mirror", Subscribe(transport=Peer(a_pub), items=[sampler]))
b.add("meter", Meter(sample=mirror.port(sampler.sample)))
b.add("pub", Publish(transport=Listen(f"127.0.0.1:{p_b}"), all=True))

Deployment(targets=[a, b])
