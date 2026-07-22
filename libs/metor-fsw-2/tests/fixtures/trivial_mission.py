"""A trivial two-system static mission, for the subprocess eval-path test."""

from metor_config import Mission, TcpServer, static_system

m = Mission(cycle_rate=100.0, sim_dt=0.01)

m.state("link", TcpServer(addr="127.0.0.1:2240"))
a = m.add("a", static_system("Alarms"))
b = m.add("b", static_system("Downlink"))

m.connect(a.out, b.in_)
