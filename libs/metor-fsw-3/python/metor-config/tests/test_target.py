"""Tests for building targets, `to_config()`, and emitting the config."""

from __future__ import annotations

import contextlib
import inspect
import json
import os
import tempfile
import unittest
from dataclasses import replace

from adcs_stub import PACK, Ctrl, Fan, Hold, Mode, MotorCmd, Nav, Ping, Plant, Safe, Tap

import metor_config._deployment as deploy_mod
import metor_config._target as target_mod
from metor_config import (
    ChannelRegistry,
    ConfigError,
    Connect,
    Db,
    DlSlot,
    Listen,
    PortRef,
    Publish,
    SequenceCommand,
    Subscribe,
    Target,
    TcpDial,
    emit,
)

CRATE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..")
"""The crate root. The goldens' locations are relative to it."""

GOLDEN = os.path.join(CRATE, "tests", "golden", "target.json")


def adcs_target() -> Target:
    """The design doc's example target."""
    fsw = Target(cycle_rate=100.0, namespace="cube_sat")
    fsw.add("cmds", Subscribe([Ping], transport=Listen("127.0.0.1:0")))
    motor_cmd = fsw.loop(MotorCmd)
    plant = fsw.add("plant", Plant(altitude=400e3, motor_cmd=motor_cmd))
    nav = fsw.add("nav", Nav(imu=plant.imu, gain=0.2))
    mode = fsw.add("mode", Mode())
    ctrl = fsw.add("ctrl", Ctrl(est=nav.est, mode=mode.cmd))
    motor_cmd.connect(ctrl.motor_cmd)
    fsw.add("pub", Publish([plant, nav.est], transport=Listen("127.0.0.1:0")))
    return fsw


def reset() -> None:
    """Clears every target and deployment, so the atexit hook has nothing to
    emit to stdout."""
    target_mod._targets.clear()
    deploy_mod._deployments.clear()
    deploy_mod._emitted = False


class ConfigTest(unittest.TestCase):
    """Starts and ends each test with no target, deployment, or env override."""

    def setUp(self) -> None:
        reset()
        os.environ.pop("METOR_CONFIG_OUT", None)

    def tearDown(self) -> None:
        reset()


class TargetTest(ConfigTest):
    def test_atexit_emits_once(self) -> None:
        adcs_target()
        with tempfile.TemporaryDirectory() as dir:
            path = os.path.join(dir, "out.json")
            os.environ["METOR_CONFIG_OUT"] = path
            deploy_mod._emit_at_exit()
            self.assertTrue(os.path.exists(path))
            os.remove(path)
            deploy_mod._emit_at_exit()
            self.assertFalse(os.path.exists(path))

    def test_example_matches_golden(self) -> None:
        with contextlib.chdir(CRATE):
            adcs_target()
        with tempfile.TemporaryDirectory() as dir:
            path = os.path.join(dir, "target.json")
            emit(path)
            with open(path, "rb") as file:
                written = file.read()
        with open(GOLDEN, "rb") as file:
            self.assertEqual(written, file.read())

    def test_emit_uses_env_path(self) -> None:
        adcs_target()
        with tempfile.TemporaryDirectory() as dir:
            path = os.path.join(dir, "out.json")
            os.environ["METOR_CONFIG_OUT"] = path
            emit()
            with open(path, encoding="utf-8") as file:
                self.assertEqual(json.load(file)["config_version"], 2)

    def test_seed_is_emitted_when_set(self) -> None:
        self.assertEqual(Target(cycle_rate=1.0, seed=7).to_config()["coordinator"]["seed"], 7)
        self.assertNotIn("seed", Target(cycle_rate=1.0).to_config()["coordinator"])

    def test_epoch_is_emitted_when_set(self) -> None:
        clock = Target(cycle_rate=1.0, sim_dt=0.01, epoch=0).to_config()["coordinator"]["clock"]
        self.assertEqual(clock["Simulated"]["epoch"], 0)
        clock = Target(cycle_rate=1.0, sim_dt=0.01).to_config()["coordinator"]["clock"]
        self.assertNotIn("epoch", clock["Simulated"])
        with self.assertRaisesRegex(ConfigError, "epoch"):
            Target(cycle_rate=1.0, epoch=0)

    def test_sim_dt_selects_simulated_clock(self) -> None:
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

    def test_reject_duplicate_system_name(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("plant", Plant())
        with self.assertRaisesRegex(ConfigError, "already added"):
            fsw.add("plant", Plant())

    def test_undeclared_port_raises_attribute_error(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        with self.assertRaisesRegex(AttributeError, "no output port `est`"):
            plant.est  # type: ignore[attr-defined]

    def test_outputs_include_log_and_status(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        self.assertEqual(plant.log.ref, PortRef("plant", "log"))
        self.assertEqual(plant.status.ref, PortRef("plant", "status"))

    def test_missing_params_emit_null(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("mode", Mode())
        entry = fsw.to_config()["coordinator"]["systems"][0]
        self.assertIsNone(entry["params"])
        self.assertEqual(entry["inputs"], [])

    def test_reject_non_source_input(self) -> None:
        with self.assertRaisesRegex(ConfigError, "input `imu`"):
            Nav(imu="plant.imu")  # type: ignore[arg-type]

    def test_emit_requires_one_target(self) -> None:
        adcs_target()
        adcs_target()
        with self.assertRaisesRegex(ConfigError, "found 2"):
            emit()


class LocationTest(ConfigTest):
    def test_system_records_where_it_was_created(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = Plant()
        line = inspect.currentframe().f_lineno - 1  # type: ignore[union-attr]
        fsw.add("plant", plant)
        fsw.add("pub", Publish([], transport=Listen("127.0.0.1:0")))
        plant_entry, pub_entry = fsw.to_config()["coordinator"]["systems"][:2]
        here = os.path.relpath(__file__)
        self.assertEqual(plant_entry["location"], {"file": here, "line": line})
        self.assertEqual(pub_entry["location"], {"file": here, "line": line + 3})

    def test_location_is_relative_to_the_working_directory(self) -> None:
        with contextlib.chdir(os.path.dirname(os.path.abspath(__file__))):
            nav = Nav()
        self.assertEqual(nav._location["file"], os.path.basename(__file__))  # type: ignore[index]

    def test_system_without_a_location_omits_it(self) -> None:
        fsw = Target(cycle_rate=100.0)
        mode = Mode()
        mode._location = None
        fsw.add("mode", mode)
        self.assertNotIn("location", fsw.to_config()["coordinator"]["systems"][0])


class WasmTest(ConfigTest):
    def test_wasm_writes_kind_fuel_and_memory(self) -> None:
        fsw = adcs_target()
        fsw.wasm(replace(PACK, wasm=True), fuel=10_000_000, memory=16 << 20)
        wasm = {"kind": "wasm", "fuel": 10_000_000, "memory": 16 << 20}
        self.assertEqual(fsw.to_config()["packs"], [{**PACK.to_json(), **wasm}])

    def test_pack_without_module_raises(self) -> None:
        fsw = adcs_target()
        with self.assertRaisesRegex(ConfigError, "`adcs` has no module"):
            fsw.wasm(PACK)


class LinkTest(ConfigTest):
    def test_publish_handle_includes_log_and_status(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        fsw.add("pub", Publish([plant], transport=Listen("0.0.0.0:2240")))
        entry = fsw.to_config()["coordinator"]["systems"][1]
        self.assertEqual(
            [input["port"] for input in entry["inputs"]],
            ["plant.imu", "plant.log", "plant.status"],
        )
        self.assertEqual(
            entry["inputs"][0]["from"], [{"system": "plant", "port": "imu"}]
        )
        self.assertEqual(entry["grants"], {"ep": "pub_ep"})
        self.assertEqual(entry["params"]["max_connections"], 8)
        self.assertNotIn("dialing", entry["params"])
        resources = fsw.to_config()["coordinator"]["resources"]
        self.assertEqual(resources, {"pub_ep": {"real": {"tcp_listen": {"addr": "0.0.0.0:2240"}}}})

    def test_publish_single_port(self) -> None:
        fsw = Target(cycle_rate=100.0)
        plant = fsw.add("plant", Plant())
        fsw.add("pub", Publish([plant.imu], transport=Connect("127.0.0.1:2240")))
        entry = fsw.to_config()["coordinator"]["systems"][1]
        self.assertEqual([input["port"] for input in entry["inputs"]], ["plant.imu"])
        self.assertEqual(entry["params"]["dialing"], True)
        resources = fsw.to_config()["coordinator"]["resources"]
        self.assertEqual(resources, {"pub_ep": {"real": {"tcp_dial": {"addr": "127.0.0.1:2240"}}}})

    def test_publish_all_includes_prior_systems(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("plant", Plant())
        fsw.add("pub", Publish(transport=Listen("0.0.0.0:2240"), all=True))
        fsw.add("mode", Mode())
        entry = fsw.to_config()["coordinator"]["systems"][1]
        self.assertEqual(
            [input["port"] for input in entry["inputs"]],
            ["plant.imu", "plant.log", "plant.status"],
        )
        # Emission repeats without growing the edge list.
        self.assertEqual(fsw.to_config()["coordinator"]["systems"][1], entry)

    def test_generated_type_accepts_ports(self) -> None:
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

    def test_empty_generated_type_has_no_edges(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("tap", Tap())
        entry = fsw.to_config()["coordinator"]["systems"][0]
        self.assertEqual(entry["inputs"], [])
        self.assertNotIn("outputs", entry)

    def test_reject_invalid_generated_ports(self) -> None:
        with self.assertRaisesRegex(ConfigError, "handles and ports"):
            Tap(items=["plant.imu"])  # type: ignore[list-item]

    def test_generated_type_accepts_records(self) -> None:
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

    def test_require_a_transport(self) -> None:
        with self.assertRaises(TypeError):
            Subscribe([Ping])  # type: ignore[call-arg]
        with self.assertRaisesRegex(ConfigError, "`Listen`, `Connect`, or `Peer`"):
            Publish([], transport="0.0.0.0:1")  # type: ignore[arg-type]

    def test_listen_max_connections(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("pub", Publish([], transport=Listen("0.0.0.0:2240", max_connections=2)))
        entry = fsw.to_config()["coordinator"]["systems"][0]
        self.assertEqual(entry["params"]["max_connections"], 2)

    def test_listen_allow_reaches_the_resource(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("pub", Publish([], transport=Listen("0.0.0.0:2240", allow=["10.0.0.0/24"])))
        resource = fsw.to_config()["coordinator"]["resources"]["pub_ep"]
        self.assertEqual(resource["real"]["tcp_listen"]["allow"], ["10.0.0.0/24"])
        with self.assertRaisesRegex(ConfigError, "not a CIDR block"):
            fsw.add("bad", Publish([], transport=Listen("0.0.0.0:2241", allow=["10.0.0.0"])))

    def test_connect_resolves_a_host_name(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("pub", Publish([], transport=Connect("localhost:2240")))
        dial = fsw.to_config()["coordinator"]["resources"]["pub_ep"]["real"]["tcp_dial"]
        self.assertIn(dial["addr"], ("127.0.0.1:2240", "[::1]:2240"))
        with self.assertRaisesRegex(ConfigError, "does not resolve"):
            fsw.add("lost", Publish([], transport=Connect("nohost.invalid:1")))
        with self.assertRaisesRegex(ConfigError, "not a `host:port`"):
            fsw.add("bare", Publish([], transport=Connect("localhost")))
        with self.assertRaisesRegex(ConfigError, "without brackets"):
            fsw.add("v6", Publish([], transport=Connect("::1:2240")))
        fsw.add("v6", Publish([], transport=Connect("[::1]:2240")))
        dial = fsw.to_config()["coordinator"]["resources"]["v6_ep"]["real"]["tcp_dial"]
        self.assertEqual(dial["addr"], "[::1]:2240")

    def test_link_name_is_an_identifier(self) -> None:
        fsw = Target(cycle_rate=100.0)
        with self.assertRaisesRegex(ConfigError, "a link's name is an identifier"):
            fsw.add("gw-link", Publish([], transport=Listen("0.0.0.0:2240")))

    def test_link_name_takes_its_endpoint_name(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.resource("pub_ep", TcpDial("127.0.0.1:1"))
        with self.assertRaisesRegex(ConfigError, "`pub_ep` is already created"):
            fsw.add("pub", Publish([], transport=Listen("0.0.0.0:2240")))

    def test_a_target_of_links_gets_io_steps(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("cmds", Subscribe([Ping], transport=Listen("0.0.0.0:2241")))
        systems = fsw.to_config()["coordinator"]["systems"]
        self.assertEqual([s["id"] for s in systems], ["cmds", "io_submit", "io_complete"])

    def test_subscribe_rejects_duplicate_record(self) -> None:
        with self.assertRaisesRegex(ConfigError, "twice"):
            Subscribe([Ping, Ping], transport=Listen("0.0.0.0:1"))

    def test_publish_rejects_invalid_port(self) -> None:
        with self.assertRaisesRegex(ConfigError, "handles and ports"):
            Publish(["plant.imu"], transport=Listen("0.0.0.0:1"))  # type: ignore[list-item]

    def test_subscribe_rejects_invalid_record(self) -> None:
        with self.assertRaisesRegex(ConfigError, "record classes"):
            Subscribe([Plant], transport=Listen("0.0.0.0:1"))  # type: ignore[list-item]
        with self.assertRaisesRegex(ConfigError, "record classes"):
            Subscribe(["ping"], transport=Listen("0.0.0.0:1"))  # type: ignore[list-item]

    def test_subscribe_resolves_record_ports(self) -> None:
        fsw = Target(cycle_rate=100.0, namespace="cube_sat")
        cmds = fsw.add("cmds", Subscribe([Ping], transport=Listen("0.0.0.0:2241")))
        self.assertEqual(cmds.ping.ref, PortRef("cmds", "ping"))  # type: ignore[attr-defined]
        self.assertEqual(cmds.link_status.ref, PortRef("cmds", "link_status"))  # type: ignore[attr-defined]
        with self.assertRaisesRegex(AttributeError, "no output port `arm`"):
            cmds.arm  # type: ignore[attr-defined]
        entry = fsw.to_config()["coordinator"]["systems"][0]
        self.assertEqual(entry["outputs"], [{"port": "ping", "record": "ping"}])
        self.assertEqual(entry["params"]["namespace"], "cube_sat")
        self.assertEqual(entry["params"]["link"], "cmds")

    def test_config_excludes_builtin_pack(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("cmds", Subscribe([], transport=Listen("0.0.0.0:2241")))
        self.assertEqual(fsw.to_config()["packs"], [])

    def test_subscribe_loads_record_pack(self) -> None:
        fsw = Target(cycle_rate=100.0)
        cmds = fsw.add("cmds", Subscribe([Ping, MotorCmd], transport=Listen("127.0.0.1:0")))
        fsw.add("pub", Publish([cmds], transport=Listen("127.0.0.1:0")))
        self.assertEqual(fsw.to_config()["packs"], [PACK.to_json()])
        fsw.add("plant", Plant())
        self.assertEqual(fsw.to_config()["packs"], [PACK.to_json()])

    def test_publish_registration_isolation(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("plant", Plant())
        publish = Publish(all=True, transport=Listen("127.0.0.1:0"))
        fsw.add("pub1", publish)
        fsw.add("pub2", publish)
        config = fsw.to_config()
        first, second = config["coordinator"]["systems"][1:3]
        self.assertEqual((first["grants"], second["grants"]), ({"ep": "pub1_ep"}, {"ep": "pub2_ep"}))
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
        self.assertNotIn("link", publish._params)
        self.assertEqual(publish._inputs, {})

    def test_target_config_isolation(self) -> None:
        subscribe = Subscribe([Ping], transport=Listen("127.0.0.1:0"))
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
        self.assertEqual((first_entry["grants"], second_entry["grants"]), ({"ep": "a_ep"}, {"ep": "b_ep"}))
        second_entry["params"]["link"] = "changed"
        second_entry["outputs"][0]["port"] = "changed"
        self.assertEqual(first.to_config(), saved)
        self.assertEqual(second.to_config()["coordinator"]["systems"][0]["outputs"][0]["port"], "ping")

    def test_system_thread_assignment(self) -> None:
        fsw = Target(cycle_rate=100.0)
        fsw.add("db", Db(listen="0.0.0.0:2241"), thread="io")
        fsw.add("plant", Plant())
        systems = fsw.to_config()["coordinator"]["systems"]
        self.assertEqual(systems[0]["thread"], "io")
        self.assertNotIn("thread", systems[1])



def slotted() -> tuple[Target, dict[str, dict]]:
    """Commands, a plain channel, a slot, and a registry, by system id."""
    fsw = Target(cycle_rate=50.0)
    cmds = fsw.add("cmds", Subscribe([SequenceCommand], transport=Listen("127.0.0.1:0")))
    fsw.commands(cmds.sequence_command)
    nav = fsw.add("nav", Nav(), channel="nav")
    fsw.add("mode", DlSlot(allow=[Safe(hold=2.0), Hold()], initial="hold", est=nav.est),
            channel="mode", start="stopped")
    fsw.add("channels", ChannelRegistry())
    systems = fsw.to_config()["coordinator"]["systems"]
    return fsw, {system["id"]: system for system in systems}


class ChannelTest(ConfigTest):
    def test_channel_emits_adapter_and_commands(self) -> None:
        _, systems = slotted()
        nav = systems["nav"]
        self.assertEqual(nav["channel"], {"name": "nav", "start": "running"})
        self.assertNotIn("thread", nav)
        commands = {"port": "commands", "from": [{"system": "cmds", "port": "sequence_command"}]}
        self.assertIn(commands, nav["inputs"])
        self.assertEqual(systems["mode"]["channel"], {"name": "mode", "start": "stopped"})

    def test_commands_reach_later_channels(self) -> None:
        fsw = Target(cycle_rate=50.0)
        fsw.add("early", Nav(), channel="early")
        cmds = fsw.add("cmds", Subscribe([SequenceCommand], transport=Listen("127.0.0.1:0")))
        fsw.commands(cmds.sequence_command)
        fsw.add("late", Nav(), channel="late")
        systems = fsw.to_config()["coordinator"]["systems"]
        for system in (systems[0], systems[2]):
            self.assertEqual(system["inputs"][-1]["port"], "commands")

    def test_channel_exposes_its_ports(self) -> None:
        fsw = Target(cycle_rate=50.0)
        cmds = fsw.add("cmds", Subscribe([SequenceCommand], transport=Listen("127.0.0.1:0")))
        fsw.commands(cmds.sequence_command)
        nav = fsw.add("nav", Nav(), channel="nav")
        self.assertEqual(nav.events.ref, PortRef("nav", "events"))
        self.assertEqual(nav.channel_status.ref, PortRef("nav", "channel_status"))
        plain = fsw.add("plain", Nav())
        with self.assertRaisesRegex(AttributeError, "channel_status"):
            plain.channel_status  # type: ignore[attr-defined]
        pub = fsw.add("pub", Publish([nav], transport=Listen("127.0.0.1:0")))
        ports = [i["port"] for i in fsw.to_config()["coordinator"]["systems"][3]["inputs"]]
        self.assertIn("nav.channel_status", ports)
        self.assertIsNotNone(pub)

    def test_thread_and_channel_refused(self) -> None:
        fsw = Target(cycle_rate=50.0)
        with self.assertRaisesRegex(ConfigError, "not both"):
            fsw.add("nav", Nav(), thread="io", channel="nav")
        with self.assertRaisesRegex(ConfigError, "needs `channel`"):
            fsw.add("nav", Nav(), start="stopped")

    def test_slot_lists_candidates_outside_packs(self) -> None:
        fsw, systems = slotted()
        mode = systems["mode"]
        self.assertEqual(mode["ty"], "fsw.dl_slot")
        allow = mode["params"]["allow"]
        self.assertEqual([c["name"] for c in allow], ["safe", "hold"])
        self.assertEqual(allow[0]["params"], {"hold": 2.0})
        self.assertIsNone(allow[1]["params"])
        self.assertEqual(allow[0]["pack"], PACK.to_json())
        self.assertEqual(mode["params"]["initial"], "hold")
        self.assertEqual(mode["inputs"][0], {"port": "est", "from": [{"system": "nav", "port": "est"}]})
        self.assertEqual(fsw.to_config()["packs"], [PACK.to_json()])

    def test_slot_takes_first_candidate_ports(self) -> None:
        fsw = Target(cycle_rate=50.0)
        mode = fsw.add("mode", DlSlot(allow=[Safe(), Hold()]), channel="mode")
        self.assertEqual(mode.cmd.ref, PortRef("mode", "cmd"))
        self.assertEqual(mode.events.ref, PortRef("mode", "events"))

    def test_slot_refusals(self) -> None:
        fsw = Target(cycle_rate=50.0)
        nav = fsw.add("nav", Nav())
        with self.assertRaisesRegex(ConfigError, "at least one"):
            DlSlot(allow=[])
        with self.assertRaisesRegex(ConfigError, "twice"):
            DlSlot(allow=[Safe(), Safe()])
        with self.assertRaisesRegex(ConfigError, "does not list"):
            DlSlot(allow=[Safe()], initial="hold")
        with self.assertRaisesRegex(ConfigError, "on the slot"):
            DlSlot(allow=[Safe(est=nav.est)])
        with self.assertRaisesRegex(ConfigError, "pack systems"):
            DlSlot(allow=[Publish(transport=Listen("127.0.0.1:0"))])

    def test_channel_without_commands_refused(self) -> None:
        fsw = Target(cycle_rate=50.0)
        fsw.add("nav", Nav(), channel="nav")
        fsw.add("mode", DlSlot(allow=[Safe()]), channel="mode")
        with self.assertRaisesRegex(ConfigError, "channel `nav` has no command source"):
            fsw.to_config()

    def test_plain_target_needs_no_commands(self) -> None:
        fsw = Target(cycle_rate=50.0)
        fsw.add("nav", Nav())
        self.assertEqual(len(fsw.to_config()["coordinator"]["systems"]), 1)

    def test_slot_without_initial_emits_null(self) -> None:
        fsw = Target(cycle_rate=50.0)
        cmds = fsw.add("cmds", Subscribe([SequenceCommand], transport=Listen("127.0.0.1:0")))
        fsw.commands(cmds.sequence_command)
        fsw.add("mode", DlSlot(allow=[Safe(), Hold()]), channel="mode")
        mode = fsw.to_config()["coordinator"]["systems"][1]
        self.assertIn("initial", mode["params"])
        self.assertIsNone(mode["params"]["initial"])

    def test_registry_lists_channels(self) -> None:
        _, systems = slotted()
        params = systems["channels"]["params"]
        self.assertEqual(
            params["channels"],
            [{"name": "nav", "available": []}, {"name": "mode", "available": ["safe", "hold"]}],
        )
        self.assertEqual(params["period"], 50)
        self.assertEqual(systems["channels"]["ty"], "fsw.channel_registry")


if __name__ == "__main__":
    unittest.main()
