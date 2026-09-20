"""A hand-written stand-in for the pack module `metor pack dev` renders."""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from metor_config import OutPort, Pack, Record, Sources, System  # noqa: E402

PACK = Pack(id="adcs", lib="adcs_systems", libs="/abs/.metor/adcs_pack/_libs", abi_version=1)


class Imu(Record):
    _name = "imu"
    _pack = PACK


class LogEvent(Record):
    _name = "log"
    _pack = PACK


class SystemStatus(Record):
    _name = "status"
    _pack = PACK


class Est(Record):
    _name = "est"
    _pack = PACK


class ModeCmd(Record):
    _name = "mode_cmd"
    _pack = PACK


class MotorCmd(Record):
    _name = "motor_cmd"
    _pack = PACK


class Ping(Record):
    _name = "ping"
    _pack = PACK


class Plant(System):
    """The vehicle."""

    _pack = PACK
    _ty = "plant"
    _outputs = ("imu",)

    def __init__(self, *, motor_cmd: Sources[MotorCmd] = (), altitude: float = 400e3) -> None:
        super().__init__({"motor_cmd": motor_cmd}, {"altitude": altitude})

    imu: OutPort[Imu]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Nav(System):
    _pack = PACK
    _ty = "nav"
    _outputs = ("est",)

    def __init__(self, *, imu: Sources[Imu] = (), gain: float = 0.1) -> None:
        super().__init__({"imu": imu}, {"gain": gain})

    est: OutPort[Est]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Mode(System):
    _pack = PACK
    _ty = "mode"
    _outputs = ("cmd",)

    def __init__(self) -> None:
        super().__init__({}, {})

    cmd: OutPort[ModeCmd]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Ctrl(System):
    _pack = PACK
    _ty = "ctrl"
    _outputs = ("motor_cmd",)

    def __init__(self, *, est: Sources[Est] = (), mode: Sources[ModeCmd] = ()) -> None:
        super().__init__({"est": est, "mode": mode}, {})

    motor_cmd: OutPort[MotorCmd]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]
