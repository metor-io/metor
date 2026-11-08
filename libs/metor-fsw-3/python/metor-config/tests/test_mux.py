"""Tests for the `Mux` builtin's config."""

from __future__ import annotations

import unittest
from typing import Any

from adcs_stub import PACK, ModeCmd, MotorCmd, Nav, Plant

from metor_config import ConfigError, Mux, Target


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

    def test_port_on_another_target(self) -> None:
        other = Target(10.0)
        plant = other.add("plant", Plant())
        cmd = MotorCmd(torque=plant.imu.field("gyro"))
        with self.assertRaisesRegex(ConfigError, "another target"):
            self.target.add("mix", Mux(cmd=cmd))


if __name__ == "__main__":
    unittest.main()
