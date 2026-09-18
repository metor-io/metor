"""The fixture's target: `echo` answers the pings `cmds` is sent, `pub` serves
them back, and `gain` feeds itself through a loop."""

from echo_pack import Echo, Gain, Ping
from metor_config import Publish, Subscribe, Target

fsw = Target(cycle_rate=1000.0)
cmds = fsw.add("cmds", Subscribe([Ping], listen="127.0.0.1:0"))
looped = fsw.loop(Ping)
gain = fsw.add("gain", Gain(input=looped, gain=2.0))
looped.connect(gain.output)
echo = fsw.add("echo", Echo(input=cmds.ping))
fsw.add("pub", Publish([echo], listen="127.0.0.1:0"))
