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


class DashboardTest(unittest.TestCase):
    def setUp(self):
        mc._targets.clear()

    def _dashboard_state(self, dashboard, namespace="sat1"):
        import json

        # One Target per recording; a test that builds two dashboards would
        # otherwise trip the exactly-one rule.
        mc._targets.clear()
        m = Target(cycle_rate=100.0, namespace=namespace)
        m.add("presets", mc.Presets([mc.Preset(name="d", layout=dashboard)]))
        sys_spec = next(s for s in m.to_ir()["systems"] if s["name"] == "presets")
        pane = sys_spec["params"]["Value"]["preset"][0]["layout"]["root"]["Pane"]
        self.assertEqual(pane["items"][0]["kind"], "dashboard")
        return json.loads(pane["items"][0]["state"])

    def test_widgets_carry_kinds_rects_and_qualified_components(self):
        import json

        state = self._dashboard_state(
            mc.Dashboard(
                title="ADCS",
                widgets=[
                    mc.Place(
                        mc.Meter("plant.wheels.h", element=1, min=-0.04, max=0.04), 0, 0
                    ),
                    mc.Place(
                        mc.Gauge("plant.sensors.gyro_b", style="needle"), 100, 0, 120, 120
                    ),
                ],
            )
        )
        self.assertEqual(state["title"], "ADCS")
        self.assertEqual([w["kind"] for w in state["widgets"]], ["meter", "gauge"])
        # Ids are assigned in placement order and the counter clears them.
        self.assertEqual([w["id"] for w in state["widgets"]], [1, 2])
        self.assertEqual(state["next_id"], 3)

        meter = json.loads(state["widgets"][0]["config"])
        self.assertEqual(meter["component"], "sat1.plant.wheels.h")
        self.assertEqual(meter["element"], 1)
        self.assertEqual(meter["orientation"], "Vertical")

        # An omitted size falls back to the kind's default; a given one wins.
        self.assertEqual(state["widgets"][0]["rect"]["w"], 90.0)
        self.assertEqual(
            state["widgets"][1]["rect"], {"x": 100.0, "y": 0.0, "w": 120.0, "h": 120.0}
        )

        gauge = json.loads(state["widgets"][1]["config"])
        self.assertEqual(gauge["style"], "Needle")
        self.assertEqual(gauge["sweep_degrees"], 240.0)

    def test_a_bare_place_endpoint_picks_the_side_facing_its_neighbour(self):
        left = mc.Place(mc.Meter("a"), 0, 0, 100, 100)
        right = mc.Place(mc.Meter("b"), 400, 0, 100, 100)
        state = self._dashboard_state(
            mc.Dashboard(widgets=[left, right], connectors=[mc.Connector([left, right])])
        )
        anchors = state["connectors"][0]["points"]
        # Left box exits right; the right box is entered from its left.
        self.assertEqual(anchors[0]["Widget"]["side"], "Right")
        self.assertEqual(anchors[1]["Widget"]["side"], "Left")
        self.assertEqual(anchors[0]["Widget"]["id"], 1)
        self.assertEqual(anchors[1]["Widget"]["id"], 2)

        # Stacked vertically instead, the same call picks top/bottom.
        top = mc.Place(mc.Meter("a"), 0, 0, 100, 100)
        bottom = mc.Place(mc.Meter("b"), 0, 400, 100, 100)
        state = self._dashboard_state(
            mc.Dashboard(widgets=[top, bottom], connectors=[mc.Connector([top, bottom])])
        )
        anchors = state["connectors"][0]["points"]
        self.assertEqual(anchors[0]["Widget"]["side"], "Bottom")
        self.assertEqual(anchors[1]["Widget"]["side"], "Top")

    def test_explicit_edges_and_free_points_survive(self):
        box = mc.Place(mc.Meter("a"), 0, 0, 100, 100)
        state = self._dashboard_state(
            mc.Dashboard(
                widgets=[box],
                connectors=[
                    mc.Connector(
                        [mc.Edge(box, "top", 0.25), mc.At(300, 40)],
                        shape="curved",
                        arrow="both",
                        dashed=True,
                        on_top=True,
                        label="leader",
                        bind=mc.Bind("plant.wheels.arm", threshold=0.5),
                    )
                ],
            )
        )
        connector = state["connectors"][0]
        self.assertEqual(
            connector["points"][0]["Widget"], {"id": 1, "side": "Top", "t": 0.25}
        )
        self.assertEqual(connector["points"][1]["Free"], {"x": 300.0, "y": 40.0})
        style = connector["style"]
        self.assertEqual(style["shape"], "Curved")
        self.assertEqual(style["arrow"], "Both")
        self.assertTrue(style["dashed"] and style["on_top"])
        self.assertEqual(style["label"], "leader")
        self.assertEqual(style["bind"]["component"], "sat1.plant.wheels.arm")
        self.assertEqual(style["bind"]["threshold"], 0.5)
        self.assertEqual(state["next_connector_id"], 2)

    def test_a_plot_placed_on_a_dashboard_uses_the_widget_kind(self):
        plot = mc.Place(mc.TimeSeriesPlot([mc.Trace("plant.gyro.rates")]), 0, 0)
        state = self._dashboard_state(mc.Dashboard(widgets=[plot]))
        # The dashboard names this kind `plot`; a pane names it
        # `time_series_plot`. Same view, two surfaces.
        self.assertEqual(state["widgets"][0]["kind"], "plot")

    def test_state_chip_tables_and_sequence_channels(self):
        import json

        chip = mc.Place(
            mc.StateChip(
                "mode.mode_cmd",
                states=[mc.State(0, "IDLE"), mc.State(3, "SAFE", "#f38ba8ff")],
                unknown="UNKNOWN",
            ),
            0,
            0,
        )
        seq = mc.Place(mc.SequenceControl("mode"), 0, 100)
        state = self._dashboard_state(mc.Dashboard(widgets=[chip, seq]))

        table = json.loads(state["widgets"][0]["config"])
        self.assertEqual(table["component"], "sat1.mode.mode_cmd")
        self.assertEqual([s["label"] for s in table["states"]], ["IDLE", "SAFE"])
        self.assertEqual(table["states"][1]["color"], "#f38ba8ff")
        self.assertEqual(table["unknown_label"], "UNKNOWN")

        # A channel is a slot instance name, not a component, so it is never
        # namespace-qualified.
        control = json.loads(state["widgets"][1]["config"])
        self.assertEqual(control["channel"], "mode")

    def test_attitude_markers_are_qualified(self):
        import json

        att = mc.Place(
            mc.Attitude(
                "nav.attitude_estimate.q_hat_b_eci",
                vectors=[mc.VectorMarker("plant.sensors.mag_b", "mag")],
            ),
            0,
            0,
        )
        state = self._dashboard_state(mc.Dashboard(widgets=[att]))
        cfg = json.loads(state["widgets"][0]["config"])
        self.assertEqual(cfg["component"], "sat1.nav.attitude_estimate.q_hat_b_eci")
        self.assertEqual(cfg["vectors"][0]["component"], "sat1.plant.sensors.mag_b")
        self.assertEqual(cfg["vectors"][0]["label"], "mag")

    def test_authoring_mistakes_are_caught_at_record_time(self):
        Target(cycle_rate=100.0)
        stray = mc.Place(mc.Meter("a"), 0, 0)
        placed = mc.Place(mc.Meter("b"), 0, 0)

        with self.assertRaisesRegex(ValueError, "not on this dashboard"):
            mc.Dashboard(
                widgets=[placed], connectors=[mc.Connector([placed, stray])]
            )._state(None)

        with self.assertRaisesRegex(ValueError, "at least two points"):
            mc.Dashboard(widgets=[placed], connectors=[mc.Connector([placed])])._state(
                None
            )

        with self.assertRaisesRegex(TypeError, "must be Place"):
            mc.Dashboard(widgets=[mc.Meter("a")])._state(None)

        with self.assertRaisesRegex(ValueError, "expected one of"):
            mc.Dashboard(
                widgets=[placed],
                connectors=[mc.Connector([placed, mc.At(1, 1)], shape="squiggly")],
            )._state(None)

    def test_two_identical_placements_stay_distinct(self):
        # Place compares by identity, so a duplicated widget spec does not
        # collapse two boxes into one anchor target.
        a = mc.Place(mc.Meter("x"), 0, 0, 50, 50)
        b = mc.Place(mc.Meter("x"), 200, 0, 50, 50)
        state = self._dashboard_state(
            mc.Dashboard(widgets=[a, b], connectors=[mc.Connector([a, b])])
        )
        self.assertEqual(len(state["widgets"]), 2)
        anchors = state["connectors"][0]["points"]
        self.assertEqual(anchors[0]["Widget"]["id"], 1)
        self.assertEqual(anchors[1]["Widget"]["id"], 2)


if __name__ == "__main__":
    unittest.main()
