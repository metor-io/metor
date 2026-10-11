"""Target recording, to_config(), and emission."""

from __future__ import annotations

import json
import os
import tempfile
import unittest

from adcs_stub import Ctrl, Mode, MotorCmd, Nav, Plant

import metor_config._target as target_mod
from metor_config import ConfigError, PortRef, Target, emit

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
    motor_cmd = fsw.loop(MotorCmd)
    plant = fsw.add("plant", Plant(altitude=400e3, motor_cmd=motor_cmd))
    nav = fsw.add("nav", Nav(imu=plant.imu, gain=0.2))
    mode = fsw.add("mode", Mode())
    ctrl = fsw.add("ctrl", Ctrl(est=nav.est, mode=mode.cmd))
    motor_cmd.connect(ctrl.motor_cmd)
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


if __name__ == "__main__":
    unittest.main()
