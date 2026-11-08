"""Tests for composites."""

from __future__ import annotations

import inspect
import unittest
from typing import NamedTuple

from adcs_stub import Ctrl, Imu, Mode, MotorCmd, Nav, Plant

from metor_config import (
    ChannelRegistry,
    ConfigError,
    Listen,
    OutPort,
    Publish,
    Scope,
    SequenceCommand,
    Sources,
    Subscribe,
    Target,
    Tty,
    composite,
)


class Gnc(NamedTuple):
    nav: Nav
    motor_cmd: OutPort[MotorCmd]


@composite
def gnc(s: Scope, *, imu: Sources[Imu], gain: float = 0.1) -> Gnc:
    nav = s.add("nav", Nav(imu=imu, gain=gain))
    mode = s.add("mode", Mode())
    ctrl = s.add("ctrl", Ctrl(est=nav.est, mode=mode.cmd))
    return Gnc(nav, ctrl.motor_cmd)


@composite
def pair(s: Scope, *, imu: Sources[Imu]) -> tuple[Gnc, Gnc]:
    return s.add("a", gnc(imu=imu, gain=1.0)), s.add("b", gnc(imu=imu, gain=2.0))


@composite
def sensing(s: Scope, *, truth: Sources[Imu] | None = None) -> Mode:
    if truth is not None:
        s.sim("model", Nav(imu=truth))
    return s.add("mode", Mode())


def steps(target: Target) -> dict[str, dict]:
    return {step["id"]: step for step in target.to_config()["coordinator"]["systems"]}


def without_locations(target: Target) -> list[dict]:
    systems = target.to_config()["coordinator"]["systems"]
    return [{k: v for k, v in step.items() if k != "location"} for step in systems]


class CompositeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.fsw = Target(10.0, namespace="sat")
        self.imu = self.fsw.add("plant", Plant()).imu

    def test_config_matches_the_same_systems_added_by_hand(self) -> None:
        out = self.fsw.add("gnc", gnc(imu=self.imu, gain=0.2))
        self.assertEqual(out.motor_cmd.name, "gnc.ctrl.motor_cmd")

        flat = Target(10.0, namespace="sat")
        plant = flat.add("plant", Plant())
        nav = flat.add("gnc.nav", Nav(imu=plant.imu, gain=0.2))
        mode = flat.add("gnc.mode", Mode())
        flat.add("gnc.ctrl", Ctrl(est=nav.est, mode=mode.cmd))

        self.assertEqual(without_locations(self.fsw), without_locations(flat))

    def test_nested_and_repeated_instances_are_independent(self) -> None:
        shared = pair(imu=self.imu)
        self.fsw.add("x", shared)
        self.fsw.add("y", shared)
        systems = steps(self.fsw)
        self.assertEqual(systems["x.a.nav"]["params"]["gain"], 1.0)
        self.assertEqual(systems["y.b.nav"]["params"]["gain"], 2.0)
        self.assertEqual(
            next(i["from"] for i in systems["y.b.ctrl"]["inputs"] if i["port"] == "est"),
            [{"system": "y.b.nav", "port": "est"}],
        )

    def test_optional_input_adds_its_model_only_when_given(self) -> None:
        self.fsw.add("g", sensing(truth=self.imu))
        self.fsw.add("h", sensing())
        systems = steps(self.fsw)
        self.assertEqual(
            next(i["from"] for i in systems["g.model"]["inputs"] if i["port"] == "imu"),
            [self.imu.ref.to_json()],
        )
        self.assertNotIn("h.model", systems)
        self.assertIn("h.mode", systems)

    def test_channel_registry_tells_reused_instances_apart(self) -> None:
        shared = Mode()

        @composite
        def seq(s: Scope) -> None:
            s.add("mode", shared, channel="c")

        self.fsw.add("a", seq())
        self.fsw.add("b", seq())
        self.fsw.add("reg", ChannelRegistry())
        cmds = self.fsw.add("cmds", Subscribe([SequenceCommand], transport=Listen("127.0.0.1:0")))
        self.fsw.commands(cmds.sequence_command)
        specs = steps(self.fsw)["reg"]["params"]["channels"]
        self.assertEqual(
            [(spec["name"], spec["components"]["state"]) for spec in specs],
            [
                ("a.c", "sat.a.mode.channel_status.state"),
                ("b.c", "sat.b.mode.channel_status.state"),
            ],
        )


class FailureTest(unittest.TestCase):
    def setUp(self) -> None:
        self.fsw = Target(10.0, namespace="sat")
        self.imu = self.fsw.add("plant", Plant()).imu

    def test_composite_takes_no_thread_channel_or_start(self) -> None:
        for adapter in ({"thread": "t"}, {"channel": "c"}, {"start": "stopped"}):
            with self.assertRaisesRegex(ConfigError, "takes no `thread`, `channel`, or `start`"):
                self.fsw.add("gnc", gnc(imu=self.imu), **adapter)  # pyright: ignore[reportCallIssue, reportArgumentType]

    def test_name_taken(self) -> None:
        self.fsw.add("gnc", gnc(imu=self.imu))
        with self.assertRaisesRegex(ConfigError, "`gnc` is already added"):
            self.fsw.add("gnc", gnc(imu=self.imu))
        with self.assertRaisesRegex(ConfigError, "`gnc` is already added"):
            self.fsw.add("gnc", Mode())
        with self.assertRaisesRegex(ConfigError, "`plant` is already added"):
            self.fsw.add("plant", gnc(imu=self.imu))

    def test_duplicate_inside_body(self) -> None:
        @composite
        def twice(s: Scope) -> None:
            s.add("mode", Mode())
            s.add("mode", Mode())

        with self.assertRaisesRegex(ConfigError, "`t.mode` is already added"):
            self.fsw.add("t", twice())

    def test_bad_names(self) -> None:
        @composite
        def spaced(s: Scope) -> None:
            s.add("a b", Mode())

        with self.assertRaisesRegex(ConfigError, "joined by `.`, got 'd.a b'"):
            self.fsw.add("d", spaced())
        for name in ("", "a b", "a..b"):
            with self.assertRaisesRegex(ConfigError, "joined by `.`"):
                self.fsw.add(name, gnc(imu=self.imu))

    def test_link_is_refused(self) -> None:
        @composite
        def comms(s: Scope) -> None:
            s.add("pub", Publish(transport=Listen("127.0.0.1:0"), all=True))

        with self.assertRaisesRegex(ConfigError, "a link's name is an identifier"):
            self.fsw.add("tx", comms())

    def test_overlapping_ids(self) -> None:
        self.fsw.add("mode", Mode())
        self.fsw.add("a.b", Mode())
        with self.assertRaisesRegex(ConfigError, "`mode.x` overlaps system `mode`"):
            self.fsw.add("mode.x", gnc(imu=self.imu))
        with self.assertRaisesRegex(ConfigError, "`a` overlaps system `a.b`"):
            self.fsw.add("a", gnc(imu=self.imu))
        self.fsw.add("gnc", gnc(imu=self.imu))
        self.fsw.add("gnc2", gnc(imu=self.imu))
        with self.assertRaisesRegex(ConfigError, "`gnc.extra` is under composite `gnc`"):
            self.fsw.add("gnc.extra", Mode())

    def test_resource_names_collide(self) -> None:
        @composite
        def uart(s: Scope) -> None:
            s.resource("c", Tty("/dev/ttyS0"))

        @composite
        def outer(s: Scope) -> None:
            s.add("b", uart())

        self.fsw.add("a", outer())
        with self.assertRaisesRegex(ConfigError, "resource `c` of composite `a_b` is `a_b_c`"):
            self.fsw.add("a_b", uart())

    def test_not_a_system(self) -> None:
        for add in (self.fsw.add, self.fsw.sim):
            with self.assertRaisesRegex(ConfigError, "takes a system"):
                add("g", gnc)  # pyright: ignore[reportCallIssue, reportArgumentType]
        with self.assertRaisesRegex(ConfigError, "takes a system"):
            self.fsw.sim("g", gnc(imu=self.imu))  # pyright: ignore[reportArgumentType]

    def test_arguments_fail_at_the_call(self) -> None:
        with self.assertRaises(TypeError):
            gnc(bogus=1)  # pyright: ignore[reportCallIssue]
        self.assertEqual(list(inspect.signature(gnc).parameters), ["imu", "gain"])

    def test_system_input_given_none_is_refused(self) -> None:
        @composite
        def careless(s: Scope, *, truth: Sources[Imu] | None = None) -> Nav:
            return s.add("nav", Nav(imu=truth))  # pyright: ignore[reportArgumentType]

        with self.assertRaisesRegex(ConfigError, "input `imu` takes ports and loops, got None"):
            self.fsw.add("g", careless())

    def test_nested_result_is_not_a_port_list(self) -> None:
        out = self.fsw.add("p", pair(imu=self.imu))
        with self.assertRaisesRegex(ConfigError, "a port list takes handles and ports"):
            Publish(out, transport=Listen("127.0.0.1:0"))
        self.fsw.add("pub", Publish([*out[0], *out[1]], transport=Listen("127.0.0.1:0")))

    def test_input_from_another_target(self) -> None:
        other = Target(10.0, namespace="other").add("plant", Plant())
        self.fsw.add("gnc", gnc(imu=other.imu))
        with self.assertRaisesRegex(ConfigError, "on another target"):
            self.fsw.to_config()


if __name__ == "__main__":
    unittest.main()
