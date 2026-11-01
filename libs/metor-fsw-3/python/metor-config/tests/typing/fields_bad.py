"""Mappings pyright refuses: each `# expect-error` line, and no other."""

from __future__ import annotations

from sensors_stub import Body, Plant

from metor_config import Target

target = Target(10.0)
plant = target.add("plant", Plant())
Body(omega_b=plant.sensors.gyro_c)  # expect-error
Body(omega_b=plant.sensors.css)  # expect-error
Body(sun=plant.sensors.css)
