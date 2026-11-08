"""Tests for lists of ports: flattening inputs and reading outputs."""

from __future__ import annotations

import unittest
from typing import Any, Sequence

from adcs_stub import PACK, Est, Imu, LogEvent, Plant, SystemStatus
from test_target import reset

from metor_config import ConfigError, OutPort, Sources, System, Target


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


if __name__ == "__main__":
    unittest.main()
