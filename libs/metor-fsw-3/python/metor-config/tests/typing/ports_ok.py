"""Lists and groups of ports pyright accepts."""

from __future__ import annotations

from ports_stub import Adcs, WheelCmdPort

from metor_config import Target

target = Target(10.0)
a = target.add("a", Adcs())
wheels = [Adcs.Wheels(speed=a.est[0]), Adcs.Wheels()]
b = target.add("b", Adcs(fss=[a.est[0], a.est[1], ()], wheels=wheels))
cmd: WheelCmdPort = b.wheels[1].cmd
