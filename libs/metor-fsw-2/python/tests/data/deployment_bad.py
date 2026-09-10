"""The negative half of the pyright gate: a mirror's ports type as the peer
instance's own, so an edge out of one is checked like any local edge. The one
`connect` here crosses frames (`Sensors` into an `InPort[Cmd]`) and must be
reported."""

from demo import Widget

from metor_config import Deployment, Publish, Subscribe, Target, TcpServer

plant = Target(cycle_rate=120.0, sim_dt=1 / 120, namespace="plant")
fsw = Target(cycle_rate=120.0, sim_dt=1 / 120, namespace="fsw")

peers = plant.state("peer", TcpServer(addr="[::]:2242"))
widget = plant.add("widget", Widget())
publish = plant.add("publish", Publish(peers, [widget]))

mirror = fsw.add("widget", Subscribe(widget, via=publish))
local = fsw.add("local", Widget())
fsw.connect(mirror.sensors, local.cmd)

deployment = Deployment(targets=[plant, fsw])
