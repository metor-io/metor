"""Runs the plant, the flight software, and a gateway as three targets.

`plant` simulates the spacecraft, `fsw` runs nav, ctrl, and the `commission`
sequence on its measurements, and the gateway's db records both. `commission`
runs as a wasm module and takes its commands through the gateway.
`METOR_DEPLOY_PORTS` sets the three listen ports so tests can pick free ones.
"""
import math
import os

import adcs_seqs
from adcs_pack import Ctrl, MtqCmd, Nav, Plant, TorqueCmd
from adcs_seqs import Commission
from metor_config import (
    Deployment,
    Listen,
    Peer,
    Publish,
    SequenceCommand,
    Subscribe,
    Target,
    gateway,
)

p_plant, p_fsw, p_db = os.environ.get("METOR_DEPLOY_PORTS", "2240,2241,2250").split(",")

plant = Target(cycle_rate=120.0, namespace="plant")

torque = plant.loop(TorqueCmd)
mtq = plant.loop(MtqCmd)

sim = plant.add(
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
plant_pub = plant.add("pub", Publish(transport=Listen(f"[::]:{p_plant}"), items=[sim]))

fsw = Target(cycle_rate=120.0, namespace="fsw")

# Naming the mirror `plant` gives its components the same paths they have on
# the plant target.
mirror = fsw.add("plant", Subscribe(transport=Peer(plant_pub), items=[sim]))
nav = fsw.add(
    "nav",
    Nav(sensors=mirror.port(sim.sensors), gps=mirror.port(sim.gps), meas_sigma=0.02),
)

fsw.wasm(adcs_seqs.PACK)
# Detumbles, then points at nadir.
commission = fsw.add(
    "commission",
    Commission(estimate=nav.attitude_estimate, rate=0.02, detumble_timeout_s=1800.0),
    channel="commission",
)
ctrl = fsw.add(
    "ctrl",
    Ctrl(
        estimate=nav.attitude_estimate,
        gps=mirror.port(sim.gps),
        mode=commission.mode_cmd,
        sensors=mirror.port(sim.sensors),
        wheels=mirror.port(sim.wheels),
        q_weight=5.0,
        r_weight=8.0,
        k_desat=0.0005,
        k_detumble=0.00005,
    ),
)
fsw_pub = fsw.add("pub", Publish(transport=Listen(f"[::]:{p_fsw}"), all=True))

# The plant applies the controller's commands one cycle late.
ctrl_cmds = plant.add(
    "ctrl", Subscribe(transport=Peer(fsw_pub), items=[ctrl.torque_cmd, ctrl.mtq_cmd])
)
torque.connect(ctrl_cmds.port(ctrl.torque_cmd))
mtq.connect(ctrl_cmds.port(ctrl.mtq_cmd))

gw, db = gateway("gw", peers=[plant_pub, fsw_pub], listen=f"127.0.0.1:{p_db}", cycle_rate=120.0)

# The ground commands `commission` through the gateway's db.
ground_cmds = fsw.add("cmds", Subscribe([SequenceCommand], transport=Peer(db)))
fsw.commands(ground_cmds.sequence_command)

Deployment(targets=[plant, fsw, gw])
