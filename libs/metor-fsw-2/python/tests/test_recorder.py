"""Recorder surface, scopes, provenance, and the record-time error cases."""

import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import metor_config as mc
from metor_config import (
    Alarm,
    Alarms,
    Mission,
    Target,
    TcpDownlink,
    TcpUplink,
    band,
    static_system,
)


class RecorderTest(unittest.TestCase):
    def setUp(self):
        # The exactly-one-Mission tracker is module-global; isolate each test.
        mc._missions.clear()

    def test_coordinator_clock_and_knobs(self):
        wall = Mission(cycle_rate=100.0).to_ir()["coordinator"]
        self.assertEqual(wall["clock"], "Wall")
        self.assertEqual(wall["cycle_rate"], 100.0)
        self.assertIsNone(wall["default_depth"])

        sim = Mission(cycle_rate=120.0, sim_dt=0.5, default_depth=8).to_ir()["coordinator"]
        self.assertEqual(sim["clock"], {"Simulated": {"dt_secs": 0.5}})
        self.assertEqual(sim["default_depth"], 8)

    def test_add_records_system_and_ports(self):
        m = Mission(cycle_rate=100.0)
        a = m.add("a", static_system("Alarms"))
        b = m.add("b", TcpDownlink(addr="127.0.0.1:2240"))
        m.connect(a.out, b.feed)
        ir = m.to_ir()
        self.assertEqual([s["name"] for s in ir["systems"]], ["a", "b"])
        self.assertEqual(ir["systems"][0]["ty"], "Alarms")
        self.assertIsNone(ir["systems"][0]["artifact"])
        self.assertEqual(ir["systems"][0]["params"], "None")
        edge = ir["edges"][0]
        self.assertEqual((edge["from"], edge["out"], edge["to"], edge["in_"]), ("a", "out", "b", "feed"))
        self.assertEqual(edge["kind"], "Frame")
        self.assertFalse(edge["delayed"])

    def test_loaded_system_via_artifact_handle(self):
        m = Mission(cycle_rate=100.0)
        adcs = m.artifact("adcs", crate="adcs-systems", lib="adcs_systems")
        m.add("plant", adcs.Plant(gain=1.0), process=True)
        m.add("nav", adcs["Nav"]())  # item access yields the same entry callable
        ir = m.to_ir()
        self.assertEqual(ir["artifacts"][0]["id"], "adcs")
        self.assertEqual(ir["artifacts"][0]["crate_name"], "adcs-systems")
        self.assertEqual(ir["artifacts"][0]["lib"], "adcs_systems")
        self.assertIsNone(ir["artifacts"][0]["path"])
        plant = ir["systems"][0]
        self.assertEqual(plant["ty"], "Plant")
        self.assertEqual(plant["artifact"], "adcs")
        self.assertEqual(plant["params"], {"Value": {"gain": 1.0}})
        self.assertTrue(plant["process"])

    def test_delayed_and_message_edges(self):
        m = Mission(cycle_rate=100.0)
        ctrl = m.add("ctrl", static_system("Ctrl"))
        plant = m.add("plant", static_system("Plant"))
        uplink = m.add("uplink", TcpUplink(addr="127.0.0.1:2240", msgs=["Cmd"]))
        m.connect(ctrl.torque_cmd, plant.torque_cmd, delayed=True)
        m.route(uplink, plant, msg="Cmd")
        m.route(m.coordinator, plant, msg="Cmd")
        edges = m.to_ir()["edges"]
        self.assertTrue(edges[0]["delayed"])
        self.assertEqual(edges[1]["kind"], "Msg")
        self.assertEqual((edges[1]["out"], edges[1]["in_"]), ("Cmd", "Cmd"))
        self.assertEqual(edges[2]["from"], "coordinator")

    def test_scope_nesting_and_indices(self):
        m = Mission(cycle_rate=100.0)
        with m.scope("outer"):
            outer = m.add("sys", static_system("A"))
            with m.scope("inner"):
                inner = m.add("sys", static_system("B"))
        top = m.add("sys", static_system("C"))  # unscoped, no collision
        ir = m.to_ir()
        self.assertEqual(outer.name, "outer.sys")
        self.assertEqual(inner.name, "outer.inner.sys")
        self.assertEqual(top.name, "sys")
        self.assertEqual([s["path"] for s in ir["scopes"]], ["outer", "outer.inner"])
        self.assertEqual([s["parent"] for s in ir["scopes"]], [None, 0])
        self.assertEqual([s["scope"] for s in ir["systems"]], [0, 1, None])

    def test_slot_records_allow_and_initial(self):
        m = Mission(cycle_rate=100.0)
        seqs = m.artifact("seqs", crate="adcs-sequences", lib="adcs_sequences")
        m.slot(
            "mode",
            inputs=["gps"],
            outputs=["cmd"],
            allow=[seqs.commissioning(rate=1.0), seqs.safe_mode()],
            initial="commissioning",
            initial_state="running",
        )
        slot = m.to_ir()["slots"][0]
        self.assertEqual(slot["inputs"], ["gps"])
        self.assertEqual(slot["allow"][0]["occupant"], "commissioning")
        self.assertEqual(slot["allow"][0]["artifact"], "seqs")
        self.assertEqual(slot["allow"][0]["params"], {"Value": {"rate": 1.0}})
        self.assertEqual(slot["allow"][1]["params"], "None")
        self.assertEqual(slot["initial"], {"occupant": "commissioning", "state": "Running"})

    def test_source_ref_anchors_to_this_file(self):
        m = Mission(cycle_rate=100.0)
        m.add("a", static_system("A"))
        src = m.to_ir()["systems"][0]["src"]
        self.assertTrue(src["file"].endswith("test_recorder.py"))
        self.assertGreater(src["line"], 0)
        self.assertEqual(src["col"], 1)

    def test_alarms_emit_the_rust_field_names(self):
        spec = Alarms(alarms=[
            Alarm(
                id="RATE_HIGH",
                name="Rate High",
                description="body rate high",
                target=Target("plant.gyro", element=1),
                warning=band(above=0.05, below=-0.05),
                critical=band(above=0.15),
                debounce=2,
                hysteresis=0.005,
            )
        ])
        params = spec._param_source()["Value"]
        self.assertIn("alarm", params)  # the field is singular in AlarmsParams
        a = params["alarm"][0]
        self.assertEqual(a["target"], {"component": "plant.gyro", "element": 1})
        self.assertEqual(a["warning"], {"above": 0.05, "below": -0.05})
        self.assertEqual(a["critical"], {"above": 0.15})  # below omitted
        self.assertNotIn("latching", a)  # unset optionals are omitted
        self.assertNotIn("severity", a)

    def test_uplink_downlink_shapes(self):
        up = TcpUplink(addr="127.0.0.1:2240", msgs=["Cmd"])._param_source()["Value"]
        self.assertEqual(up, {"addr": "127.0.0.1:2240", "msgs": ["Cmd"]})
        self.assertEqual(TcpDownlink(addr="1.2.3.4:5")._param_source()["Value"], {"addr": "1.2.3.4:5"})

    # -- error cases --------------------------------------------------------

    def test_duplicate_instance_name(self):
        m = Mission(cycle_rate=100.0)
        m.add("a", static_system("A"))
        with self.assertRaisesRegex(ValueError, "duplicate instance name 'a'"):
            m.add("a", static_system("B"))

    def test_unknown_initial_occupant(self):
        m = Mission(cycle_rate=100.0)
        seqs = m.artifact("seqs", crate="c", lib="l")
        with self.assertRaisesRegex(ValueError, "initial occupant 'missing'"):
            m.slot("mode", inputs=[], outputs=[], allow=[seqs.safe_mode()], initial="missing")

    def test_non_json_param_names_the_key(self):
        m = Mission(cycle_rate=100.0)
        with self.assertRaisesRegex(TypeError, "'bad'"):
            m.add("a", static_system("A", bad=object()))

    def test_dataclass_rejects_unknown_kwargs(self):
        with self.assertRaises(TypeError):
            Target("x", bogus=1)
        with self.assertRaises(TypeError):
            band(above=1.0, sideways=2.0)
        with self.assertRaises(TypeError):
            Alarm(id="x", name="n", target=Target("c"), nope=1)

    def test_exactly_one_mission_rule(self):
        mc._missions.clear()
        with self.assertRaisesRegex(RuntimeError, "found 0"):
            mc.emit()
        Mission(cycle_rate=1.0)
        Mission(cycle_rate=2.0)
        with self.assertRaisesRegex(RuntimeError, "found 2"):
            mc.emit()


if __name__ == "__main__":
    unittest.main()
