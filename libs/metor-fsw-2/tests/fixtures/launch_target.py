"""Two members, each with its own link server, for the launcher tests."""

from metor_config import Deployment, Downlink, Target, TcpServer

a = Target(cycle_rate=100.0, sim_dt=0.01, namespace="a")
link_a = a.state("link", TcpServer(addr="127.0.0.1:2250"))
a.add("downlink", Downlink(link_a))

b = Target(cycle_rate=100.0, sim_dt=0.01, namespace="b")
link_b = b.state("link", TcpServer(addr="127.0.0.1:2251"))
b.add("downlink", Downlink(link_b))

Deployment(targets=[a, b])
