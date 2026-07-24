from metor_config import (
    Alarm,
    AlarmList,
    Alarms,
    Component,
    Downlink,
    HSplit,
    Logs,
    Pane,
    Preset,
    Presets,
    SequenceList,
    Target,
    TcpServer,
    TimeSeriesPlot,
    Trace,
    Uplink,
    VSplit,
    band,
)
from adcs_pack import Ctrl, Nav, Plant
from adcs_seqs import commissioning, safe_mode

m = Target(
    cycle_rate=120.0,
    sim_dt=1 / 120,
    namespace="cube_sat"
)

link = m.state("link", TcpServer(addr="[::]:2240", name="cube_sat"))

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
            target=Component("plant.sensors.gyro_b", element=1),
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
            target=Component("plant.wheels.wheels.0.ang_momentum", element=0),
            warning=band(above=0.03, below=-0.03),
            critical=band(above=0.038, below=-0.038),
            debounce=2,
            hysteresis=0.001,
        ),
    ]),
)

# The layout the panel offers on a fresh connection: rates and wheel momentum
# up top, the operational surfaces (logs, alarms, sequences) below.
presets = m.add(
    "presets",
    Presets([
        Preset(
            name="adcs-ops",
            time_range="LAST 5m",
            layout=VSplit(
                HSplit(
                    TimeSeriesPlot(
                        label="Body Rates",
                        traces=[
                            Trace("plant.sensors.gyro_b", element=i, label=f"gyro_b.{ax}")
                            for i, ax in enumerate("xyz")
                        ]
                        + [
                            Trace("nav.attitude_estimate.omega_b", element=1, label="omega_b.y")
                        ],
                    ),
                    TimeSeriesPlot(
                        label="Wheel Momentum",
                        traces=[
                            Trace(
                                f"plant.wheels.wheels.{w}.ang_momentum",
                                label=f"wheel {w}",
                            )
                            for w in range(3)
                        ],
                    ),
                ),
                HSplit(
                    Logs(),
                    Pane([AlarmList(), SequenceList()]),
                    flexes=[2.0, 1.0],
                ),
                flexes=[2.0, 1.0],
            ),
        ),
    ]),
)

uplink = m.add("uplink", Uplink(link, msgs=["SequenceCommand", "AlarmAck", "ReloadSequences"]))

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
m.connect(plant.sensors, ctrl.sensors)
m.connect(plant.wheels, ctrl.wheels)
m.connect(nav.attitude_estimate, ctrl.attitude_estimate)
m.connect(nav.attitude_estimate, mode.attitude_estimate)
m.connect(plant.gps, mode.gps)

m.connect(mode.mode_cmd, ctrl.mode_cmd, delayed=True)
m.connect(ctrl.torque_cmd, plant.torque_cmd, delayed=True)
m.connect(ctrl.mtq_cmd, plant.mtq_cmd, delayed=True)

downlink = m.add("downlink", Downlink(link))

m.route(uplink, mode, msg="SequenceCommand")  #
m.route(m.coordinator, mode, msg="SequenceCommand")
m.route(uplink, alarms, msg="AlarmAck")
m.route(uplink, m.coordinator, msg="ReloadSequences")
