"""The generated-pack contract, from the Python side.

The checked-in ``tests/data/demo.py`` is the exact module ``metor-fsw
stubgen`` produces for a fixture manifest (a Rust golden test pins the text).
Here the recorder side is exercised: importing the module, constructing its
typed entries, and adding them to a :class:`Mission` records the same IR the
untyped Phase 1 path would, and the artifact the module declares is
auto-registered — no explicit ``m.artifact(...)`` needed.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "data"))

import metor_config as mc
from metor_config import Artifact, InPort, Mission, OutPort, System

import demo


class TypedCoreTest(unittest.TestCase):
    def test_artifact_dataclass_shape(self):
        self.assertEqual(demo.ARTIFACT.id, "demo")
        self.assertEqual(demo.ARTIFACT.crate, "demo-systems")
        self.assertEqual(demo.ARTIFACT.lib, "demo_systems")
        self.assertTrue(demo.ARTIFACT.manifest_hash.startswith("sha256:"))

    def test_entry_is_a_system_spec(self):
        w = demo.Widget()
        self.assertIsInstance(w, System)
        self.assertEqual(w.ty, "Widget")
        self.assertEqual(w.artifact, "demo")
        self.assertIs(w.artifact_decl, demo.ARTIFACT)

    def test_defaults_and_overrides(self):
        # Every field carries a default from the manifest blob.
        self.assertEqual(demo.Widget().params, {
            "count": 0,
            "gain": 0.0,
            "label": "",
            "armed": False,
            "limit": None,
            "offsets": [0.0, 0.0, 0.0],
        })
        # Overrides flow through; a tuple param JSON-ifies to a list.
        w = demo.Widget(count=3, offsets=(1.0, 2.0, 3.0))
        self.assertEqual(w.params["count"], 3)
        self.assertEqual(w.params["offsets"], [1.0, 2.0, 3.0])

    def test_ports_are_generic_annotation_carriers(self):
        # Erased generics: never instantiated, but subscriptable for annotations.
        self.assertIsNotNone(OutPort[demo.Sensors])
        self.assertIsNotNone(InPort[demo.Cmd])


class RecordTest(unittest.TestCase):
    def setUp(self):
        mc._missions.clear()

    def test_add_auto_registers_artifact_and_records_system(self):
        m = Mission(cycle_rate=100.0)
        w = m.add("w", demo.Widget(count=5))
        ir = m.to_ir()

        # The artifact is registered implicitly, with its manifest hash.
        self.assertEqual(len(ir["artifacts"]), 1)
        art = ir["artifacts"][0]
        self.assertEqual(art["id"], "demo")
        self.assertEqual(art["manifest_hash"], demo.ARTIFACT.manifest_hash)

        # The system records the entry, artifact id, and value-tree params.
        sys_node = ir["systems"][0]
        self.assertEqual(sys_node["ty"], "Widget")
        self.assertEqual(sys_node["artifact"], "demo")
        self.assertEqual(sys_node["params"], {"Value": {
            "count": 5, "gain": 0.0, "label": "", "armed": False,
            "limit": None, "offsets": [0.0, 0.0, 0.0],
        }})

        # A port reference off the returned handle is a plain (instance, port).
        ref = w.sensors
        self.assertEqual((ref.instance, ref.port), ("w", "sensors"))

    def test_occupant_callable_records_in_a_slot(self):
        m = Mission(cycle_rate=100.0)
        mode = m.slot(
            "mode",
            inputs=["gps"],
            outputs=["mode_cmd"],
            allow=[demo.startup()],
            initial="startup",
        )
        ir = m.to_ir()
        # The occupant's artifact is auto-registered from the allow set.
        self.assertEqual([a["id"] for a in ir["artifacts"]], ["demo"])
        self.assertEqual(ir["slots"][0]["allow"][0]["occupant"], "startup")
        self.assertEqual(ir["slots"][0]["initial"], {"occupant": "startup", "state": "Running"})
        self.assertEqual(mode.name, "mode")

    def test_duplicate_artifact_ids_dedupe(self):
        m = Mission(cycle_rate=100.0)
        m.add("a", demo.Widget())
        m.add("b", demo.Widget())
        self.assertEqual(len(m.to_ir()["artifacts"]), 1)

    def test_conflicting_artifact_definition_is_an_error(self):
        m = Mission(cycle_rate=100.0)
        m.add("a", demo.Widget())
        rogue = System("Widget", Artifact(id="demo", crate="other", lib="other"))
        with self.assertRaises(ValueError):
            m.add("b", rogue)


if __name__ == "__main__":
    unittest.main()
