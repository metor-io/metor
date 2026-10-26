"""Runs the whole ADCS loop in one process, feeding the controller's commands
back to the plant one cycle late.

Nav and ctrl take their GPS fixes from a driver reading a simulated receiver
over a serial port. Nav also fuses star vectors from the star tracker, which
identifies the stars in the images the star simulator renders from the plant's
true attitude. The receiver logs the plant's true orbit state. A db records
everything and serves it on 2250."""

import math
import os

from adcs_pack import (
    Ctrl,
    GpsDriver,
    GpsReceiver,
    Mode,
    MtqCmd,
    Nav,
    Plant,
    StarTracker,
    TorqueCmd,
)
from metor_config import Db, Listen, Publish, SimUart, Target
from startracker_sim_pack import StarSim

# `METOR_DEPLOY_PORTS` (as in target.py) moves the publisher and the db; the
# middle port is unused here.
p_pub, _, p_db = os.environ.get("METOR_DEPLOY_PORTS", "2240,2241,2250").split(",")

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

# Renders a 20° tracker's 1024² images at 10 Hz from the true attitude on its
# own thread, from the committed G ≤ 8 catalog.
star_sim = fsw.add(
    "star_sim",
    StarSim(body=plant.body, world=plant.world, rate_hz=10.0, seed=13),
    thread="startracker",
)
# Centroids each image and identifies its stars against the onboard DB.
tracker = fsw.add(
    "star_tracker",
    StarTracker(image=star_sim.star_image, sensors=plant.sensors),
)
nav = fsw.add(
    "nav",
    Nav(
        sensors=plant.sensors,
        gps=gps.fix,
        stars=tracker.star_vectors,
        star_solution=tracker.star_solution,
        meas_sigma=0.02,
    ),
)
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

# `conn_cap` fits a 2 MiB star image.
fsw.add("pub", Publish(transport=Listen(f"[::]:{p_pub}"), all=True, conn_cap=4 << 20))
# Records the whole loop, GPS included, for a panel on 2250, as target.py's
# gateway does.
fsw.add("db", Db(listen=f"0.0.0.0:{p_db}", all=True), thread="db")
