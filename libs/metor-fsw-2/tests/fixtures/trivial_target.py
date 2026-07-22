"""A trivial two-system static target, for the subprocess eval-path test."""

from metor_config import Target, TcpServer, static_system

m = Target(cycle_rate=100.0, sim_dt=0.01)

m.state("link", TcpServer(addr="127.0.0.1:2240"))
a = m.add("a", static_system("Alarms"))
b = m.add("b", static_system("Downlink"))

m.connect(a.out, b.in_)
