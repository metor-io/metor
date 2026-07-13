"""The adcs-fsw2 mission, expressed with the `metor_config` Python front-end.

This is the mission: the coordinator config, artifacts, systems, `mode` slot,
and edges the CLI runner and the tracked tests read, evaluated once at
build/package time into the `Wiring` IR.

Systems and occupants come from generated, `py.typed` pack modules
(`metor-fsw stubgen`): importing `Plant`/`Nav`/`Ctrl` and
`commissioning`/`safe_mode` gives pyright-checked params, ports, and frames,
and each module's `ARTIFACT` is registered implicitly — no `m.artifact(...)`.
Alarms and links stay on the `metor_config` builtins.

    metor-fsw stubgen                                            # regenerate packs/
    metor-fsw run examples/adcs-fsw2/mission.py --build          # headless sim
"""

from metor_config import Alarm, Alarms, Mission, Target, TcpDownlink, TcpUplink, band
from packs.adcs import Ctrl, Nav, Plant
from packs.seqs import commissioning, safe_mode

m = Mission(cycle_rate=120.0, sim_dt=1 / 120)

# The plant's disturbance/actuator environment is spelled out in full, physically
# honest for a ~3 kg spacecraft at 400 km. `process=True` runs it in its own worker.
plant = m.add(
    "plant",
    Plant(
        init_angle=0.5,
        init_rate=0.15,
        meas_sigma=0.002,
        seed=42,
        disarmed=False,
        rho=3e-12,  # atmospheric density (kg/m^3, 400 km solar-mean)
        cd=2.2,  # drag coefficient
        area_aero=0.03,  # aero reference area (m^2)
        cp_offset_b=(0.02, 0.0, 0.0),  # CP-CG offset (m) - the drag/SRP torque arm
        m_res_b=(0.002, 0.002, 0.002),  # residual magnetic dipole (A*m^2)
        area_srp=0.03,  # SRP reference area (m^2)
        cr=1.5,  # SRP reflectivity coefficient
        mtq_max_dipole=0.2,  # per-axis magnetorquer authority (A*m^2)
        init_wheel_h=0.0,  # per-wheel stored-momentum preload (N*m*s)
        init_orbit_phase=0.0,  # in-plane boot phase (rad) - eclipse tests start in shadow
    ),
    process=True,
)
nav = m.add("nav", Nav(meas_sigma=0.02))
ctrl = m.add("ctrl", Ctrl(q_weight=5.0, r_weight=8.0, k_desat=0.0005, k_detumble=0.00005))

alarms = m.add(
    "alarms",
    Alarms(alarms=[
        Alarm(
            id="ADCS_RATE_HIGH",
            name="Body Rate High",
            description="Measured body-Y rate exceeds the detumbled envelope",
            target=Target("plant.sensors.gyro_b", element=1),
            warning=band(above=0.05, below=-0.05),
            critical=band(above=0.15, below=-0.15),
            debounce=2,
            hysteresis=0.005,
        ),
        Alarm(
            id="RW_MOMENTUM_HIGH",
            name="Wheel Momentum High",
            description="Wheel-0 stored momentum approaching the saturation limit",
            # instance `plant`, frame `wheels`, field `wheels: [ReactionWheel; 3]`,
            # wheel 0, its X-axis momentum element.
            target=Target("plant.wheels.wheels.0.ang_momentum", element=0),
            warning=band(above=0.03, below=-0.03),
            critical=band(above=0.038, below=-0.038),
            debounce=2,
            hysteresis=0.001,
        ),
    ]),
)

# Detumble enters only above 1.0 rad/s; the gates/budgets ride the allow line.
mode = m.slot(
    "mode",
    inputs=["attitude_estimate", "gps"],
    outputs=["mode_cmd"],
    allow=[
        commissioning(
            rate_detumble_enter=1.0,
            rate_detumble_exit=0.8,
            est_delta_rad=0.001,
            est_dwell_s=0.2,
            coarse_err_rad=0.2,
            coarse_dwell_s=0.5,
            confirm_dwell_s=1.0,
            warmup_timeout_s=10.0,
            detumble_timeout_s=900.0,
            settle_timeout_s=60.0,
            confirm_timeout_s=30.0,
        ),
        safe_mode(),
    ],
    initial="commissioning",
    initial_state="running",
)

m.connect(plant.sensors, nav.sensors)
m.connect(plant.gps, nav.gps)
m.connect(plant.gps, ctrl.gps)
m.connect(plant.sensors, ctrl.sensors)  # measured B for the magnetorquer laws
m.connect(plant.wheels, ctrl.wheels)  # telemetered wheel momentum for desat
m.connect(nav.attitude_estimate, ctrl.attitude_estimate)
m.connect(nav.attitude_estimate, mode.attitude_estimate)
m.connect(plant.gps, mode.gps)

m.connect(mode.mode_cmd, ctrl.mode_cmd, delayed=True)
m.connect(ctrl.torque_cmd, plant.torque_cmd, delayed=True)
m.connect(ctrl.mtq_cmd, plant.mtq_cmd, delayed=True)

uplink = m.add(
    "uplink",
    TcpUplink(addr="127.0.0.1:2240", msgs=["SequenceCommand", "AlarmAck", "ReloadSequences"]),
)
downlink = m.add("downlink", TcpDownlink(addr="127.0.0.1:2240"))

m.route(uplink, mode, msg="SequenceCommand")  # ground commands
m.route(m.coordinator, mode, msg="SequenceCommand")  # in-proc control_handle
m.route(uplink, alarms, msg="AlarmAck")  # operator acks (gate latching alarms)
m.route(uplink, m.coordinator, msg="ReloadSequences")  # panel Reload -> registry re-emit
