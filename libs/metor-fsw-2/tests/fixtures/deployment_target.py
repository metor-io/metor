"""A two-member deployment, for the subprocess eval-path selection test."""

from metor_config import Deployment, Target, TcpServer, static_system

a = Target(cycle_rate=100.0, sim_dt=0.01, namespace="a")
b = Target(cycle_rate=50.0, sim_dt=0.02, namespace="b")

a.state("link", TcpServer(addr="127.0.0.1:2240"))
a.add("plant", static_system("Alarms"))

b.state("link", TcpServer(addr="127.0.0.1:2241"))
b.add("fsw", static_system("Downlink"))

Deployment(targets=[a, b])
