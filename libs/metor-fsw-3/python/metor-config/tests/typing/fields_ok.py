"""Mappings pyright accepts."""

from __future__ import annotations

from sensors_stub import Body, Plant, SensorsPort

from metor_config import Mux, Target

target = Target(10.0)
plant = target.add("plant", Plant())
port: SensorsPort = plant.sensors
body = Body(omega_b=plant.sensors.gyro_b, sun=plant.sensors.css, wheel=plant.sensors.wheel)
speed = plant.sensors.wheel.speed
exec_time = plant.status.exec_time_ns
mux = target.add("est", Mux(body=body, on=plant.sensors, max_age=0.5))
target.add("plant2", Plant(body=mux.body))
