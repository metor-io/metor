"""Target recording, to_config(), and emission."""

from __future__ import annotations

import json
import os
import tempfile
import unittest

from adcs_stub import PACK, Ctrl, Fan, Mode, MotorCmd, Nav, Ping, Plant, Tap

import metor_config._target as target_mod
from metor_config import ConfigError, PortRef, Publish, Subscribe, Target, emit
from metor_config._config import ABI_VERSION

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
        os.environ.pop("METOR_FSW_ABI_VERSION", None)

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

    def test_a_generated_type_takes_a_port_list(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        nav = fsw.add("nav", Nav(imu=plant.imu))
        fsw.add("tap", Tap(items=[plant, nav.est], gain=2.0))
        entry = fsw.to_config()["coordinator"]["systems"][2]
        self.assertEqual(
            [input["port"] for input in entry["inputs"]],
            ["plant.imu", "plant.log", "plant.status", "nav.est"],
        )
        self.assertEqual(entry["inputs"][3]["from"], [{"system": "nav", "port": "est"}])
        self.assertEqual(entry["params"], {"gain": 2.0})

    def test_a_generated_type_with_no_items_has_no_edges(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("tap", Tap())
        entry = fsw.to_config()["coordinator"]["systems"][0]
        self.assertEqual(entry["inputs"], [])
        self.assertNotIn("outputs", entry)

    def test_a_generated_port_list_takes_ports_and_handles_only(self) -> None:
        with self.assertRaisesRegex(ConfigError, "handles and ports"):
            Tap(items=["plant.imu"])  # type: ignore[list-item]

    def test_a_generated_type_takes_a_record_list(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fan = fsw.add("fan", Fan(records=[Ping, MotorCmd]))
        entry = fsw.to_config()["coordinator"]["systems"][0]
        self.assertEqual(
            entry["outputs"],
            [
                {"port": "ping", "record": "ping"},
                {"port": "motor_cmd", "record": "motor_cmd"},
            ],
        )
        self.assertEqual(fan.ping.ref, PortRef("fan", "ping"))  # type: ignore[attr-defined]
        with self.assertRaisesRegex(ConfigError, "twice"):
            Fan(records=[Ping, Ping])
        with self.assertRaisesRegex(ConfigError, "record classes"):
            Fan(records=[Plant])  # type: ignore[list-item]

    def test_both_or_neither_transport_raises(self) -> None:
        with self.assertRaisesRegex(ConfigError, "exactly one"):
            Publish([], listen="0.0.0.0:1", connect="0.0.0.0:2")
        with self.assertRaisesRegex(ConfigError, "exactly one"):
            Subscribe([Ping])

    def test_max_connections_on_a_dialing_link_raises(self) -> None:
        with self.assertRaisesRegex(ConfigError, "max_connections"):
            Publish([], connect="127.0.0.1:2240", max_connections=2)
        with self.assertRaisesRegex(ConfigError, "max_connections"):
            Subscribe([Ping], connect="127.0.0.1:2240", max_connections=2)

    def test_subscribing_to_one_record_twice_raises(self) -> None:
        with self.assertRaisesRegex(ConfigError, "twice"):
            Subscribe([Ping, Ping], listen="0.0.0.0:1")

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
        fsw.add("cmds", Subscribe([], listen="0.0.0.0:2241"))
        self.assertEqual(fsw.to_config()["packs"], [])

    def test_subscribe_loads_its_record_pack_without_a_pack_system(self) -> None:
        fsw = Target(cycle_rate=100.0)
        cmds = fsw.add("cmds", Subscribe([Ping, MotorCmd], listen="127.0.0.1:0"))
        fsw.add("pub", Publish([cmds], listen="127.0.0.1:0"))
        self.assertEqual(fsw.to_config()["packs"], [PACK.to_json()])
        fsw.add("plant", Plant())
        self.assertEqual(fsw.to_config()["packs"], [PACK.to_json()])

    def test_a_record_pack_abi_mismatch_does_not_register_the_link(self) -> None:
        os.environ["METOR_FSW_ABI_VERSION"] = str(ABI_VERSION)
        fsw = Target(cycle_rate=100.0)
        with self.assertRaisesRegex(ConfigError, "pack `adcs`.*ABI 1"):
            fsw.add("cmds", Subscribe([Ping], listen="127.0.0.1:0"))
        self.assertEqual(fsw.to_config()["packs"], [])
        self.assertEqual(fsw.to_config()["coordinator"]["systems"], [])
        fsw.add("cmds", Subscribe([], listen="127.0.0.1:0"))

    def test_a_reused_publish_resolves_each_registration_independently(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("plant", Plant())
        publish = Publish(all=True, listen="127.0.0.1:0")
        fsw.add("pub1", publish)
        fsw.add("pub2", publish)
        config = fsw.to_config()
        first, second = config["coordinator"]["systems"][1:]
        self.assertEqual(first["params"]["link"], "pub1")
        self.assertEqual(second["params"]["link"], "pub2")
        self.assertEqual(
            [item["port"] for item in first["inputs"]],
            ["plant.imu", "plant.log", "plant.status"],
        )
        self.assertEqual(
            [item["port"] for item in second["inputs"]],
            ["plant.imu", "plant.log", "plant.status", "pub1.link_status", "pub1.log", "pub1.status"],
        )
        self.assertEqual(fsw.to_config(), config)
        self.assertEqual(publish._params["link"], "")
        self.assertEqual(publish._inputs, {})

    def test_reuse_across_targets_and_emission_preserves_previous_configs(self) -> None:
        subscribe = Subscribe([Ping], listen="127.0.0.1:0")
        first = Target(cycle_rate=100.0, namespace="first")
        first.add("a", subscribe)
        saved = first.to_config()
        second = Target(cycle_rate=100.0, namespace="second")
        second.add("b", subscribe)
        emitted = second.to_config()
        first_entry = saved["coordinator"]["systems"][0]
        self.assertEqual(first_entry["params"]["namespace"], "first")
        self.assertEqual(first_entry["params"]["link"], "a")
        second_entry = emitted["coordinator"]["systems"][0]
        self.assertEqual(second_entry["params"]["namespace"], "second")
        second_entry["params"]["transport"]["listen"]["addr"] = "changed"
        second_entry["outputs"][0]["port"] = "changed"
        self.assertEqual(first.to_config(), saved)
        self.assertEqual(second.to_config()["coordinator"]["systems"][0]["outputs"][0]["port"], "ping")

    def test_a_thread_lands_on_the_system(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("cmds", Subscribe([Ping], listen="0.0.0.0:2241"), thread="io")
        fsw.add("plant", Plant())
        systems = fsw.to_config()["coordinator"]["systems"]
        self.assertEqual(systems[0]["thread"], "io")
        self.assertNotIn("thread", systems[1])


if __name__ == "__main__":
    unittest.main()
