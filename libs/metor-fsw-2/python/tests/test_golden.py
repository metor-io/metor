"""The cross-language contract: the recorder must emit exactly the shared
``tests/golden/mission.json`` fixture the Rust round-trip test also consumes,
modulo the fields both sides normalize away (``src`` anchors, the located
``path``/``prebuilt_dir``, and the emitter-only ``metor_config_version``
envelope field)."""

import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import metor_config as mc
from metor_config import Alarm, Alarms, Mission, Target, TcpUplink, band

GOLDEN = os.path.join(
    os.path.dirname(__file__), "..", "..", "tests", "golden", "mission.json"
)


def build_mission() -> Mission:
    """The mission whose IR is the golden fixture."""
    m = Mission(cycle_rate=120.0, sim_dt=0.5)
    adcs = m.artifact("adcs", crate="adcs-systems", lib="adcs_systems")
    seqs = m.artifact("seqs", crate="adcs-sequences", lib="adcs_sequences")
    with m.scope("block"):
        plant = m.add(
            "plant",
            adcs.Plant(
                init_angle=0.5,
                init_rate=0.15,
                seed=42,
                disarmed=False,
                cp_offset_b=(0.02, 0.0, 0.0),
            ),
            process=True,
        )
        nav = m.add("nav", adcs.Nav())
    m.add(
        "alarms",
        Alarms(alarms=[
            Alarm(
                id="ADCS_RATE_HIGH",
                name="Body Rate High",
                description="Measured body-Y rate exceeds the detumbled envelope",
                target=Target("block.plant.sensors.gyro_b", element=1),
                warning=band(above=0.05, below=-0.05),
                critical=band(above=0.15, below=-0.15),
                debounce=2,
                hysteresis=0.005,
            )
        ]),
    )
    uplink = m.add("uplink", TcpUplink(addr="127.0.0.1:2240", msgs=["SequenceCommand"]))
    mode = m.slot(
        "mode",
        inputs=["attitude_estimate", "gps"],
        outputs=["mode_cmd"],
        allow=[seqs.commissioning(rate_detumble_enter=1.0), seqs.safe_mode()],
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
        mc._missions.clear()

    def test_emits_the_golden_fixture(self):
        with open(GOLDEN, encoding="utf-8") as f:
            expected = normalize(json.load(f))
        actual = normalize(build_mission().to_ir())
        self.assertEqual(actual, expected)


if __name__ == "__main__":
    unittest.main()
