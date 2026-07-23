"""Recorder surface, scopes, provenance, and the record-time error cases."""

import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "metor-config"))

import metor_config as mc
from metor_config import (
    Alarm,
    Alarms,
    Component,
    Target,
    Downlink,
    TcpServer,
    Uplink,
    band,
    static_system,
)


class RecorderTest(unittest.TestCase):
    def setUp(self):
        # The exactly-one-Target tracker is module-global; isolate each test.
        mc._targets.clear()

    def test_coordinator_clock_and_knobs(self):
        wall = Target(cycle_rate=100.0).to_ir()["coordinator"]
        self.assertEqual(wall["clock"], "Wall")
        self.assertEqual(wall["cycle_rate"], 100.0)
        self.assertIsNone(wall["default_depth"])

        sim = Target(cycle_rate=120.0, sim_dt=0.5, default_depth=8).to_ir()["coordinator"]
        self.assertEqual(sim["clock"], {"Simulated": {"dt_secs": 0.5}})
        self.assertEqual(sim["default_depth"], 8)

    def test_namespace_emission_and_validation(self):
        # Absent by default: an un-namespaced target emits a null namespace,
        # keeping component ids identical to before.
        self.assertIsNone(Target(cycle_rate=100.0).to_ir()["coordinator"]["namespace"])

        # A dotted namespace rides in the coordinator dict verbatim.
        ns = Target(cycle_rate=100.0, namespace="fleet.sat1").to_ir()["coordinator"]
        self.assertEqual(ns["namespace"], "fleet.sat1")

        # Malformed namespaces are rejected at construction.
        for bad in ["", ".sat1", "sat1.", "sat1..a"]:
            mc._targets.clear()
            with self.assertRaises(ValueError):
                Target(cycle_rate=100.0, namespace=bad)

    def test_add_records_system_and_ports(self):
        m = Target(cycle_rate=100.0)
        link = m.state("link", TcpServer(addr="127.0.0.1:2240"))
        a = m.add("a", static_system("Alarms"))
        b = m.add("b", Downlink(link))
        m.connect(a.out, b.feed)
        ir = m.to_ir()
        self.assertEqual([s["name"] for s in ir["systems"]], ["a", "b"])
        self.assertEqual(ir["systems"][0]["ty"], "Alarms")
        self.assertIsNone(ir["systems"][0]["artifact"])
        self.assertEqual(ir["systems"][0]["params"], "None")
        self.assertIsNone(ir["systems"][0]["attach"])
        self.assertEqual(ir["systems"][1]["attach"], "link")
        edge = ir["edges"][0]
        self.assertEqual((edge["from"], edge["out"], edge["to"], edge["in_"]), ("a", "out", "b", "feed"))
        self.assertEqual(edge["kind"], "Frame")
        self.assertFalse(edge["delayed"])

    def test_loaded_system_via_artifact_handle(self):
        m = Target(cycle_rate=100.0)
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
        m = Target(cycle_rate=100.0)
        link = m.state("link", TcpServer(addr="127.0.0.1:2240"))
        ctrl = m.add("ctrl", static_system("Ctrl"))
        plant = m.add("plant", static_system("Plant"))
        uplink = m.add("uplink", Uplink(link, msgs=["Cmd"]))
        m.connect(ctrl.torque_cmd, plant.torque_cmd, delayed=True)
        m.route(uplink, plant, msg="Cmd")
        m.route(m.coordinator, plant, msg="Cmd")
        edges = m.to_ir()["edges"]
        self.assertTrue(edges[0]["delayed"])
        self.assertEqual(edges[1]["kind"], "Msg")
        self.assertEqual((edges[1]["out"], edges[1]["in_"]), ("Cmd", "Cmd"))
        self.assertEqual(edges[2]["from"], "coordinator")

    def test_scope_nesting_and_indices(self):
        m = Target(cycle_rate=100.0)
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
        m = Target(cycle_rate=100.0)
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
        m = Target(cycle_rate=100.0)
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
                target=Component("plant.gyro", element=1),
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
        link = mc.StateHandle("link")
        uplink = Uplink(link, msgs=["Cmd"])
        self.assertEqual(uplink._param_source()["Value"], {"msgs": ["Cmd"]})
        self.assertEqual(uplink.attach, "link")
        self.assertEqual(Downlink(link).attach, "link")
        self.assertEqual(TcpServer(addr="1.2.3.4:5")._param_source()["Value"], {"addr": "1.2.3.4:5"})

    # -- error cases --------------------------------------------------------

    def test_duplicate_instance_name(self):
        m = Target(cycle_rate=100.0)
        m.add("a", static_system("A"))
        with self.assertRaisesRegex(ValueError, "duplicate instance name 'a'"):
            m.add("a", static_system("B"))

    def test_unknown_initial_occupant(self):
        m = Target(cycle_rate=100.0)
        seqs = m.artifact("seqs", crate="c", lib="l")
        with self.assertRaisesRegex(ValueError, "initial occupant 'missing'"):
            m.slot("mode", inputs=[], outputs=[], allow=[seqs.safe_mode()], initial="missing")

    def test_non_json_param_names_the_key(self):
        m = Target(cycle_rate=100.0)
        with self.assertRaisesRegex(TypeError, "'bad'"):
            m.add("a", static_system("A", bad=object()))

    def test_dataclass_rejects_unknown_kwargs(self):
        with self.assertRaises(TypeError):
            Component("x", bogus=1)
        with self.assertRaises(TypeError):
            band(above=1.0, sideways=2.0)
        with self.assertRaises(TypeError):
            Alarm(id="x", name="n", target=Component("c"), nope=1)

    def test_exactly_one_target_rule(self):
        mc._targets.clear()
        with self.assertRaisesRegex(RuntimeError, "found 0"):
            mc.emit()
        Target(cycle_rate=1.0)
        Target(cycle_rate=2.0)
        with self.assertRaisesRegex(RuntimeError, "found 2"):
            mc.emit()


class PresetTest(unittest.TestCase):
    def setUp(self):
        mc._targets.clear()

    def test_component_id_masks_and_hashes(self):
        # Pinned against the Rust `ComponentId::new` (see the parity test in
        # src/preset/tests.rs) — the two must agree byte-for-byte.
        self.assertEqual(
            mc.component_id("sat1.plant.gyro.rates"), 3325449500645109259
        )
        self.assertEqual(mc.component_id("x") >> 63, 0)

    def test_presets_qualify_and_embed(self):
        import json

        m = Target(cycle_rate=100.0, namespace="sat1")
        m.add(
            "presets",
            mc.Presets(
                [
                    mc.Preset(
                        name="ops",
                        time_range="LAST 30m",
                        layout=mc.VSplit(
                            mc.TimeSeriesPlot([mc.Trace("plant.gyro.rates", element=1)]),
                            mc.Pane([mc.Logs(), mc.AlarmList()], active=1),
                            flexes=[2.0, 1.0],
                        ),
                    )
                ]
            ),
        )
        sys_spec = next(s for s in m.to_ir()["systems"] if s["name"] == "presets")
        preset = sys_spec["params"]["Value"]["preset"][0]
        self.assertEqual(preset["name"], "ops")
        layout = preset["layout"]
        self.assertEqual(layout["global_time_range"], "LAST 30m")
        self.assertNotIn("version", layout, "stamping is the Rust side's job")

        split = layout["root"]["Split"]
        self.assertEqual(split["axis"], "Vertical")
        self.assertEqual(split["flexes"], [2.0, 1.0])

        # Bare pane content wraps into a single-tab pane; the trace id is the
        # namespace-qualified hash and an unset color cycles the palette.
        plot_pane = split["children"][0]["Pane"]
        plot = json.loads(plot_pane["items"][0]["state"])
        trace = plot["traces"][0]
        self.assertEqual(
            trace["component_id"], mc.component_id("sat1.plant.gyro.rates")
        )
        self.assertEqual(trace["element_index"], 1)
        self.assertEqual(trace["label"], "plant.gyro.rates")
        self.assertTrue(trace["color"].startswith("#"))

        tab_pane = split["children"][1]["Pane"]
        self.assertEqual(tab_pane["active_index"], 1)
        self.assertEqual(
            [i["kind"] for i in tab_pane["items"]], ["logs", "alarm"]
        )

    def test_presets_flex_arity_is_checked(self):
        Target(cycle_rate=100.0)
        with self.assertRaisesRegex(ValueError, "2 split children but 1 flexes"):
            mc.Presets(
                [
                    mc.Preset(
                        name="bad",
                        layout=mc.HSplit(mc.Logs(), mc.DataTable(), flexes=[1.0]),
                    )
                ]
            )


if __name__ == "__main__":
    unittest.main()
