"""Two members contending for one link address: whichever loses the bind fails."""

from metor_config import Deployment, Downlink, Target, TcpServer

a = Target(cycle_rate=100.0, sim_dt=0.01, namespace="a")
link_a = a.state("link", TcpServer(addr="127.0.0.1:2252"))
a.add("downlink", Downlink(link_a))

b = Target(cycle_rate=100.0, sim_dt=0.01, namespace="b")
link_b = b.state("link", TcpServer(addr="127.0.0.1:2252"))
b.add("downlink", Downlink(link_b))

Deployment(targets=[a, b])
