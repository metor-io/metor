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
    Image,
    Logs,
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
    TrafficLight,
    TrafficLightGrid,
    Uplink,
    VSplit,
    VectorMarker,
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

# --- the operator dashboard -------------------------------------------------
#
# Instrument scales come from the contract constants, so the panel and the
# plant agree on what "full" means: RW_MOMENTUM_MAX = 0.04 N·m·s per wheel and
# MTQ_MAX_DIPOLE = 0.2 A·m² per axis. Warn/critical ticks are *not* set here —
# the meters and gauges read them from the ADCS_RATE_HIGH and RW_MOMENTUM_HIGH
# definitions above, so a limit is stated once and shown everywhere.
RW_MOMENTUM_MAX = 0.04
MTQ_MAX_DIPOLE = 0.2
RATE_FULL_SCALE = 0.2  # rad/s, comfortably past the critical rate band

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
    20, 20, 160, 62,
)
law_chip = Place(
    StateChip(
        "mode.mode_cmd",
        element=1,
        label="law",
        states=[State(0, "NADIR"), State(1, "VELOCITY"), State(2, "B-CROSS")],
    ),
    190, 20, 160, 62,
)
# `illuminated` is a 0/1 flag, so an on/off light says it better than a number.
eclipse = Place(TrafficLight("plant.world.illuminated"), 360, 20, 70, 62)
wheel_arm = Place(TrafficLightGrid("plant.wheels.wheels.*.arm"), 440, 20, 190, 62)
sequence = Place(SequenceControl("mode"), 640, 20, 300, 120)

attitude = Place(
    Attitude(
        "nav.attitude_estimate.q_hat_b_eci",
        label="attitude estimate",
        vectors=[VectorMarker("plant.sensors.mag_b", "mag")],
    ),
    20, 100, 260, 320,
)

# One gauge per body axis: the question here is "where in the envelope is this
# rate", which a dial answers faster than a bar.
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
        300 + 160 * i, 100, 150, 140,
    )
    for i, axis in enumerate("xyz")
]

# Wheel momentum is signed and saturating, so vertical bipolar bars filling
# outward from zero: the direction of the stored momentum is the point.
wheel_meters = [
    Place(
        Meter(
            f"plant.wheels.wheels.{w}.ang_momentum",
            element=0,
            label=f"wheel {w}",
            unit="N·m·s",
            min=-RW_MOMENTUM_MAX,
            max=RW_MOMENTUM_MAX,
        ),
        300 + 92 * w, 260, 84, 210,
    )
    for w in range(3)
]

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
        590, 260 + 68 * i, 350, 60,
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
    970, 100, 520, 240,
)
momentum_plot = Place(
    TimeSeriesPlot(
        label="Wheel Momentum",
        traces=[
            Trace(f"plant.wheels.wheels.{w}.ang_momentum", label=f"wheel {w}")
            for w in range(3)
        ],
    ),
    970, 360, 520, 240,
)

# The bus outline, placed 1:1 with its pixels so the leader lines below can
# name a point on the drawing by its position in the image.
BUS_X, BUS_Y = 20, 470
bus = Place(Image("assets/bus.png"), BUS_X, BUS_Y, 420, 260)


def on_bus(x: float, y: float) -> At:
    """A canvas anchor at image-pixel ``(x, y)`` of the bus drawing."""
    return At(BUS_X + x, BUS_Y + y)


adcs_dashboard = Dashboard(
    title="ADCS Ops",
    widgets=[
        mode_chip, law_chip, eclipse, wheel_arm, sequence,
        attitude, *rate_gauges, *wheel_meters, *mtq_meters,
        rate_plot, momentum_plot, bus,
    ],
    connectors=[
        # Signal flow, drawn under the widgets so each run disappears into the
        # box it enters, the way a schematic should read. `bind` energizes the
        # actuator legs off live telemetry.
        Connector([attitude, rate_gauges[1]], label="ω", arrow="end"),
        Connector(
            [rate_gauges[1], wheel_meters[1]],
            label="control",
            arrow="end",
            bind=Bind("plant.wheels.wheels.1.arm"),
        ),
        Connector(
            [wheel_meters[2], mtq_meters[0]],
            label="desat",
            arrow="end",
            dashed=True,
            bind=Bind("ctrl.mtq_cmd.dipole_b", threshold=1e-6),
        ),
        # Callout leaders from parts of the drawing to the live instrument for
        # that hardware. These paint *over* the widgets, since a leader has to
        # cross what lies between its ends.
        Connector(
            [on_bus(210, 155), Edge(wheel_meters[1], "bottom", 0.5)],
            shape="curved",
            arrow="end",
            on_top=True,
            label="reaction wheels",
        ),
        Connector(
            [on_bus(210, 96), Edge(mtq_meters[1], "left", 0.5)],
            shape="curved",
            arrow="end",
            on_top=True,
            label="magnetorquers",
        ),
        Connector(
            [on_bus(210, 46), Edge(attitude, "bottom", 0.5)],
            shape="curved",
            arrow="end",
            on_top=True,
            label="sun sensor",
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
