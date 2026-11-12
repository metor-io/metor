"""Tests for `Cal`, the calibrations a `Mux` mapping applies."""

from __future__ import annotations

import math
import unittest
from typing import Any

from adcs_stub import Nav, Plant

from metor_config import Cal, Calibrated, ConfigError, Target


class CalTest(unittest.TestCase):
    def setUp(self) -> None:
        target = Target(10.0)
        self.plant = target.add("plant", Plant())
        self.nav = target.add("nav", Nav(imu=self.plant.imu))

    def test_json(self) -> None:
        cases: list[tuple[Cal, Any]] = [
            (Cal.poly([1, 2.5, 3]), {"poly": [1.0, 2.5, 3.0]}),
            (Cal.linear(gain=2.0, offset=-1.0), {"poly": [-1.0, 2.0]}),
            (Cal.table([(0, 1), (2, 3)]), {"table": [[0.0, 1.0], [2.0, 3.0]]}),
            (Cal.thermocouple("K"), {"thermocouple": {"kind": "k"}}),
            (Cal.thermocouple("E"), {"thermocouple": {"kind": "e"}}),
        ]
        for cal, want in cases:
            with self.subTest(want):
                self.assertEqual(cal.to_json(), want)
                self.assertIsNone(cal.excitation)

    def test_two_point_is_a_line(self) -> None:
        cal = Cal.two_point(volts=(0.5, 4.5), eng=(0, 1000))
        self.assertEqual(cal, Cal.poly([-125, 250]))
        falling = Cal.two_point(volts=(0, 1), eng=(10, 0))
        self.assertEqual(falling.to_json(), {"poly": [10.0, -10.0]})

    def test_table_sorts(self) -> None:
        cal = Cal.table([(2.5, 1499), (0.5, 0), (1.5, 752)])
        self.assertEqual(cal.to_json(), {"table": [[0.5, 0.0], [1.5, 752.0], [2.5, 1499.0]]})
        self.assertEqual(Cal.table([(1, 2)]).to_json(), {"table": [[1.0, 2.0]]})

    def test_ratiometric(self) -> None:
        excitation = self.nav.est.field("bias.a")
        cal = Cal.ratiometric(excitation=excitation, span=(0, 0.003), eng=(0, 500))
        self.assertIs(cal.excitation, excitation)
        self.assertEqual(
            cal.to_json(),
            {
                "ratiometric": {
                    "excitation": "bias.a",
                    "span": [0.0, 0.003],
                    "eng": [0.0, 500.0],
                }
            },
        )

    def test_not_hashable(self) -> None:
        linear = Cal.linear(gain=2.0, offset=0.0)
        self.assertEqual(linear, Cal.linear(gain=2.0, offset=0.0))
        for value in (linear, Calibrated(self.nav.est.field("bias.a"), linear)):
            with self.assertRaises(TypeError):
                hash(value)

    def test_refusals(self) -> None:
        excitation = self.nav.est.field("bias.a")
        cases: list[tuple[str, Any]] = [
            ("at least one", lambda: Cal.poly([])),
            ("finite", lambda: Cal.poly([1.0, math.nan])),
            ("finite", lambda: Cal.poly([math.inf])),
            ("finite", lambda: Cal.poly(["1"])),
            ("finite", lambda: Cal.poly([True])),
            ("list", lambda: Cal.poly(1.0)),
            ("finite", lambda: Cal.linear(gain=math.nan, offset=0)),
            ("distinct", lambda: Cal.two_point(volts=(1, 1), eng=(0, 1))),
            ("takes 2 numbers", lambda: Cal.two_point(volts=(0, 1, 2), eng=(0, 1))),
            (
                "takes 2 numbers",
                lambda: Cal.ratiometric(excitation=excitation, span=(0,), eng=(0, 1)),
            ),
            ("at least one", lambda: Cal.table([])),
            ("two points at x = 1.0", lambda: Cal.table([(1, 0), (1, 2)])),
            ("finite", lambda: Cal.table([(0, math.nan)])),
            ("takes 2 numbers", lambda: Cal.table([(0, 1, 2)])),
            ("finite", lambda: Cal.table([(0, "1")])),
            ("one of", lambda: Cal.thermocouple("k")),
            ("one of", lambda: Cal.thermocouple("S")),
            (
                "nonzero width",
                lambda: Cal.ratiometric(excitation=excitation, span=(1, 1), eng=(0, 1)),
            ),
            (
                "finite",
                lambda: Cal.ratiometric(excitation=excitation, span=(0, 1), eng=(0, math.inf)),
            ),
            (
                "f64 excitation",
                lambda: Cal.ratiometric(
                    excitation=self.nav.est.field("q"), span=(0, 1), eng=(0, 1)
                ),
            ),
            ("f64 excitation", lambda: Cal.ratiometric(excitation=1.0, span=(0, 1), eng=(0, 1))),
        ]
        for message, make in cases:
            with self.subTest(message), self.assertRaisesRegex(ConfigError, message):
                make()


if __name__ == "__main__":
    unittest.main()
