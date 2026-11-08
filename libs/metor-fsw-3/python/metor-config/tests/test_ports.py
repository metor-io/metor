"""Tests for lists and groups of ports: flattening inputs and reading outputs."""

from __future__ import annotations

import unittest
from typing import TYPE_CHECKING, Any, Sequence

from adcs_stub import PACK, Est, Imu, LogEvent, Plant, SystemStatus
from test_target import reset

from metor_config import ConfigError, OutGroup, OutPort, PortGroup, Sources, System, Target


class Sun(System):
    _pack = PACK
    _ty = "sun"
    _input_ports = {
        "fss.0": Imu,
        "fss.1": Imu,
        "fss.2": Imu,
        "grid.0.0": Imu,
        "grid.0.1": Imu,
        "grid.1.0": Imu,
        "grid.1.1": Imu,
        "gain": Est,
    }
    _ports = {
        "est.0": Est,
        "est.1": Est,
        "pairs.0.0": Est,
        "pairs.0.1": Est,
        "log": LogEvent,
        "status": SystemStatus,
    }

    def __init__(
        self,
        *,
        fss: Sequence[Sources[Imu]] | None = None,
        grid: Sequence[Sequence[Sources[Imu]]] | None = None,
        gain: Sources[Est] = (),
    ) -> None:
        super().__init__({"fss": fss, "grid": grid, "gain": gain}, {})

    est: tuple[OutPort[Est], ...]
    pairs: tuple[tuple[OutPort[Est], ...], ...]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


def inputs(target: Target, name: str) -> dict[str, list[str]]:
    systems = target.to_config()["coordinator"]["systems"]
    system = next(system for system in systems if system["id"] == name)
    return {
        entry["port"]: [f"{src['system']}.{src['port']}" for src in entry["from"]]
        for entry in system.get("inputs", [])
    }


class ListTest(unittest.TestCase):
    def setUp(self) -> None:
        reset()
        self.target = Target(10.0)
        self.a = self.target.add("a", Plant())
        self.b = self.target.add("b", Plant())

    def tearDown(self) -> None:
        reset()

    def test_list_inputs_flatten_to_elements(self) -> None:
        self.target.add("sun", Sun(fss=[self.a.imu, [self.a.imu, self.b.imu], ()]))
        self.assertEqual(
            inputs(self.target, "sun"),
            {"fss.0": ["a.imu"], "fss.1": ["a.imu", "b.imu"], "fss.2": [], "gain": []},
        )

    def test_nested_lists_flatten(self) -> None:
        imu = self.a.imu
        self.target.add("sun", Sun(grid=[[imu, ()], [(), self.b.imu]]))
        wired = inputs(self.target, "sun")
        self.assertEqual(wired["grid.0.0"], ["a.imu"])
        self.assertEqual(wired["grid.1.1"], ["b.imu"])
        self.assertEqual(wired["grid.0.1"], [])

    def test_unwired_list_takes_no_input(self) -> None:
        self.target.add("sun", Sun())
        self.assertEqual(inputs(self.target, "sun"), {"gain": []})

    def test_none_leaf_takes_no_input(self) -> None:
        none: Any = None
        self.target.add("sun", Sun(fss=[self.a.imu, none, ()], gain=none))
        self.assertEqual(inputs(self.target, "sun"), {"fss.0": ["a.imu"], "fss.2": []})
        wheel = Adcs.Wheels(speed=none)
        self.target.add("adcs", Adcs(wheels=[wheel, Adcs.Wheels()], mag=none))
        self.assertEqual(inputs(self.target, "adcs"), {})

    def test_list_of_wrong_length_is_refused(self) -> None:
        with self.assertRaisesRegex(ConfigError, "`fss` takes a list of 3, got 2"):
            Sun(fss=[self.a.imu, self.b.imu])
        with self.assertRaisesRegex(ConfigError, "`grid.1` takes a list of 2, got 1"):
            Sun(grid=[[(), ()], [()]])

    def test_port_for_a_list_is_refused(self) -> None:
        port: Any = self.a.imu
        with self.assertRaisesRegex(ConfigError, "`fss` takes a list of 3"):
            Sun(fss=port)

    def test_list_outputs_read_as_tuples(self) -> None:
        sun = self.target.add("sun", Sun())
        self.assertEqual([port.name for port in sun.est], ["sun.est.0", "sun.est.1"])
        self.assertEqual(sun.est[1].record, Est)
        self.assertEqual([port.name for port in sun.pairs[0]], ["sun.pairs.0.0", "sun.pairs.0.1"])
        self.assertEqual(sun.log.name, "sun.log")

    def test_element_output_wires_an_input(self) -> None:
        sun = self.target.add("sun", Sun())
        self.target.add("next", Sun(gain=sun.est[1]))
        self.assertEqual(inputs(self.target, "next")["gain"], ["sun.est.1"])

    def test_missing_output_is_an_attribute_error(self) -> None:
        sun = self.target.add("sun", Sun())
        with self.assertRaises(AttributeError):
            sun.missing  # pyright: ignore[reportAttributeAccessIssue]


class Adcs(System):
    _pack = PACK
    _ty = "adcs"

    class Wheels(PortGroup):
        _input_ports = {"speed": Imu, "pair.0": Est, "pair.1": Est}
        if TYPE_CHECKING:

            def __init__(
                self, *, speed: Sources[Imu] = (), pair: Sequence[Sources[Est]] | None = None
            ) -> None: ...

    class WheelsOut(OutGroup):
        cmd: OutPort[Est]

    _input_ports = {
        "wheels.0.speed": Imu,
        "wheels.0.pair.0": Est,
        "wheels.0.pair.1": Est,
        "wheels.1.speed": Imu,
        "wheels.1.pair.0": Est,
        "wheels.1.pair.1": Est,
        "mag": Imu,
    }
    _ports = {"wheels.0.cmd": Est, "wheels.1.cmd": Est, "log": LogEvent, "status": SystemStatus}

    def __init__(
        self,
        *,
        wheels: Sequence[Adcs.Wheels] | None = None,
        mag: Sources[Imu] = (),
    ) -> None:
        super().__init__({"wheels": wheels, "mag": mag}, {})

    wheels: tuple[Adcs.WheelsOut, ...]
    log: OutPort[LogEvent]
    status: OutPort[SystemStatus]


class GroupTest(unittest.TestCase):
    def setUp(self) -> None:
        reset()
        self.target = Target(10.0)
        self.a = self.target.add("a", Plant())

    def tearDown(self) -> None:
        reset()

    def test_groups_flatten_to_their_ports(self) -> None:
        wheel = Adcs.Wheels(speed=self.a.imu)
        self.target.add("adcs", Adcs(wheels=[wheel, Adcs.Wheels()]))
        self.assertEqual(
            inputs(self.target, "adcs"),
            {"wheels.0.speed": ["a.imu"], "mag": []},
        )

    def test_group_holds_a_list(self) -> None:
        sun = self.target.add("sun", Sun())
        wheel = Adcs.Wheels(pair=[sun.est[0], [sun.est[1], sun.est[0]]])
        self.target.add("adcs", Adcs(wheels=[wheel, Adcs.Wheels()]))
        wired = inputs(self.target, "adcs")
        self.assertEqual(wired["wheels.0.pair.1"], ["sun.est.1", "sun.est.0"])
        with self.assertRaisesRegex(ConfigError, "`pair` takes a list of 2, got 1"):
            Adcs.Wheels(pair=[sun.est[0]])

    def test_unknown_keyword_is_refused(self) -> None:
        with self.assertRaisesRegex(ConfigError, "`Adcs.Wheels` has no input `torque`"):
            Adcs.Wheels(torque=self.a.imu)  # pyright: ignore[reportCallIssue]

    def test_mismatched_record_is_refused(self) -> None:
        sun = self.target.add("sun", Sun())
        port: Any = sun.est[0]
        with self.assertRaisesRegex(ConfigError, "`Adcs.Wheels.speed` takes `Imu`, got `Est`"):
            Adcs.Wheels(speed=port)

    def test_port_for_a_group_is_refused(self) -> None:
        port: Any = self.a.imu
        with self.assertRaisesRegex(ConfigError, "`wheels.0` takes a port group"):
            Adcs(wheels=[port, Adcs.Wheels()])

    def test_output_groups_read_as_attributes(self) -> None:
        adcs = self.target.add("adcs", Adcs())
        self.assertEqual(adcs.wheels[1].cmd.name, "adcs.wheels.1.cmd")
        self.target.add("sun", Sun(gain=adcs.wheels[0].cmd))
        self.assertEqual(inputs(self.target, "sun")["gain"], ["adcs.wheels.0.cmd"])
        with self.assertRaises(AttributeError):
            adcs.wheels[0].speed  # pyright: ignore[reportAttributeAccessIssue]


if __name__ == "__main__":
    unittest.main()
