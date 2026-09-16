"""The fixture's target: `gain` feeds itself through a loop, `echo` watches it."""

from echo_pack import Echo, Gain, Ping
from metor_config import Target

fsw = Target(cycle_rate=1000.0)
looped = fsw.loop(Ping)
gain = fsw.add("gain", Gain(input=looped, gain=2.0))
looped.connect(gain.output)
fsw.add("echo", Echo(input=gain.output))
