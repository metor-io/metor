"""The deployment: the vehicle on `plant`, the flight software on `fsw`, and a
gateway whose db records both.

`METOR_DEPLOY_PORTS` names the three listen ports, so tests do not collide.
"""

import math
import os

from adcs_pack import Ctrl, Mode, MtqCmd, Nav, Plant, TorqueCmd
from metor_config import Deployment, Publish, Subscribe, Target, gateway

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
        # A 400 km sun-synchronous orbit, ascending node at ~10:30 local solar
        # time, so the boot is sunlit.
        altitude=400e3,
        inclination=math.radians(97.03),
        ltan_hours=10.5,
        init_orbit_phase=0.0,
    ),
)
plant_pub = plant.add("pub", Publish(listen=f"127.0.0.1:{p_plant}", items=[sim]))

fsw = Target(cycle_rate=120.0, namespace="fsw")

# The mirror is named `plant`, so every component path a dashboard already uses
# reads the same on this member.
mirror = fsw.add("plant", Subscribe(peer=plant_pub, items=[sim]))
nav = fsw.add(
    "nav",
    Nav(sensors=mirror.port(sim.sensors), gps=mirror.port(sim.gps), meas_sigma=0.02),
)
mode = fsw.add("mode", Mode())
ctrl = fsw.add(
    "ctrl",
    Ctrl(
        estimate=nav.attitude_estimate,
        gps=mirror.port(sim.gps),
        mode=mode.mode_cmd,
        sensors=mirror.port(sim.sensors),
        wheels=mirror.port(sim.wheels),
        q_weight=5.0,
        r_weight=8.0,
        k_desat=0.0005,
        k_detumble=0.00005,
    ),
)
fsw_pub = fsw.add("pub", Publish(listen=f"127.0.0.1:{p_fsw}", all=True))

# The commands come back the other way: the plant actuates on what the
# controller sent, one cycle late.
cmds = plant.add("ctrl", Subscribe(peer=fsw_pub, items=[ctrl.torque_cmd, ctrl.mtq_cmd]))
torque.connect(cmds.port(ctrl.torque_cmd))
mtq.connect(cmds.port(ctrl.mtq_cmd))

gw, _db = gateway("gw", peers=[plant_pub, fsw_pub], listen=f"127.0.0.1:{p_db}", cycle_rate=120.0)

Deployment(targets=[plant, fsw, gw])
