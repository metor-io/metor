"""The fixture's links inside composites: `a` serves samples through
`tx.pub`, and `b` dials it from `rx.sub`.

`METOR_DEPLOY_PORTS` names the port `tx.pub` listens on."""

import os

from echo_pack import Meter, Sampler
from metor_config import Deployment, Listen, Peer, Publish, Scope, Subscribe, Target, composite

port = os.environ.get("METOR_DEPLOY_PORTS", "2240")


@composite
def tx(s: Scope) -> Publish:
    sampler = s.add("sampler", Sampler())
    return s.add("pub", Publish([sampler], transport=Listen(f"127.0.0.1:{port}")))


@composite
def rx(s: Scope, *, link: Publish) -> None:
    s.add("sub", Subscribe(transport=Peer(link)))


a = Target(cycle_rate=1000.0, namespace="a")
pub = a.add("tx", tx())
b = Target(cycle_rate=1000.0, namespace="b")
b.add("rx", rx(link=pub))
Deployment(targets=[a, b])
