"""Two members that exchange data: `b` mirrors `a`'s downlink.

Wall-clocked: the subscriber's socket task is polled once per cycle, so a
simulated clock would starve the connect the tests wait for.
"""

from metor_config import Deployment, Downlink, Publish, Subscribe, Target, TcpServer

a = Target(cycle_rate=100.0, namespace="a")
link_a = a.state("link", TcpServer(addr="127.0.0.1:2253"))
peer_a = a.state("peer", TcpServer(addr="127.0.0.1:2254", name="a-peer"))
dl = a.add("downlink", Downlink(link_a))
a.add("publish", Publish(peer_a, [dl]))

b = Target(cycle_rate=100.0, namespace="b")
link_b = b.state("link", TcpServer(addr="127.0.0.1:2255"))
b.add("downlink", Downlink(link_b))
b.add("a_link", Subscribe(dl))

Deployment(targets=[a, b])
