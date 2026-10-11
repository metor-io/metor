"""Three members: `a` and `b` each serve a ground link, `gw` ingests both.

Wall-clocked: the ingests' client tasks are polled once per cycle, so a
simulated clock would starve the connects the tests wait for. `a` forwards
`AlarmAck` and `b` forwards `ReloadSequences`, which `b` routes on to its
coordinator, so a command pushed into the gateway reaches exactly one member.
The downlinks are named per member because a `LogEvent`'s `source` is the
emitting instance alone: the names are what tells the two members' log lines
apart once both are in the gateway's one log.
"""

from metor_config import (
    Alarms,
    Db,
    Deployment,
    Downlink,
    Ingest,
    Record,
    Target,
    TcpServer,
    Uplink,
)

a = Target(cycle_rate=100.0, namespace="a")
link_a = a.state("link", TcpServer(addr="127.0.0.1:2256"))
a.add("uplink", Uplink(link_a, msgs=["AlarmAck"]))
a.add("alarms", Alarms([]))
a.add("a_downlink", Downlink(link_a))

b = Target(cycle_rate=100.0, namespace="b")
link_b = b.state("link", TcpServer(addr="127.0.0.1:2257"))
uplink_b = b.add("uplink", Uplink(link_b, msgs=["ReloadSequences"]))
b.route(uplink_b, b.coordinator, msg="ReloadSequences")
b.add("b_downlink", Downlink(link_b))

gw = Target(cycle_rate=100.0, namespace="gw")
db = gw.state("db", Db(addr="127.0.0.1:2258"))
gw.add("a", Ingest(db, link_a))
gw.add("b", Ingest(db, link_b))
gw.add("record", Record(db))

Deployment(targets=[a, b, gw])
