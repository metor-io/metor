import math

from metor_config import (
    Alarm,
    AlarmList,
    Alarms,
    At,
    Attitude,
    Bind,
    Component,
    Connector,
    Dashboard,
    Downlink,
    Edge,
    Gauge,
    HSplit,
    Logs,
    Map,
    Meter,
    Pane,
    Place,
    Preset,
    Presets,
    SequenceControl,
    SequenceList,
    State,
    StateChip,
    Target,
    TcpServer,
    TimeSeriesPlot,
    Trace,
    TrafficLightGrid,
    Uplink,
    VSplit,
    VectorMarker,
    band,
    f64,
    node,
    system,
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
        # A 400 km sun-synchronous orbit, ascending node at ~10:30 local solar time
        # (morning SSO - the phase-zero boot is sunlit).
        altitude=400e3,  # orbit altitude (m)
        inclination=math.radians(97.03),  # SSO inclination at 400 km
        ltan_hours=10.5,  # local time of ascending node (hours)
        # The old equatorial orbit:
        # inclination=0.0,
        # ltan_hours=12.0,
        init_orbit_phase=0.0,  # in-plane boot phase (rad) - eclipse tests start in shadow
    ),
    process=True,
)
nav = m.add("nav", Nav(meas_sigma=0.02))
ctrl = m.add("ctrl", Ctrl(q_weight=5.0, r_weight=8.0, k_desat=0.0005, k_detumble=0.00005))

# A Python system, compiled at build time into an ordinary wasm pack entry
# and run by the vehicle like any other cyclic system: the measured body-rate
# magnitude, published as `cube_sat.gyro_norm.gyro_norm`. The decorator only
# declares it; the `add` registers it, at this position in the step order.
@system("plant.sensors.gyro_b")
@node(x=980, y=40)
def gyro_norm(gyro_b) -> f64:
    return (gyro_b @ gyro_b) ** 0.5


m.add("gyro_norm", gyro_norm)

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

RW_MOMENTUM_MAX = 0.04
MTQ_MAX_DIPOLE = 0.2
RATE_FULL_SCALE = 0.2  # rad/s, comfortably past the critical rate band

GUTTER = 14
LANE = 40  # a gutter wide enough for a connector and its label

# Columns: status/attitude on the left, instruments in the middle, plots right.
COL_A = 20                       # attitude
COL_A_W = 270
COL_B = COL_A + COL_A_W + LANE   # gauges, wheels, sequence, torquers
COL_B_W = 534
COL_C = COL_B + COL_B_W + 30     # plots
COL_C_W = 560

ROW_STATUS = 20
ROW_MAIN = ROW_STATUS + 64 + GUTTER + 6      # 104
ROW_WHEELS = ROW_MAIN + 150 + LANE           # 294
ROW_TORQUERS = ROW_WHEELS + 150 + 30         # 474

mode_chip = Place(
    StateChip(
        "mode.mode_cmd",
        element=0,
        label="phase",
        states=[
            State(0, "IDLE"),
            State(1, "SETTLING", "#f9e2afff"),
            State(2, "POINTING", "#a6e3a1ff"),
            State(3, "SAFE", "#f38ba8ff"),
            State(4, "DETUMBLE", "#cba6f7ff"),
        ],
    ),
    COL_A, ROW_STATUS, 150, 64,
)
law_chip = Place(
    StateChip(
        "mode.mode_cmd",
        element=1,
        label="law",
        states=[State(0, "NADIR"), State(1, "VELOCITY"), State(2, "B-CROSS")],
    ),
    COL_A + 164, ROW_STATUS, 150, 64,
)
# A chip rather than a traffic light: `illuminated` is a 0/1 flag, but an
# unlabelled square asks the operator to remember which way round it reads.
eclipse = Place(
    StateChip(
        "plant.world.illuminated",
        label="sun",
        states=[State(0, "ECLIPSE", "#585b70ff"), State(1, "SUNLIT", "#f9e2afff")],
    ),
    COL_A + 328, ROW_STATUS, 150, 64,
)
wheel_arm = Place(
    TrafficLightGrid("plant.wheels.wheels.*.arm"), COL_A + 492, ROW_STATUS, 130, 64
)

attitude = Place(
    Attitude(
        "nav.attitude_estimate.q_hat_b_eci",
        label="attitude estimate",
        vectors=[VectorMarker("plant.sensors.mag_b", "mag")],
    ),
    COL_A, ROW_MAIN, COL_A_W, 340,
)

# One gauge per body axis: the question here is "where in the envelope is this
# rate", which a dial answers faster than a bar.
GAUGE_W = 170
rate_gauges = [
    Place(
        Gauge(
            "plant.sensors.gyro_b",
            element=i,
            label=f"ω {axis}",
            unit="rad/s",
            min=-RATE_FULL_SCALE,
            max=RATE_FULL_SCALE,
        ),
        COL_B + (GAUGE_W + GUTTER - 2) * i, ROW_MAIN, GAUGE_W, 150,
    )
    for i, axis in enumerate("xyz")
]

# Wheel momentum is signed and saturating, so vertical bipolar bars filling
# outward from zero: the direction of the stored momentum is the point.
WHEEL_W = 100
wheel_meters = [
    Place(
        Meter(
            f"plant.wheels.wheels.{w}.ang_momentum",
            element=w,
            label=f"wheel {w}",
            unit="N·m·s",
            min=-RW_MOMENTUM_MAX,
            max=RW_MOMENTUM_MAX,
        ),
        COL_B + (WHEEL_W + GUTTER) * w, ROW_WHEELS, WHEEL_W, 150,
    )
    for w in range(3)
]

sequence = Place(
    SequenceControl("mode"),
    COL_B + (WHEEL_W + GUTTER) * 3, ROW_WHEELS,
    COL_B_W - (WHEEL_W + GUTTER) * 3, 150,
)

mtq_meters = [
    Place(
        Meter(
            "ctrl.mtq_cmd.dipole_b",
            element=i,
            label=f"MTQ {axis}",
            unit="A·m²",
            min=-MTQ_MAX_DIPOLE,
            max=MTQ_MAX_DIPOLE,
            orientation="horizontal",
        ),
        COL_B, ROW_TORQUERS + 68 * i, COL_B_W, 56,
    )
    for i, axis in enumerate("xyz")
]

rate_plot = Place(
    TimeSeriesPlot(
        label="Body Rates",
        traces=[
            Trace("plant.sensors.gyro_b", element=i, label=f"gyro_b.{ax}")
            for i, ax in enumerate("xyz")
        ],
    ),
    COL_C, ROW_MAIN, COL_C_W, 250,
)
momentum_plot = Place(
    TimeSeriesPlot(
        label="Wheel Momentum",
        traces=[
            Trace(f"plant.wheels.wheels.{w}.ang_momentum", label=f"wheel {w}")
            for w in range(3)
        ],
    ),
    COL_C, ROW_MAIN + 250 + 30, COL_C_W, 250,
)

# Signal flow, under the widgets so each run disappears into the box it enters
# the way a schematic should read. Waypoints put every leg down the middle of a
# lane rather than letting the router graze a widget edge, and `bind` energizes
# the actuator legs off live telemetry.
LANE_AB = COL_A + COL_A_W + LANE // 2          # between attitude and the gauges
LANE_GAUGE_WHEEL = ROW_MAIN + 150 + LANE // 2  # between the gauges and wheels


def _center_x(place: Place) -> float:
    return place.x + (place.w or 0) / 2


adcs_dashboard = Dashboard(
    title="ADCS Ops",
    widgets=[
        mode_chip, law_chip, eclipse, wheel_arm,
        attitude, *rate_gauges, *wheel_meters, sequence, *mtq_meters,
        rate_plot, momentum_plot,
    ],
    connectors=[
        Connector(
            [
                Edge(attitude, "right", 0.5),
                At(LANE_AB, attitude.y + 170),
                At(LANE_AB, rate_gauges[0].y + 75),
                Edge(rate_gauges[0], "left", 0.5),
            ],
            shape="straight",
            arrow="end",
            label="ω",
        ),
        Connector(
            [
                Edge(rate_gauges[1], "bottom", 0.5),
                At(_center_x(rate_gauges[1]), LANE_GAUGE_WHEEL),
                At(_center_x(wheel_meters[1]), LANE_GAUGE_WHEEL),
                Edge(wheel_meters[1], "top", 0.5),
            ],
            shape="straight",
            arrow="end",
            label="control",
            bind=Bind("plant.wheels.wheels.1.arm"),
        ),
    ],
)

# `adcs-dashboard` leads the list, so a panel with no saved layout for this
# target opens on it; `adcs-ops` stays available from the preset palette for
# the plot-centric view that suits debugging a control loop.
presets = m.add(
    "presets",
    Presets([
        Preset(name="adcs-dashboard", time_range="LAST 5m", layout=adcs_dashboard),
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
        # The GPS fix on a world map: the ground track the orbit traces out.
        Preset(
            name="adcs-ground-track",
            time_range="LAST 5m",
            layout=Map("plant.gps.lla"),
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

m.route(uplink, mode, msg="SequenceCommand")
m.route(m.coordinator, mode, msg="SequenceCommand")
m.route(uplink, alarms, msg="AlarmAck")
m.route(uplink, m.coordinator, msg="ReloadSequences")
