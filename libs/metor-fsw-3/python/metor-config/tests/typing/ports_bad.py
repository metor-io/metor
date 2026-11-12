"""Lists and groups of ports pyright refuses: each `# expect-error` line, and no other."""

from __future__ import annotations

from ports_stub import Adcs

from metor_config import Target

target = Target(10.0)
a = target.add("a", Adcs())
Adcs(fss=a.est[0])  # expect-error
Adcs(wheels=[a.est[0]])  # expect-error
Adcs.Wheels(speed=a.wheels[0].cmd)  # expect-error
Adcs.Wheels(torque=a.est[0])  # expect-error
a.wheels[0].speed  # expect-error
Adcs(fss=[a.est[0], a.est[1], ()])
