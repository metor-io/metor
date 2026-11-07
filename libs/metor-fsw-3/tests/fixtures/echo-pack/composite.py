"""The fixture's composites: a gain feeding itself through a loop and an echo
of it, added alone and as two instances of a nested composite."""

from typing import NamedTuple

from echo_pack import Echo, Gain, Ping
from metor_config import Listen, OutPort, Publish, Scope, Target, composite


class Pair(NamedTuple):
    gain: Gain
    echoed: OutPort[Ping]


@composite
def pair(s: Scope, *, gain: float) -> Pair:
    looped = s.loop(Ping)
    amp = s.add("gain", Gain(input=looped, gain=gain))
    looped.connect(amp.output)
    return Pair(amp, s.add("echo", Echo(input=amp.output)).output)


@composite
def rack(s: Scope) -> tuple[Pair, Pair]:
    return s.add("a", pair(gain=2.0)), s.add("b", pair(gain=3.0))


fsw = Target(cycle_rate=1000.0, namespace="fsw")
solo = fsw.add("solo", pair(gain=2.0))
a, b = fsw.add("rack", rack())
fsw.add("pub", Publish([*solo, *a, *b], transport=Listen("127.0.0.1:0")))
