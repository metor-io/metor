"""A hand-written stand-in for the pack module `metor pack dev` renders."""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from typing import Sequence  # noqa: E402

from metor_config import Item, OutPort, Pack, Record, Sources, System  # noqa: E402

PACK = Pack(id="adcs", lib="adcs_systems", libs="/abs/.metor/adcs_pack/_libs", abi_version=1)


class Imu(Record):
    _kind = "frame"
    _name = "imu"
    _pack = PACK
    _fields = {"gyro": "f64[3]", "accel": "f64[3]", "css": "f64[6]"}


class LogEvent(Record):
    _kind = "msg"
    _name = "log"
    _pack = PACK


class SystemStatus(Record):
    _kind = "frame"
    _name = "status"
    _pack = PACK


class Est(Record):
    _kind = "frame"
    _name = "est"
    _pack = PACK
    _fields = {
        "q": "f64[4]",
        "bias.a": "f64",
        "bias.b": "f64[3]",
        "p": "f64[6,6]",
        "gains.0": "f64",
        "gains.1": "f64",
    }


class ModeCmd(Record):
    _kind = "msg"
    _name = "mode_cmd"
    _pack = PACK


class MotorCmd(Record):
    _kind = "frame"
    _name = "motor_cmd"
    _pack = PACK
    _fields = {"torque": "f64[3]", "trim.b": "f64[3]", "trim.a": "f64"}


class Ping(Record):
    _kind = "msg"
    _name = "ping"
    _pack = PACK


class AlarmRaised(Record):
    _kind = "msg"
    _name = "alarm_raised"
    _pack = PACK


class AlarmCleared(Record):
    _kind = "msg"
    _name = "alarm_cleared"
    _pack = PACK


class Plant(System):
    """The vehicle."""

    _pack = PACK
    _ty = "plant"
    _ports = {"imu": Imu, "log": LogEvent, "status": SystemStatus}

    def __init__(self, *, motor_cmd: Sources[MotorCmd] = (), altitude: float = 400e3) -> None:
        super().__init__({"motor_cmd": motor_cmd}, {"altitude": altitude})

    imu: OutPort[Imu]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Nav(System):
    _pack = PACK
    _ty = "nav"
    _ports = {"est": Est, "log": LogEvent, "status": SystemStatus}

    def __init__(self, *, imu: Sources[Imu] = (), gain: float = 0.1) -> None:
        super().__init__({"imu": imu}, {"gain": gain})

    est: OutPort[Est]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Mode(System):
    _pack = PACK
    _ty = "mode"
    _ports = {"cmd": ModeCmd, "log": LogEvent, "status": SystemStatus}

    def __init__(self) -> None:
        super().__init__({}, {})

    cmd: OutPort[ModeCmd]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Safe(System):
    """A second mode, with `Mode`'s ports."""

    _pack = PACK
    _ty = "safe"
    _ports = {"cmd": ModeCmd, "log": LogEvent, "status": SystemStatus}

    def __init__(self, *, est: Sources[Est] = (), hold: float = 1.0) -> None:
        super().__init__({"est": est}, {"hold": hold})

    cmd: OutPort[ModeCmd]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Hold(System):
    """A third mode, with `Safe`'s ports and no params."""

    _pack = PACK
    _ty = "hold"
    _ports = {"cmd": ModeCmd, "log": LogEvent, "status": SystemStatus}

    def __init__(self, *, est: Sources[Est] = ()) -> None:
        super().__init__({"est": est}, {})

    cmd: OutPort[ModeCmd]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Ctrl(System):
    _pack = PACK
    _ty = "ctrl"
    _ports = {"motor_cmd": MotorCmd, "log": LogEvent, "status": SystemStatus}

    def __init__(self, *, est: Sources[Est] = (), mode: Sources[ModeCmd] = ()) -> None:
        super().__init__({"est": est, "mode": mode}, {})

    motor_cmd: OutPort[MotorCmd]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Tap(System):
    """A generated type that takes the input ports its config lists."""

    _pack = PACK
    _ty = "tap"
    _takes_inputs = True
    _ports = {"output": Ping, "log": LogEvent, "status": SystemStatus}

    def __init__(self, *, items: Sequence[Item] = (), gain: float = 1.0) -> None:
        super().__init__({}, {"gain": gain}, items=items)

    output: OutPort[Ping]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Fan(System):
    """A generated type that takes the output ports its config lists."""

    _pack = PACK
    _ty = "fan"
    _takes_outputs = True
    _ports = {"log": LogEvent, "status": SystemStatus}

    def __init__(self, *, records: Sequence[type[Record]] = ()) -> None:
        super().__init__({}, {}, records=records)

    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class Burn(System):
    """A sequence: its alarm ports follow its own outputs."""

    _pack = PACK
    _ty = "burn"
    _ports = {
        "cmd": MotorCmd,
        "alarm_raised": AlarmRaised,
        "alarm_cleared": AlarmCleared,
        "log": LogEvent,
        "status": SystemStatus,
    }

    def __init__(self, *, est: Sources[Est] = ()) -> None:
        super().__init__({"est": est}, {})

    cmd: OutPort[MotorCmd]
    alarm_raised: OutPort[AlarmRaised]
    alarm_cleared: OutPort[AlarmCleared]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]
