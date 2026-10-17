"""Tests for resources, grants, and the io steps in `to_config()`."""

from __future__ import annotations

import json
import os
import runpy
import sys
import unittest
from typing import Any

from adcs_stub import PACK

import metor_config._target as target_mod
from metor_config import (
    ConfigError,
    Dir,
    OutPort,
    Publish,
    Record,
    ResourceHandle,
    ResourceStatus,
    SimGpioLine,
    SimUart,
    System,
    Target,
    Tty,
    Udp,
)


class Fix(Record):
    _kind = "msg"
    _name = "gps_fix"
    _pack = PACK


class IoSqe(Record):
    _kind = "msg"
    _name = "io_sqe"
    _pack = PACK


class Gps(System):
    """A driver with one `Cap<Uart>`, as a pack module renders it."""

    _pack = PACK
    _ty = "gps"
    _ports = {"fix": Fix}
    _caps = {"uart": "uart"}

    def __init__(self, *, uart: ResourceHandle) -> None:
        super().__init__({}, {}, grants={"uart": uart})

    fix: OutPort[Fix]


class Leaky(System):
    """A hand-written module that lists its SQ as a port."""

    _pack = PACK
    _ty = "leaky"
    _ports = {"io_sq": IoSqe, "fix": Fix}
    _caps = {"uart": "uart"}

    def __init__(self, *, uart: ResourceHandle) -> None:
        super().__init__({}, {}, grants={"uart": uart})


class UartModel(System):
    """A model with one `Cap<Uart>`, as a pack module renders it."""

    _pack = PACK
    _ty = "uart_model"
    _ports = {}
    _caps = {"uart": "uart"}

    def __init__(self, *, uart: ResourceHandle) -> None:
        super().__init__({}, {}, grants={"uart": uart})


def _holder(interface: str, handle: ResourceHandle) -> System:
    """A system with one `Cap` of ``interface``, granted ``handle``."""

    class Holder(System):
        _pack = PACK
        _ty = "holder"
        _ports = {}
        _caps = {"cap": interface}

        def __init__(self) -> None:
            super().__init__({}, {}, grants={"cap": handle})

    return Holder()


def coordinator(target: Target) -> dict[str, Any]:
    return target.to_config()["coordinator"]


class ResourceTests(unittest.TestCase):
    def tearDown(self) -> None:
        target_mod._targets.clear()

    def test_granted_tty_emits_resource_grant_and_io_steps(self) -> None:
        fsw = Target(cycle_rate=10.0)
        gps1 = fsw.resource("gps1", Tty("/dev/ttyTHS0", baud=9600))
        fsw.add("gps", Gps(uart=gps1))
        config = coordinator(fsw)
        self.assertEqual(
            config["resources"],
            {"gps1": {"real": {"tty": {"path": "/dev/ttyTHS0", "baud": 9600}}}},
        )
        gps, submit, complete = config["systems"]
        self.assertEqual(gps["grants"], {"uart": "gps1"})
        self.assertEqual(gps["inputs"], [], "the build wires io_cq")
        self.assertEqual(submit, {"id": "io_submit", "ty": "fsw.io"})
        self.assertEqual(
            complete, {"id": "io_complete", "ty": "fsw.io", "params": {"status": True}}
        )

    def test_resource_status_names_io_status(self) -> None:
        self.assertEqual((ResourceStatus._name, ResourceStatus._kind), ("io_status", "frame"))

    def test_two_systems_share_one_resource(self) -> None:
        fsw = Target(cycle_rate=10.0)
        gps1 = fsw.resource("gps1", Tty("/dev/ttyTHS0"))
        fsw.add("a", Gps(uart=gps1))
        fsw.add("b", Gps(uart=gps1))
        config = coordinator(fsw)
        self.assertEqual(len(config["resources"]), 1)
        a, b = config["systems"][:2]
        self.assertEqual((a["grants"], b["grants"]), ({"uart": "gps1"}, {"uart": "gps1"}))

    def test_no_grants_emits_no_io(self) -> None:
        fsw = Target(cycle_rate=10.0)
        fsw.add("pub", Publish([], listen="127.0.0.1:0"))
        config = coordinator(fsw)
        self.assertNotIn("resources", config)
        self.assertEqual([s["id"] for s in config["systems"]], ["pub"])

    def test_publish_all_skips_io_records(self) -> None:
        fsw = Target(cycle_rate=10.0)
        gps1 = fsw.resource("gps1", Tty("/dev/ttyTHS0"))
        fsw.add("gps", Leaky(uart=gps1))
        fsw.add("pub", Publish([], listen="127.0.0.1:0", all=True))
        published = coordinator(fsw)["systems"][1]["inputs"]
        self.assertEqual([i["port"] for i in published], ["gps.fix"])

    def test_wrong_interface_is_refused(self) -> None:
        fsw = Target(cycle_rate=10.0)
        logs = fsw.resource("logs", Dir("/var/log"))
        with self.assertRaisesRegex(ConfigError, "gps.uart"):
            fsw.add("gps", Gps(uart=logs))

    def test_resource_of_another_target_is_refused(self) -> None:
        fsw, other = Target(cycle_rate=10.0), Target(cycle_rate=10.0)
        gps1 = other.resource("gps1", Tty("/dev/ttyTHS0"))
        with self.assertRaisesRegex(ConfigError, "this target"):
            fsw.add("gps", Gps(uart=gps1))

    def test_missing_grant_is_refused(self) -> None:
        fsw = Target(cycle_rate=10.0)
        gps = Gps(uart=fsw.resource("gps1", Tty("/dev/ttyTHS0")))
        gps._grants.clear()
        with self.assertRaisesRegex(ConfigError, "not granted"):
            fsw.add("gps", gps)

    def test_repeated_or_bad_resource_name_is_refused(self) -> None:
        fsw = Target(cycle_rate=10.0)
        fsw.resource("link", Udp("127.0.0.1:0"))
        with self.assertRaisesRegex(ConfigError, "already"):
            fsw.resource("link", Udp("127.0.0.1:0"))
        with self.assertRaisesRegex(ConfigError, "identifier"):
            fsw.resource("not a name", Udp("127.0.0.1:0"))

    def test_io_status_reads_io_complete(self) -> None:
        fsw = Target(cycle_rate=10.0)
        with self.assertRaisesRegex(ConfigError, "io_status"):
            fsw.io_status
        fsw.add("gps", Gps(uart=fsw.resource("gps1", Tty("/dev/ttyTHS0"))))
        port = fsw.io_status
        self.assertEqual((port.ref.system, port.ref.port), ("io_complete", "io_status"))
        self.assertIs(port.record, ResourceStatus)

    def test_io_step_names_are_reserved(self) -> None:
        fsw = Target(cycle_rate=10.0)
        gps1 = fsw.resource("gps1", Tty("/dev/ttyTHS0"))
        with self.assertRaisesRegex(ConfigError, "io step"):
            fsw.add("io_submit", Gps(uart=gps1))


class SimTests(unittest.TestCase):
    def tearDown(self) -> None:
        target_mod._targets.clear()

    def test_sim_kinds_emit_their_backing(self) -> None:
        for kind, key in ((SimUart(), "uart"), (SimGpioLine(), "gpio_line")):
            fsw = Target(cycle_rate=10.0)
            line = fsw.resource("line", kind)
            fsw.add("a", _holder(kind.interface, line))
            fsw.sim("b", _holder(kind.interface, line.device))
            (backing,) = coordinator(fsw)["resources"].values()
            self.assertEqual(backing["sim"]["kind"], key)
            target_mod._targets.clear()

    def test_sim_resource_names_its_holders(self) -> None:
        fsw = Target(cycle_rate=10.0)
        gps1 = fsw.resource("gps1", SimUart())
        fsw.add("gps", Gps(uart=gps1))
        fsw.sim("model", UartModel(uart=gps1.device))
        config = coordinator(fsw)
        gps, submit, model, complete = config["systems"]
        self.assertNotIn("grants", gps)
        self.assertNotIn("grants", model)
        self.assertEqual(
            config["resources"],
            {
                "gps1": {
                    "sim": {
                        "kind": "uart",
                        "host": {"system": "gps", "param": "uart"},
                        "device": {"system": "model", "param": "uart"},
                    }
                }
            },
        )

    def test_sim_resource_held_on_one_end_is_refused(self) -> None:
        fsw = Target(cycle_rate=10.0)
        fsw.add("gps", Gps(uart=fsw.resource("gps1", SimUart())))
        with self.assertRaisesRegex(ConfigError, "`gps1` has no holder on its device end"):
            fsw.to_config()

    def test_real_and_sim_resources_mix(self) -> None:
        fsw = Target(cycle_rate=10.0)
        sim = fsw.resource("sim", SimUart())
        fsw.add("a", Gps(uart=fsw.resource("port", Tty("/dev/ttyS0"))))
        fsw.add("b", Gps(uart=sim))
        fsw.sim("model", UartModel(uart=sim.device))
        config = coordinator(fsw)
        a, b = config["systems"][:2]
        self.assertEqual(a["grants"], {"uart": "port"})
        self.assertNotIn("grants", b)
        self.assertEqual(len(config["resources"]), 2)

    def test_real_resource_has_no_device_end(self) -> None:
        fsw = Target(cycle_rate=10.0)
        port = fsw.resource("port", Tty("/dev/ttyS0"))
        with self.assertRaisesRegex(ConfigError, "`port` is real"):
            port.device

    def test_models_run_between_the_io_steps(self) -> None:
        fsw = Target(cycle_rate=10.0)
        gps1 = fsw.resource("gps1", SimUart())
        fsw.add("gps", Gps(uart=gps1))
        fsw.sim("model", UartModel(uart=gps1.device))
        fsw.add("late", Gps(uart=fsw.resource("gps2", SimUart())))
        fsw.sim("model2", UartModel(uart=fsw._resources["gps2"].device))
        order = [s["id"] for s in coordinator(fsw)["systems"]]
        self.assertEqual(order, ["gps", "late", "io_submit", "model", "model2", "io_complete"])

    def test_model_added_as_flight_is_wired_like_a_driver(self) -> None:
        fsw = Target(cycle_rate=10.0)
        gps1 = fsw.resource("gps1", SimUart())
        fsw.add("gps", Gps(uart=gps1))
        fsw.add("model", UartModel(uart=gps1.device))
        systems = coordinator(fsw)["systems"]
        self.assertEqual([s["id"] for s in systems], ["gps", "model", "io_submit", "io_complete"])

    def test_sim_golden_matches(self) -> None:
        golden = os.path.join(os.path.dirname(__file__), "..", "..", "..", "tests", "golden")
        sys.path.insert(0, golden)
        try:
            runpy.run_path(os.path.join(golden, "sim.py"))
        finally:
            sys.path.remove(golden)
        written = target_mod._targets[-1].to_config()["coordinator"]
        with open(os.path.join(golden, "sim.json")) as file:
            self.assertEqual(written, json.load(file))


if __name__ == "__main__":
    unittest.main()
