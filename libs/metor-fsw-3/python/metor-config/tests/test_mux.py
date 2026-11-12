"""Tests for the `Mux` builtin's config."""

from __future__ import annotations

import unittest
from typing import Any

from adcs_stub import PACK, ModeCmd, MotorCmd, Nav, Plant

from metor_config import Cal, Calibrated, ConfigError, Mux, Target


def entry(target: Target, name: str) -> dict[str, Any]:
    systems = target.to_config()["coordinator"]["systems"]
    return next(system for system in systems if system["id"] == name)


class MuxTest(unittest.TestCase):
    def setUp(self) -> None:
        self.target = Target(10.0)
        self.plant = self.target.add("plant", Plant())
        self.nav = self.target.add("nav", Nav(imu=self.plant.imu))

    def test_split(self) -> None:
        gyro = self.plant.imu.field("gyro")
        self.target.add("split", Mux(cmd=MotorCmd(torque=gyro)))
        config = entry(self.target, "split")
        self.assertEqual([i["port"] for i in config["inputs"]], ["plant.imu"])
        self.assertEqual(config["outputs"], [{"port": "cmd", "record": "motor_cmd"}])
        self.assertEqual(
            config["params"],
            {
                "inputs": ["plant.imu"],
                "outputs": [
                    {
                        "port": "cmd",
                        "on": {"input": "plant.imu"},
                        "stamp": {"input": "plant.imu"},
                        "fields": [{"dst": "torque", "input": "plant.imu", "src": "gyro"}],
                    }
                ],
            },
        )

    def test_combine_feeds_another_system(self) -> None:
        cmd = MotorCmd(torque=self.plant.imu.field("gyro"), trim=self.nav.est.field("bias"))
        mux = self.target.add("mix", Mux(cmd=cmd, on=self.nav.est, max_age=0.5))
        self.assertIs(mux.cmd.record, MotorCmd)
        self.target.add("plant2", Plant(motor_cmd=mux.cmd))
        params = entry(self.target, "mix")["params"]
        out = params["outputs"][0]
        self.assertEqual((out["on"], out["stamp"]), ({"input": "nav.est"}, {"input": "nav.est"}))
        self.assertEqual(out["max_age"], {"secs": 0, "nanos": 500_000_000})
        self.assertEqual(params["inputs"], ["plant.imu", "nav.est"])
        feeds = entry(self.target, "plant2")["inputs"][0]["from"]
        self.assertEqual(feeds, [{"system": "mix", "port": "cmd"}])
        self.assertIn(PACK, Mux(cmd=cmd, on="any")._record_packs)

    def test_two_outputs_share_an_input(self) -> None:
        gyro = self.plant.imu.field("gyro")
        mux = Mux(a=MotorCmd(torque=gyro), b=MotorCmd(torque=self.plant.imu.field("accel")))
        self.target.add("split", mux)
        config = entry(self.target, "split")
        self.assertEqual([i["port"] for i in config["inputs"]], ["plant.imu"])
        self.assertEqual([o["port"] for o in config["outputs"]], ["a", "b"])

    def test_unmapped_on_and_timestamp_are_inputs(self) -> None:
        cmd = MotorCmd(torque=self.plant.imu.field("gyro"))
        self.target.add("mix", Mux(cmd=cmd, on=self.nav.est, timestamp=self.plant.imu))
        params = entry(self.target, "mix")["params"]
        self.assertEqual(params["inputs"], ["plant.imu", "nav.est"])
        out = params["outputs"][0]
        self.assertEqual((out["on"], out["stamp"]), ({"input": "nav.est"}, {"input": "plant.imu"}))

    def test_rule_triggers_stamp_now(self) -> None:
        cmd = MotorCmd(torque=self.plant.imu.field("gyro"))
        self.target.add("mix", Mux(cmd=cmd, on="cycle"))
        out = entry(self.target, "mix")["params"]["outputs"][0]
        self.assertEqual((out["on"], out["stamp"]), ("cycle", "now"))

    def test_bad_arguments(self) -> None:
        gyro = self.plant.imu.field("gyro")
        both = MotorCmd(torque=gyro, trim=self.nav.est.field("bias"))
        cases: list[tuple[str, Any]] = [
            ("needs `on=`", lambda: Mux(cmd=both)),
            ("`on=` takes", lambda: Mux(cmd=MotorCmd(torque=gyro), on="every")),  # pyright: ignore[reportArgumentType]
            ("positive", lambda: Mux(cmd=MotorCmd(torque=gyro), max_age=0)),
            ("frame record instance", lambda: Mux(cmd=MotorCmd)),  # pyright: ignore[reportArgumentType]
            ("frame record instance", lambda: Mux(cmd=object.__new__(ModeCmd))),
            ("message", lambda: Mux(cmd=ModeCmd())),
        ]
        for message, make in cases:
            with self.subTest(message), self.assertRaisesRegex(ConfigError, message):
                make()

    def test_element_paths(self) -> None:
        gyro = self.plant.imu.field("gyro")
        cmd = MotorCmd(torque={0: gyro[1]}, **{"trim.a": self.nav.est.field("p")[2][3]})
        self.target.add("mix", Mux(cmd=cmd, on="cycle"))
        fields = entry(self.target, "mix")["params"]["outputs"][0]["fields"]
        self.assertEqual(
            fields,
            [
                {"dst": "torque.0", "input": "plant.imu", "src": "gyro.1"},
                {"dst": "trim.a", "input": "nav.est", "src": "p.2.3"},
            ],
        )

    def test_calibrated_mappings(self) -> None:
        est = self.nav.est
        linear = Cal.linear(gain=2.0, offset=1.0)
        ratio = Cal.ratiometric(excitation=est.field("gains.1"), span=(0, 1), eng=(0, 10))
        cmd = MotorCmd(
            torque={0: Calibrated(est.field("bias.a"), linear), 1: est.field("gains.0")},
            **{"trim.a": Calibrated(est.field("gains.0"), ratio)},
        )
        self.target.add("mix", Mux(cmd=cmd))
        params = entry(self.target, "mix")["params"]
        self.assertEqual(params["inputs"], ["nav.est"])
        self.assertEqual(
            params["outputs"][0]["fields"],
            [
                {"dst": "torque.0", "input": "nav.est", "src": "bias.a", "cal": {"poly": [1.0, 2.0]}},
                {"dst": "torque.1", "input": "nav.est", "src": "gains.0"},
                {
                    "dst": "trim.a",
                    "input": "nav.est",
                    "src": "gains.0",
                    "cal": {
                        "ratiometric": {
                            "excitation": "gains.1",
                            "span": [0.0, 1.0],
                            "eng": [0.0, 10.0],
                        }
                    },
                },
            ],
        )

    def test_calibrated_source_is_an_input(self) -> None:
        cal = Calibrated(self.plant.imu.field("gyro")[2], Cal.thermocouple("J"))
        cmd = MotorCmd(torque=self.nav.est.field("bias.b"), **{"trim.a": cal})
        self.target.add("mix", Mux(cmd=cmd, on="any"))
        params = entry(self.target, "mix")["params"]
        self.assertEqual(params["inputs"], ["nav.est", "plant.imu"])
        trim = params["outputs"][0]["fields"][1]
        self.assertEqual(trim["src"], "gyro.2")
        self.assertEqual(trim["cal"], {"thermocouple": {"kind": "j"}})

    def test_calibrated_refusals(self) -> None:
        est, gyro = self.nav.est, self.plant.imu.field("gyro")
        linear = Cal.linear(gain=1.0, offset=0.0)
        other = Cal.ratiometric(excitation=gyro[0], span=(0, 1), eng=(0, 1))
        cases: list[tuple[str, Any]] = [
            ("f64 to f64", lambda: MotorCmd(torque=Calibrated(est.field("bias.a"), linear))),
            ("f64 to f64", lambda: MotorCmd(**{"trim.a": Calibrated(gyro, linear)})),
            ("f64 to f64", lambda: MotorCmd(trim=Calibrated(est.field("bias.a"), linear))),
            ("not on `nav.est`", lambda: MotorCmd(**{"trim.a": Calibrated(est.field("bias.a"), other)})),
            ("calibrates a port field", lambda: MotorCmd(**{"trim.a": Calibrated(1.0, linear)})),  # pyright: ignore[reportArgumentType]
            ("takes a `Cal`", lambda: MotorCmd(**{"trim.a": Calibrated(est.field("bias.a"), "k")})),  # pyright: ignore[reportArgumentType]
            ("has no field", lambda: MotorCmd(trim_x=Calibrated(est.field("bias.a"), linear))),
            ("maps", lambda: MotorCmd(torque={0: Calibrated(gyro[0], linear), 1: gyro[1]}, **{"torque.0": gyro[2]})),
        ]
        for message, make in cases:
            with self.subTest(message), self.assertRaisesRegex(ConfigError, message):
                make()

    def test_port_on_another_target(self) -> None:
        other = Target(10.0)
        plant = other.add("plant", Plant())
        cmd = MotorCmd(torque=plant.imu.field("gyro"))
        with self.assertRaisesRegex(ConfigError, "another target"):
            self.target.add("mix", Mux(cmd=cmd))


if __name__ == "__main__":
    unittest.main()
