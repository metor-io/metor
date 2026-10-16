"""The fixture's slot: `mode` loads `count_up` or `count_down` on the
commands `cmds` is sent, and `pub` serves its events and the registry.

`METOR_SLOT_PORTS` names the two listen ports, so tests do not collide.
"""

import os

from echo_pack import CountDown, CountUp
from metor_config import ChannelRegistry, DlSlot, Publish, SequenceCommand, Subscribe, Target

p_cmds, p_pub = os.environ.get("METOR_SLOT_PORTS", "2242,2243").split(",")

fsw = Target(cycle_rate=1000.0)
cmds = fsw.add("cmds", Subscribe([SequenceCommand], listen=f"127.0.0.1:{p_cmds}"))
fsw.commands(cmds.sequence_command)
mode = fsw.add("mode", DlSlot(allow=[CountUp(), CountDown()]), channel="mode", start="stopped")
registry = fsw.add("channels", ChannelRegistry())
fsw.add("pub", Publish([mode, registry.registry], listen=f"127.0.0.1:{p_pub}"))
