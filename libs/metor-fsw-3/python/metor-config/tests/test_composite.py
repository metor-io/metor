"""Tests for composites."""

from __future__ import annotations

import inspect
import unittest
from typing import NamedTuple

from adcs_stub import Ctrl, Est, Imu, Mode, MotorCmd, Nav, Plant

from metor_config import (
    ConfigError,
    Listen,
    OutPort,
    Publish,
    Scope,
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
    return s.add("a", gnc(imu=imu)), s.add("b", gnc(imu=imu))


def steps(target: Target) -> dict[str, dict]:
    return {step["id"]: step for step in target.to_config()["coordinator"]["systems"]}


def sources(step: dict, port: str) -> list[dict[str, str]]:
    return next(entry["from"] for entry in step["inputs"] if entry["port"] == port)


def without_locations(target: Target) -> list[dict]:
    systems = target.to_config()["coordinator"]["systems"]
    return [{k: v for k, v in step.items() if k != "location"} for step in systems]


class NominalTest(unittest.TestCase):
    def setUp(self) -> None:
        self.fsw = Target(10.0, namespace="sat")
        self.plant = self.fsw.add("plant", Plant())

    def test_config_matches_the_same_systems_added_by_hand(self) -> None:
        self.fsw.add("gnc", gnc(imu=self.plant.imu, gain=0.2))

        flat = Target(10.0, namespace="sat")
        plant = flat.add("plant", Plant())
        nav = flat.add("gnc.nav", Nav(imu=plant.imu, gain=0.2))
        mode = flat.add("gnc.mode", Mode())
        flat.add("gnc.ctrl", Ctrl(est=nav.est, mode=mode.cmd))

        self.assertEqual(without_locations(self.fsw), without_locations(flat))

    def test_returns_the_functions_result(self) -> None:
        out = self.fsw.add("gnc", gnc(imu=self.plant.imu))
        self.assertIsInstance(out, Gnc)
        self.assertEqual(out.motor_cmd.name, "gnc.ctrl.motor_cmd")
        self.assertEqual(out.nav.est.name, "gnc.nav.est")
        self.assertIs(out.nav.est.target, self.fsw)

    def test_result_is_a_port_list(self) -> None:
        out = self.fsw.add("gnc", gnc(imu=self.plant.imu))
        link = self.fsw.add("pub", Publish(out, transport=Listen("127.0.0.1:0")))
        self.assertEqual(
            [entry["port"] for entry in steps(self.fsw)[link._name]["inputs"]],
            ["gnc.nav.est", "gnc.nav.log", "gnc.nav.status", "gnc.ctrl.motor_cmd"],
        )

    def test_nests(self) -> None:
        self.fsw.add("gnc", pair(imu=self.plant.imu))
        self.assertIn("gnc.a.nav", steps(self.fsw))
        self.assertIn("gnc.b.ctrl", steps(self.fsw))

    def test_instances_are_independent(self) -> None:
        a = self.fsw.add("a", gnc(imu=self.plant.imu, gain=1.0))
        self.fsw.add("b", gnc(imu=self.plant.imu, gain=2.0))
        systems = steps(self.fsw)
        self.assertEqual(systems["a.nav"]["params"]["gain"], 1.0)
        self.assertEqual(systems["b.nav"]["params"]["gain"], 2.0)
        self.assertEqual(sources(systems["a.ctrl"], "est"), [a.nav.est.ref.to_json()])

    def test_one_composite_adds_twice(self) -> None:
        shared = gnc(imu=self.plant.imu)
        self.fsw.add("a", shared)
        self.fsw.add("b", shared)
        self.assertIn("a.nav", steps(self.fsw))
        self.assertIn("b.nav", steps(self.fsw))

    def test_systems_run_where_the_composite_is_added(self) -> None:
        self.fsw.add("gnc", gnc(imu=self.plant.imu))
        self.fsw.add("after", Mode())
        self.assertEqual(
            [entry.name for entry in self.fsw._systems],
            ["plant", "gnc.nav", "gnc.mode", "gnc.ctrl", "after"],
        )

    def test_location_is_the_line_in_the_body(self) -> None:
        self.fsw.add("gnc", gnc(imu=self.plant.imu))
        location = steps(self.fsw)["gnc.nav"]["location"]
        self.assertTrue(location["file"].endswith("test_composite.py"))
        decorator = inspect.unwrap(gnc).__code__.co_firstlineno
        self.assertEqual(location["line"], decorator + 2)


class EdgeTest(unittest.TestCase):
    def setUp(self) -> None:
        self.fsw = Target(10.0, namespace="sat")

    def test_body_runs_at_add(self) -> None:
        calls: list[str] = []

        @composite
        def record(s: Scope) -> None:
            calls.append(s._prefix)

        pending = record()
        self.assertEqual(calls, [])
        self.fsw.add("r", pending)
        self.assertEqual(calls, ["r"])

    def test_empty_composite(self) -> None:
        @composite
        def empty(s: Scope) -> None:
            return None

        self.assertIsNone(self.fsw.add("e", empty()))
        self.assertEqual(steps(self.fsw), {})

    def test_loop_closes_through_the_result(self) -> None:
        @composite
        def inner(s: Scope) -> OutPort[MotorCmd]:
            looped = s.loop(MotorCmd)
            s.add("plant", Plant(motor_cmd=looped))
            mode = s.add("mode", Mode())
            nav = s.add("nav", Nav())
            ctrl = s.add("ctrl", Ctrl(est=nav.est, mode=mode.cmd))
            looped.connect(ctrl.motor_cmd)
            return ctrl.motor_cmd

        self.fsw.add("g", inner())
        self.assertEqual(
            sources(steps(self.fsw)["g.plant"], "motor_cmd"),
            [{"system": "g.ctrl", "port": "motor_cmd"}],
        )

    def test_outer_loop_feeds_an_input(self) -> None:
        @composite
        def plant_in(s: Scope, *, cmd: Sources[MotorCmd]) -> Plant:
            return s.add("plant", Plant(motor_cmd=cmd))

        cmd = self.fsw.loop(MotorCmd)
        self.fsw.add("g", plant_in(cmd=cmd))
        mode = self.fsw.add("mode", Mode())
        nav = self.fsw.add("nav", Nav())
        cmd.connect(self.fsw.add("ctrl", Ctrl(est=nav.est, mode=mode.cmd)).motor_cmd)
        self.assertEqual(
            sources(steps(self.fsw)["g.plant"], "motor_cmd"),
            [{"system": "ctrl", "port": "motor_cmd"}],
        )

    def test_resource_takes_the_prefix(self) -> None:
        @composite
        def uart(s: Scope) -> None:
            s.resource("port", Tty("/dev/ttyS0"))

        @composite
        def outer(s: Scope) -> None:
            s.add("in", uart())

        self.fsw.add("a", uart())
        self.fsw.add("b", outer())
        self.assertEqual(set(self.fsw._resources), {"a_port", "b_in_port"})

    def test_sim_takes_the_prefix(self) -> None:
        @composite
        def model(s: Scope) -> None:
            s.sim("plant", Plant())

        self.fsw.add("g", model())
        self.assertEqual([entry.name for entry in self.fsw._sims], ["g.plant"])

    def test_each_instance_has_its_own_channel(self) -> None:
        @composite
        def seq(s: Scope) -> None:
            s.add("mode", Mode(), channel="mode", start="stopped")

        self.fsw.add("a", seq())
        self.fsw.add("b", seq())
        self.assertEqual([name for name, _ in self.fsw._channels()], ["a.mode", "b.mode"])

    def test_threads_are_shared(self) -> None:
        @composite
        def threaded(s: Scope) -> None:
            s.add("nav", Nav(), thread="worker")

        self.fsw.add("a", threaded())
        self.fsw.add("b", threaded())
        self.assertEqual(
            [steps(self.fsw)[name]["thread"] for name in ("a.nav", "b.nav")], ["worker", "worker"]
        )


class FailureTest(unittest.TestCase):
    def setUp(self) -> None:
        self.fsw = Target(10.0, namespace="sat")
        self.imu = self.fsw.add("plant", Plant()).imu

    def test_composite_takes_no_adapter(self) -> None:
        for adapter in ({"thread": "t"}, {"channel": "c"}, {"start": "stopped"}):
            with self.assertRaisesRegex(ConfigError, "takes no adapter"):
                self.fsw.add("gnc", gnc(imu=self.imu), **adapter)  # pyright: ignore[reportCallIssue, reportArgumentType]

    def test_name_taken(self) -> None:
        self.fsw.add("gnc", gnc(imu=self.imu))
        with self.assertRaisesRegex(ConfigError, "`gnc` is already added"):
            self.fsw.add("gnc", gnc(imu=self.imu))
        with self.assertRaisesRegex(ConfigError, "`gnc` is already added"):
            self.fsw.add("gnc", Mode())
        with self.assertRaisesRegex(ConfigError, "`plant` is already added"):
            self.fsw.add("plant", gnc(imu=self.imu))
        with self.assertRaisesRegex(ConfigError, "`gnc.nav` is already added"):
            self.fsw.add("gnc.nav", Mode())

    def test_duplicate_inside_body(self) -> None:
        @composite
        def twice(s: Scope) -> None:
            s.add("mode", Mode())
            s.add("mode", Mode())

        with self.assertRaisesRegex(ConfigError, "`t.mode` is already added"):
            self.fsw.add("t", twice())

    def test_bad_names(self) -> None:
        @composite
        def dotted(s: Scope) -> None:
            s.add("a b", Mode())

        with self.assertRaisesRegex(ConfigError, "identifier, got 'a b'"):
            self.fsw.add("d", dotted())
        for name in ("", "a b", "a..b"):
            with self.assertRaisesRegex(ConfigError, "composite's name is an identifier"):
                self.fsw.add(name, gnc(imu=self.imu))

    def test_link_is_refused(self) -> None:
        @composite
        def comms(s: Scope) -> None:
            s.add("pub", Publish(transport=Listen("127.0.0.1:0"), all=True))

        @composite
        def rx(s: Scope) -> None:
            s.add("sub", Subscribe([Est], transport=Listen("127.0.0.1:0")))

        for name, body in (("tx", comms), ("rx", rx)):
            with self.assertRaisesRegex(ConfigError, "a link's name is an identifier"):
                self.fsw.add(name, body())

    def test_input_from_another_target(self) -> None:
        other = Target(10.0, namespace="other").add("plant", Plant())
        self.fsw.add("gnc", gnc(imu=other.imu))
        with self.assertRaisesRegex(ConfigError, "on another target"):
            self.fsw.to_config()


if __name__ == "__main__":
    unittest.main()
