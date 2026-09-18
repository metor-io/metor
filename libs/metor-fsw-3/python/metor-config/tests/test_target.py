"""Target recording, to_config(), and emission."""

from __future__ import annotations

import json
import os
import tempfile
import unittest

from adcs_stub import Ctrl, Mode, MotorCmd, Nav, Ping, Plant

import metor_config._target as target_mod
from metor_config import ConfigError, PortRef, Publish, Subscribe, Target, emit

GOLDEN = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "..",
    "..",
    "..",
    "tests",
    "golden",
    "target.json",
)


def adcs_target() -> Target:
    """The design doc's example target."""
    fsw = Target(cycle_rate=100.0, namespace="cube_sat")
    fsw.add("cmds", Subscribe([Ping], listen="127.0.0.1:0"))
    motor_cmd = fsw.loop(MotorCmd)
    plant = fsw.add("plant", Plant(altitude=400e3, motor_cmd=motor_cmd))
    nav = fsw.add("nav", Nav(imu=plant.imu, gain=0.2))
    mode = fsw.add("mode", Mode())
    ctrl = fsw.add("ctrl", Ctrl(est=nav.est, mode=mode.cmd))
    motor_cmd.connect(ctrl.motor_cmd)
    fsw.add("pub", Publish([plant, nav.est], listen="127.0.0.1:0"), thread="io")
    return fsw


class TargetTest(unittest.TestCase):
    def setUp(self) -> None:
        target_mod._targets.clear()
        target_mod._emitted = False
        os.environ.pop("METOR_FSW_ABI_VERSION", None)
        os.environ.pop("METOR_CONFIG_OUT", None)

    def tearDown(self) -> None:
        # Leave no target behind, or the atexit hook emits one to stdout.
        target_mod._targets.clear()

    def test_the_atexit_hook_emits_once(self) -> None:
        adcs_target()
        with tempfile.TemporaryDirectory() as dir:
            path = os.path.join(dir, "out.json")
            os.environ["METOR_CONFIG_OUT"] = path
            target_mod._emit_at_exit()
            self.assertTrue(os.path.exists(path))
            os.remove(path)
            target_mod._emit_at_exit()
            self.assertFalse(os.path.exists(path))

    def test_example_emits_the_golden(self) -> None:
        adcs_target()
        with tempfile.TemporaryDirectory() as dir:
            path = os.path.join(dir, "target.json")
            emit(path)
            with open(path, "rb") as file:
                written = file.read()
        with open(GOLDEN, "rb") as file:
            self.assertEqual(written, file.read())

    def test_emit_writes_to_the_env_path(self) -> None:
        adcs_target()
        with tempfile.TemporaryDirectory() as dir:
            path = os.path.join(dir, "out.json")
            os.environ["METOR_CONFIG_OUT"] = path
            emit()
            with open(path, encoding="utf-8") as file:
                self.assertEqual(json.load(file)["config_version"], 1)

    def test_sim_dt_selects_the_simulated_clock(self) -> None:
        fsw = Target(cycle_rate=120.0, sim_dt=1 / 120)
        self.assertEqual(
            fsw.to_config()["coordinator"]["clock"],
            {"Simulated": {"dt": {"secs": 0, "nanos": 8333333}}},
        )

    def test_unconnected_loop_raises(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("plant", Plant(motor_cmd=fsw.loop(MotorCmd)))
        with self.assertRaisesRegex(ConfigError, "plant.motor_cmd"):
            fsw.to_config()

    def test_loop_connected_twice_raises(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        ctrl = fsw.add("ctrl", Ctrl())
        motor_cmd = fsw.loop(MotorCmd)
        motor_cmd.connect(ctrl.motor_cmd)
        with self.assertRaisesRegex(ConfigError, "already connected"):
            motor_cmd.connect(plant.imu)  # pyright: ignore[reportArgumentType]

    def test_sequence_input_keeps_edge_order(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        mode = fsw.add("mode", Mode())
        sources = [mode.cmd, plant.imu]  # mixed records: a checker rejects it, the recorder does not
        fsw.add("nav", Nav(imu=sources))
        systems = fsw.to_config()["coordinator"]["systems"]
        self.assertEqual(
            systems[2]["inputs"],
            [
                {
                    "port": "imu",
                    "from": [
                        {"system": "mode", "port": "cmd"},
                        {"system": "plant", "port": "imu"},
                    ],
                }
            ],
        )

    def test_add_twice_under_one_name_raises(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("plant", Plant())
        with self.assertRaisesRegex(ConfigError, "already added"):
            fsw.add("plant", Plant())

    def test_abi_mismatch_names_both_versions(self) -> None:
        os.environ["METOR_FSW_ABI_VERSION"] = "7"
        fsw = Target(cycle_rate=100.0)
        with self.assertRaises(ConfigError) as caught:
            fsw.add("plant", Plant())
        message = str(caught.exception)
        self.assertIn("ABI 1", message)
        self.assertIn("host ABI is 7", message)

    def test_matching_abi_is_accepted(self) -> None:
        os.environ["METOR_FSW_ABI_VERSION"] = "1"
        fsw = Target(cycle_rate=100.0)
        fsw.add("plant", Plant())

    def test_undeclared_port_raises_attribute_error(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        with self.assertRaisesRegex(AttributeError, "no output port `est`"):
            plant.est  # type: ignore[attr-defined]

    def test_log_and_status_are_always_outputs(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        self.assertEqual(plant.log.ref, PortRef("plant", "log"))
        self.assertEqual(plant.status.ref, PortRef("plant", "status"))

    def test_a_system_without_params_emits_null(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("mode", Mode())
        entry = fsw.to_config()["coordinator"]["systems"][0]
        self.assertIsNone(entry["params"])
        self.assertEqual(entry["inputs"], [])

    def test_a_non_source_input_raises(self) -> None:
        with self.assertRaisesRegex(ConfigError, "input `imu`"):
            Nav(imu="plant.imu")  # type: ignore[arg-type]

    def test_emission_needs_exactly_one_target(self) -> None:
        adcs_target()
        adcs_target()
        with self.assertRaisesRegex(ConfigError, "found 2"):
            emit()


class LinkTest(unittest.TestCase):
    def setUp(self) -> None:
        target_mod._targets.clear()

    def tearDown(self) -> None:
        target_mod._targets.clear()

    def test_a_published_handle_lists_log_and_status(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        fsw.add("pub", Publish([plant], listen="0.0.0.0:2240"))
        entry = fsw.to_config()["coordinator"]["systems"][1]
        self.assertEqual(
            [input["port"] for input in entry["inputs"]],
            ["plant.imu", "plant.log", "plant.status"],
        )
        self.assertEqual(
            entry["inputs"][0]["from"], [{"system": "plant", "port": "imu"}]
        )
        self.assertEqual(
            entry["params"]["transport"],
            {"listen": {"addr": "0.0.0.0:2240", "max_connections": 8}},
        )

    def test_a_published_port_stands_alone(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        fsw.add("pub", Publish([plant.imu], connect="127.0.0.1:2240"))
        entry = fsw.to_config()["coordinator"]["systems"][1]
        self.assertEqual([input["port"] for input in entry["inputs"]], ["plant.imu"])
        self.assertEqual(entry["params"]["transport"], {"connect": {"addr": "127.0.0.1:2240"}})

    def test_publishing_everything_lists_only_earlier_systems(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("plant", Plant())
        fsw.add("pub", Publish(listen="0.0.0.0:2240", all=True))
        fsw.add("mode", Mode())
        entry = fsw.to_config()["coordinator"]["systems"][1]
        self.assertEqual(
            [input["port"] for input in entry["inputs"]],
            ["plant.imu", "plant.log", "plant.status"],
        )
        # Emission repeats without growing the edge list.
        self.assertEqual(fsw.to_config()["coordinator"]["systems"][1], entry)

    def test_both_or_neither_transport_raises(self) -> None:
        with self.assertRaisesRegex(ConfigError, "exactly one"):
            Publish([], listen="0.0.0.0:1", connect="0.0.0.0:2")
        with self.assertRaisesRegex(ConfigError, "exactly one"):
            Subscribe([Ping])

    def test_publishing_something_that_is_no_port_raises(self) -> None:
        with self.assertRaisesRegex(ConfigError, "handles and ports"):
            Publish(["plant.imu"], listen="0.0.0.0:1")  # type: ignore[list-item]

    def test_subscribing_to_a_non_record_raises(self) -> None:
        with self.assertRaisesRegex(ConfigError, "record classes"):
            Subscribe([Plant], listen="0.0.0.0:1")  # type: ignore[list-item]
        with self.assertRaisesRegex(ConfigError, "record classes"):
            Subscribe(["ping"], listen="0.0.0.0:1")  # type: ignore[list-item]

    def test_a_subscribed_record_is_a_port_the_handle_resolves(self) -> None:
        fsw = Target(cycle_rate=100.0, namespace="cube_sat")
        cmds = fsw.add("cmds", Subscribe([Ping], listen="0.0.0.0:2241"))
        self.assertEqual(cmds.ping.ref, PortRef("cmds", "ping"))  # type: ignore[attr-defined]
        self.assertEqual(cmds.link_status.ref, PortRef("cmds", "link_status"))  # type: ignore[attr-defined]
        with self.assertRaisesRegex(AttributeError, "no output port `arm`"):
            cmds.arm  # type: ignore[attr-defined]
        entry = fsw.to_config()["coordinator"]["systems"][0]
        self.assertEqual(entry["outputs"], [{"port": "ping", "record": "ping"}])
        self.assertEqual(entry["params"]["namespace"], "cube_sat")
        self.assertEqual(entry["params"]["link"], "cmds")

    def test_the_builtin_pack_is_not_a_config_pack(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("cmds", Subscribe([Ping], listen="0.0.0.0:2241"))
        self.assertEqual(fsw.to_config()["packs"], [])

    def test_a_thread_lands_on_the_system(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("cmds", Subscribe([Ping], listen="0.0.0.0:2241"), thread="io")
        fsw.add("plant", Plant())
        systems = fsw.to_config()["coordinator"]["systems"]
        self.assertEqual(systems[0]["thread"], "io")
        self.assertNotIn("thread", systems[1])


if __name__ == "__main__":
    unittest.main()
