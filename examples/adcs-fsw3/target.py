"""Runs the plant, the flight software, and a gateway as three targets.

`plant` simulates the spacecraft. `fsw` runs its measurements through three
composites: `gnc`, which holds nav, ctrl, and the `commission` sequence;
`monitor`, which checks sensor health; and `downlink`, which sends the beacon.
The gateway's db records both targets. `commission` runs as a wasm module and
takes its commands through the gateway.
`METOR_DEPLOY_PORTS` sets the three listen ports so tests can pick free ones.
"""
import math
import os
from typing import NamedTuple

import adcs_seqs
from adcs_pack import (
    AttitudeEstimatePort,
    Beacon,
    Body,
    Ctrl,
    GpsPort,
    Health,
    Housekeeping,
    Magnetometer,
    MtqCmd,
    MtqCmdPort,
    Nav,
    Plant,
    SensorsPort,
    SunSensors,
    TorqueCmd,
    TorqueCmdPort,
    WheelsPort,
)
from adcs_seqs import Commission
from metor_config import (
    ChannelRegistry,
    Deployment,
    Listen,
    Mux,
    Peer,
    Publish,
    Scope,
    SequenceCommand,
    Subscribe,
    Target,
    composite,
    gateway,
)


class Gnc(NamedTuple):
    nav: Nav
    commission: Commission
    ctrl: Ctrl


@composite
def gnc(s: Scope, *, sensors: SensorsPort, gps: GpsPort, wheels: WheelsPort) -> Gnc:
    """Estimates the attitude, sequences the modes, and commands the actuators."""
    nav = s.add("nav", Nav(sensors=sensors, gps=gps, meas_sigma=0.02))
    # Detumbles, then points at nadir and watches the wheels hold the rate.
    commission = s.add(
        "commission",
        Commission(
            estimate=nav.attitude_estimate, rate=0.02, detumble_timeout_s=1800.0, handover_s=600.0
        ),
        channel="commission",
    )
    ctrl = s.add(
        "ctrl",
        Ctrl(
            estimate=nav.attitude_estimate,
            gps=gps,
            mode=commission.mode_cmd,
            sensors=sensors,
            wheels=wheels,
            q_weight=5.0,
            r_weight=8.0,
            k_desat=0.0005,
            k_detumble=0.00005,
        ),
    )
    # The body state as the flight software knows it: nav's attitude and the GPS
    # fix, emitted with each estimate while the fix is under half a second old.
    s.add(
        "est_body",
        Mux(
            body=Body(
                q_b_eci=nav.attitude_estimate.q_hat_b_eci,
                omega_b=nav.attitude_estimate.omega_b,
                pos_eci=gps.pos_eci,
                vel_eci=gps.vel_eci,
            ),
            on=nav.attitude_estimate,
            max_age=0.5,
        ),
    )
    return Gnc(nav, commission, ctrl)


@composite
def monitor(s: Scope, *, sensors: SensorsPort, gps: GpsPort) -> None:
    """Splits the sensors into one frame per sensor, each emitted with its
    sample, and checks each sensor's health."""
    split = s.add(
        "sensor_split",
        Mux(
            css=SunSensors(css=sensors.css),
            mag=Magnetometer(mag_b=sensors.mag_b),
        ),
    )
    s.add("health", Health(css=split.css, mag=split.mag, gps=gps))


@composite
def downlink(
    s: Scope, *, estimate: AttitudeEstimatePort, torque: TorqueCmdPort, mtq: MtqCmdPort
) -> None:
    """Beacons housekeeping, built every cycle from each source's newest value."""
    hk = s.add(
        "hk",
        Mux(
            hk=Housekeeping(
                omega_b=estimate.omega_b,
                b_hat_b=estimate.b_hat_b,
                torque_b=torque.torque_b,
                dipole_b=mtq.dipole_b,
            ),
            on="cycle",
        ),
    )
    s.add("beacon", Beacon(hk=hk.hk, period_s=10.0))


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
sensors = mirror.port(sim.sensors)
gps = mirror.port(sim.gps)

fsw.wasm(adcs_seqs.PACK)
g = fsw.add("gnc", gnc(sensors=sensors, gps=gps, wheels=mirror.port(sim.wheels)))
fsw.add("monitor", monitor(sensors=sensors, gps=gps))
fsw.add(
    "downlink",
    downlink(estimate=g.nav.attitude_estimate, torque=g.ctrl.torque_cmd, mtq=g.ctrl.mtq_cmd),
)
# Tells the panel which channels the target has.
fsw.add("channels", ChannelRegistry())
fsw_pub = fsw.add("pub", Publish(transport=Listen(f"[::]:{p_fsw}"), all=True))

# The plant applies the controller's commands one cycle late.
ctrl_cmds = plant.add(
    "ctrl", Subscribe(transport=Peer(fsw_pub), items=[g.ctrl.torque_cmd, g.ctrl.mtq_cmd])
)
torque.connect(ctrl_cmds.port(g.ctrl.torque_cmd))
mtq.connect(ctrl_cmds.port(g.ctrl.mtq_cmd))

gw, db = gateway("gw", peers=[plant_pub, fsw_pub], listen=f"127.0.0.1:{p_db}", cycle_rate=120.0)

# The ground commands `gnc.commission` through the gateway's db.
ground_cmds = fsw.add("cmds", Subscribe([SequenceCommand], transport=Peer(db)))
fsw.commands(ground_cmds.sequence_command)

Deployment(targets=[plant, fsw, gw])
