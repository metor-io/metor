"""Mappings pyright accepts."""

from __future__ import annotations

from typing import Literal

from sensors_stub import Body, Plant, SensorsPort

from metor_config import Cal, Calibrated, Field, Mux, Target

target = Target(10.0)
plant = target.add("plant", Plant())
port: SensorsPort = plant.sensors
body = Body(omega_b=plant.sensors.gyro_b, sun=plant.sensors.css, wheel=plant.sensors.wheel)
speed = plant.sensors.wheel.speed
exec_time = plant.status.exec_time_ns
mux = target.add("est", Mux(body=body, on=plant.sensors, max_age=0.5))
target.add("plant2", Plant(body=mux.body))
first: Field[Literal["f64"]] = plant.sensors.gyro_b[0]
Body(omega_b={0: plant.sensors.css[1], 2: first})
linear = Cal.linear(gain=2.0, offset=0.0)
Body(rate=Calibrated(plant.sensors.gyro_b[0], linear), wheel=plant.sensors.wheel)
Body(omega_b={1: Calibrated(plant.sensors.css[0], Cal.thermocouple("K")), 2: first})
