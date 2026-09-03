"""The cross-language contract: the recorder must emit exactly the shared
``tests/golden/target.json`` fixture the Rust round-trip test also consumes,
modulo the fields both sides normalize away (``src`` anchors, the located
``path``/``prebuilt_dir``, and the emitter-only ``metor_config_version``
envelope field).

``tests/golden/dashboard.json`` is the same idea one layer down, for the
panel: a dashboard preset covering every widget kind and both connector
layers. The panel's ``DashboardPanelConfig`` test parses that same file, so a
rename on either side breaks a test instead of silently degrading a shipped
preset into placeholder tiles. ``tests/golden/outline.json`` pins the outline
pane's config the same way, every field set."""

import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "metor-config"))

import metor_config as mc
from metor_config import (
    Alarm,
    Alarms,
    Artifact,
    At,
    Attitude,
    Bind,
    Component,
    Connector,
    Dashboard,
    Edge,
    FrameType,
    Gauge,
    Image,
    Meter,
    Outline,
    Pivot,
    Place,
    SequenceControl,
    State,
    StateChip,
    System,
    Target,
    TcpServer,
    Text,
    TimeSeriesPlot,
    Trace,
    TrafficLight,
    TrafficLightGrid,
    Uplink,
    VectorMarker,
    band,
    f64,
    node,
    system,
)

GOLDEN = os.path.join(
    os.path.dirname(__file__), "..", "..", "tests", "golden", "target.json"
)
DASHBOARD_GOLDEN = os.path.join(
    os.path.dirname(__file__), "..", "..", "tests", "golden", "dashboard.json"
)
OUTLINE_GOLDEN = os.path.join(
    os.path.dirname(__file__), "..", "..", "tests", "golden", "outline.json"
)
FIXTURE_PNG = os.path.join(os.path.dirname(__file__), "data", "pixel.png")


def build_dashboard() -> Dashboard:
    """A dashboard exercising every widget kind and both connector layers."""
    meter = Place(
        Meter("wheels.h", element=1, min=-0.04, max=0.04, unit="N m s"), 20, 20
    )
    gauge = Place(
        Gauge("sensors.gyro_b", label="rate y", element=1, min=-0.2, max=0.2,
              style="needle", sweep=200.0),
        120, 20, 150, 140,
    )
    chip = Place(
        StateChip(
            "mode.mode_cmd",
            states=[State(0, "IDLE"), State(3, "SAFE", "#f38ba8ff")],
            unknown="UNKNOWN",
        ),
        280, 20,
    )
    att = Place(
        Attitude(
            "nav.attitude_estimate.q_hat_b_eci",
            vectors=[VectorMarker("sensors.mag_b", "mag", "#89b4faff")],
        ),
        20, 200,
    )
    seq = Place(SequenceControl("mode", compact=True), 280, 100)
    light = Place(TrafficLight("world.illuminated"), 440, 20, 70, 60)
    grid = Place(TrafficLightGrid("wheels.wheels.*.arm"), 520, 20, 190, 60)
    text = Place(Text("ctrl.mtq_cmd.dipole_b"), 440, 100)
    plot = Place(TimeSeriesPlot([Trace("sensors.gyro_b", element=0)]), 440, 200)
    image = Place(Image(FIXTURE_PNG), 20, 520, 200, 140)

    return Dashboard(
        title="Golden",
        widgets=[meter, gauge, chip, att, seq, light, grid, text, plot, image],
        connectors=[
            # Under the widgets, telemetry-coloured: the schematic case.
            Connector(
                [att, gauge, meter],
                label="flow",
                arrow="end",
                bind=Bind("wheels.wheels.0.arm", threshold=0.5),
            ),
            # Over them, with an explicit edge and a free end: the callout case.
            Connector(
                [Edge(image, "top", 0.25), At(700.0, 400.0)],
                shape="curved",
                dashed=True,
                arrow="both",
                on_top=True,
                label="leader",
                color="#f9e2afff",
                width=2.0,
            ),
            Connector([meter, At(900.0, 30.0)], shape="straight"),
        ],
    )


def build_outline() -> Outline:
    """An outline pane with every field set."""
    return Outline(
        root="wheels",
        columns=["name", "value", "unit"],
        sort="descending",
        filter="speed",
        filter_bar=True,
        expanded=["wheels.wheels.0"],
        collapsed=["wheels.status"],
        pivots=[
            Pivot(
                "wheels.wheels",
                fields=["speed", "torque"],
                hidden=["motor.temp"],
                rows=["3", "0"],
            )
        ],
        types=[
            FrameType(
                "psu",
                fields=["current", "voltage"],
                order=["voltage"],
                rows=["dut2.psu"],
            )
        ],
        focus="psu",
    )


def build_target() -> Target:
    """The target whose IR is the golden fixture."""
    m = Target(cycle_rate=120.0, sim_dt=0.5)
    adcs = Artifact(id="adcs", crate="adcs-systems", lib="adcs_systems")
    seqs = Artifact(id="seqs", crate="adcs-sequences", lib="adcs_sequences")

    @system("block.plant.sensors.gyro_b")
    @node(x=420, y=180)
    def gyro_norm(gyro_b) -> f64:
        return (gyro_b @ gyro_b) ** 0.5

    with m.scope("block"):
        plant = m.add(
            "plant",
            System(
                "Plant",
                adcs,
                init_angle=0.5,
                init_rate=0.15,
                seed=42,
                disarmed=False,
                cp_offset_b=(0.02, 0.0, 0.0),
            ),
            process=True,
        )
        nav = m.add("nav", System("Nav", adcs))
        # A Python system added like any native one: scoped, renamed, and
        # interleaved into the step order at this position.
        m.add("gyro_norm", gyro_norm)
    m.add(
        "alarms",
        Alarms(alarms=[
            Alarm(
                id="ADCS_RATE_HIGH",
                name="Body Rate High",
                description="Measured body-Y rate exceeds the detumbled envelope",
                target=Component("block.plant.sensors.gyro_b", element=1),
                warning=band(above=0.05, below=-0.05),
                critical=band(above=0.15, below=-0.15),
                debounce=2,
                hysteresis=0.005,
            )
        ]),
        node=(40, 80),
    )
    link = m.state("link", TcpServer(addr="127.0.0.1:2240"))
    uplink = m.add("uplink", Uplink(link, msgs=["SequenceCommand"]))
    mode = m.slot(
        "mode",
        inputs=["attitude_estimate", "gps"],
        outputs=["mode_cmd"],
        allow=[
            System("commissioning", seqs, rate_detumble_enter=1.0),
            System("safe_mode", seqs),
        ],
        initial="commissioning",
        initial_state="running",
    )
    m.connect(plant.sensors, nav.sensors)
    m.connect(nav.attitude_estimate, mode.attitude_estimate)
    m.connect(mode.mode_cmd, plant.torque_cmd, delayed=True)
    m.route(uplink, mode, msg="SequenceCommand")
    m.route(m.coordinator, mode, msg="SequenceCommand")
    return m


def normalize(v):
    """Drop the fields the cross-language comparison ignores: every ``src``
    anchor, the top-level ``metor_config_version`` envelope, and each artifact's
    located ``path``/``prebuilt_dir``."""
    if isinstance(v, dict):
        v = {k: normalize(x) for k, x in v.items() if k != "src"}
        v.pop("metor_config_version", None)
        for a in v.get("artifacts", []):
            a.pop("path", None)
            a.pop("prebuilt_dir", None)
        return v
    if isinstance(v, list):
        return [normalize(x) for x in v]
    return v


class GoldenTest(unittest.TestCase):
    def setUp(self):
        mc._targets.clear()
        mc._program.clear()

    def test_emits_the_golden_fixture(self):
        with open(GOLDEN, encoding="utf-8") as f:
            expected = normalize(json.load(f))
        actual = normalize(build_target().to_ir())
        self.assertEqual(actual, expected)

    def test_emits_the_golden_dashboard(self):
        # `sat1` so the fixture also pins namespace qualification of every
        # component reference a widget or a binding carries.
        Target(cycle_rate=100.0, namespace="sat1")
        with open(DASHBOARD_GOLDEN, encoding="utf-8") as f:
            expected = json.load(f)
        self.assertEqual(build_dashboard()._state("sat1"), expected)

    def test_emits_the_golden_outline(self):
        with open(OUTLINE_GOLDEN, encoding="utf-8") as f:
            expected = json.load(f)
        self.assertEqual(build_outline()._state("sat1"), expected)

    def test_outline_rejects_unknown_columns_and_sorts(self):
        with self.assertRaises(ValueError):
            Outline(columns=["names"])
        with self.assertRaises(ValueError):
            Outline(sort="up")


if __name__ == "__main__":
    unittest.main()
