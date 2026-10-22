"""Runs the whole ADCS loop in one process, feeding the controller's commands
back to the plant one cycle late.

Nav and ctrl take their GPS fixes from a driver reading a simulated receiver
over a serial port. The receiver logs the plant's true orbit state. A db
records everything and serves it on 2250."""

import math

from adcs_pack import Ctrl, GpsDriver, GpsReceiver, Mode, MtqCmd, Nav, Plant, TorqueCmd
from metor_config import Db, Listen, Publish, SimUart, Target

fsw = Target(cycle_rate=120.0, namespace="sim")

torque = fsw.loop(TorqueCmd)
mtq = fsw.loop(MtqCmd)

plant = fsw.add(
    "plant",
    Plant(
        torque_cmd=torque,
        mtq_cmd=mtq,
        init_angle=0.5,
        init_rate=0.15,
        meas_sigma=0.002,
        seed=42,
        disarmed=False,
        rho=3e-12,
        cd=2.2,
        area_aero=0.03,
        cp_offset_b=[0.02, 0.0, 0.0],
        m_res_b=[0.002, 0.002, 0.002],
        area_srp=0.03,
        cr=1.5,
        mtq_max_dipole=0.2,
        init_wheel_h=0.0,
        # A 400 km sun-synchronous orbit with the ascending node at 10:30 local
        # solar time, which starts the run in sunlight.
        altitude=400e3,
        inclination=math.radians(97.03),
        ltan_hours=10.5,
        init_orbit_phase=0.0,
    ),
)
gps_uart = fsw.resource("gps_uart", SimUart())
gps = fsw.add("gps", GpsDriver(uart=gps_uart))
fsw.sim("gps_rx", GpsReceiver(uart=gps_uart.device, truth=plant.body, seed=7))

nav = fsw.add("nav", Nav(sensors=plant.sensors, gps=gps.fix, meas_sigma=0.02))
mode = fsw.add("mode", Mode())
ctrl = fsw.add(
    "ctrl",
    Ctrl(
        estimate=nav.attitude_estimate,
        gps=gps.fix,
        mode=mode.mode_cmd,
        sensors=plant.sensors,
        wheels=plant.wheels,
        q_weight=5.0,
        r_weight=8.0,
        k_desat=0.0005,
        k_detumble=0.00005,
    ),
)

torque.connect(ctrl.torque_cmd)
mtq.connect(ctrl.mtq_cmd)

fsw.add("pub", Publish(transport=Listen("[::]:2240"), all=True))
# Records the whole loop, GPS included, for a panel on 2250, as target.py's
# gateway does.
fsw.add("db", Db(listen="0.0.0.0:2250", all=True), thread="db")
