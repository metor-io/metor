"""The fixture's alarms: `hot` and `burn` read the level `cmds` is pinged, and
`pub` serves their alarm edges.

`METOR_ALARM_PORTS` names the two listen ports, so tests do not collide.
"""

import os

from echo_pack import Burn, Hot, Level, Ping
from metor_config import Listen, Publish, SequenceCommand, Subscribe, Target

p_cmds, p_pub = os.environ.get("METOR_ALARM_PORTS", "2244,2245").split(",")

fsw = Target(cycle_rate=1000.0)
cmds = fsw.add("cmds", Subscribe([Ping, SequenceCommand], transport=Listen(f"127.0.0.1:{p_cmds}")))
fsw.commands(cmds.sequence_command)
level = fsw.add("level", Level(input=cmds.ping))
hot = fsw.add("hot", Hot(sample=level.sample), channel="hot")
burn = fsw.add("burn", Burn(sample=level.sample), channel="burn")
alarms = [hot.alarm_raised, hot.alarm_cleared, burn.alarm_raised, burn.alarm_cleared]
fsw.add("pub", Publish(alarms, transport=Listen(f"127.0.0.1:{p_pub}")))
