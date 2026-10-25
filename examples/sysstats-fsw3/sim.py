"""Samples this host's CPU, memory, network interfaces, processes, and disks,
and records them in a db that serves a panel on 2250.

The samplers are async systems on the `stats` thread, so a slow read never
stretches a cycle. CPU, memory, and interfaces are read at 100 Hz, processes
at 10 Hz since a full scan costs far more, and disks at 1 Hz since their
health changes slowly."""

from metor_config import Db, Listen, Publish, Target
from stats_pack import Cpu, Disks, Mem, Nics, Procs

fsw = Target(cycle_rate=100.0, namespace="host")

fsw.add("cpu", Cpu(rate_hz=100.0), thread="stats")
fsw.add("mem", Mem(rate_hz=100.0), thread="stats")
fsw.add("nics", Nics(rate_hz=100.0, max_nics=32, max_ethtool=64), thread="stats")
fsw.add("procs", Procs(rate_hz=10.0, max_procs=64), thread="stats")
fsw.add("disks", Disks(rate_hz=1.0, max_disks=64, smart=True), thread="stats")

fsw.add("pub", Publish(transport=Listen("[::]:2240"), all=True))
fsw.add("db", Db(listen="0.0.0.0:2250", all=True), thread="db")
