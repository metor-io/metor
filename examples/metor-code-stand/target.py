"""A test stand whose control code is metor-code.

`chamber_sim` (Rust, the `stand_io` pack) models the chamber, valve and
heater. `leak_check` and `heater` (metor-code, the `stand` pack built from
`stand.mc`) run the leak check and hold the wall temperature. `monitor`
prints what happens.

`STAND_SCENARIO` picks the plant: `nominal` (the check completes), `leak`
(the check fails, after `on abort:` closes the valve), `surge` (the redline
trips while the valve is open and `on abort:` closes it), or `stuck` (the valve
sticks open, the redline trips, and `on abort:` runs).

`cmds` takes `SequenceCommand`s on the first port of `METOR_STAND_PORTS`, and
with `STAND_PANEL=1`, `db` records everything and serves a panel on the
second, whose sequence
control can start, abort or reset `leak_check`.
"""

import os
import sys
from pathlib import Path

# `metor code build stand.mc` writes the `stand` module here.
sys.path.insert(0, str(Path(__file__).resolve().parent / ".metor"))

from metor_config import Db, Listen, SequenceCommand, Subscribe, Target  # noqa: E402
from stand import Heater, LeakCheck  # noqa: E402
from stand_io import ChamberSim, HeaterCmd, Monitor, ValveCmd  # noqa: E402

PLANTS = {
    "nominal": dict(fill=10.0),
    "leak": dict(fill=10.0, leak=0.05),
    "stuck": dict(fill=10.0, stuck=True),
    "surge": dict(fill=4000.0),
}
scenario = os.environ.get("STAND_SCENARIO", "nominal")
p_cmds, p_db = os.environ.get("METOR_STAND_PORTS", "2241,2250").split(",")

fsw = Target(cycle_rate=100.0, namespace="stand")
cmds = fsw.add("cmds", Subscribe([SequenceCommand], transport=Listen(f"127.0.0.1:{p_cmds}")))
fsw.commands(cmds.sequence_command)

valve = fsw.loop(ValveCmd)
heater_cmd = fsw.loop(HeaterCmd)
sim = fsw.add("sim", ChamberSim(valve=valve, heater=heater_cmd, **PLANTS[scenario]))
leak = fsw.add("leak_check", LeakCheck(chamber=sim.chamber, target=150.0), channel="leak_check")
heater = fsw.add("heater", Heater(chamber=sim.chamber, setpoint=40.0))
valve.connect(leak.valve)
heater_cmd.connect(heater.heater)
fsw.add(
    "monitor",
    Monitor(
        status=leak.channel_status,
        valve=leak.valve,
        chamber=sim.chamber,
        heater=heater.heater,
        report=float(os.environ.get("STAND_REPORT", "60.0")),
    ),
)
if os.environ.get("STAND_PANEL"):
    fsw.add("db", Db(listen=f"127.0.0.1:{p_db}", all=True), thread="db")
