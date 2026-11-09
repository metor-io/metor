"""Mappings pyright refuses: each `# expect-error` line, and no other."""

from __future__ import annotations

from typing import Literal

from sensors_stub import Body, Plant

from metor_config import Cal, Calibrated, Field, Target

target = Target(10.0)
plant = target.add("plant", Plant())
Body(omega_b=plant.sensors.gyro_c)  # expect-error
Body(omega_b=plant.sensors.css)  # expect-error
Body(sun=plant.sensors.css)
Body(omega_b={0: plant.sensors.gyro_b})  # expect-error
whole: Field[Literal["f64[3]"]] = plant.sensors.gyro_b[0]  # expect-error
linear = Cal.linear(gain=2.0, offset=0.0)
Body(rate=Calibrated(plant.sensors.gyro_b, linear))  # expect-error
Body(omega_b=Calibrated(plant.sensors.gyro_b[0], linear))  # expect-error
Body(rate=Calibrated(plant.sensors.wheel.speed, linear))
Cal.thermocouple("S")  # expect-error
