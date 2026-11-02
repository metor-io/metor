"""Tests for port fields and record mappings."""

from __future__ import annotations

import unittest

from adcs_stub import Est, ModeCmd, MotorCmd, Nav, Plant

from metor_config import ConfigError, Target


class FieldsTest(unittest.TestCase):
    def setUp(self) -> None:
        target = Target(10.0)
        self.plant = target.add("plant", Plant())
        self.nav = target.add("nav", Nav(imu=self.plant.imu))

    def test_leaf_maps(self) -> None:
        gyro = self.plant.imu.field("gyro")
        MotorCmd(torque=gyro)
        self.assertEqual((gyro.path, gyro.signature), ("gyro", "f64[3]"))

    def test_group_maps_in_any_leaf_order(self) -> None:
        bias = self.nav.est.field("bias")
        self.assertEqual(bias.signature, "{a:f64,b:f64[3]}")
        MotorCmd(trim=bias)

    def test_unknown_destination(self) -> None:
        with self.assertRaisesRegex(ConfigError, "no field `thrust`"):
            MotorCmd(thrust=self.plant.imu.field("gyro"))

    def test_unknown_source(self) -> None:
        with self.assertRaisesRegex(ConfigError, "no field `mag`"):
            self.plant.imu.field("mag")

    def test_prefix_is_not_a_group(self) -> None:
        with self.assertRaises(ConfigError):
            self.nav.est.field("bia")

    def test_shape_mismatch(self) -> None:
        with self.assertRaisesRegex(ConfigError, "`f64\\[3\\]`.*`f64\\[6\\]`"):
            MotorCmd(torque=self.plant.imu.field("css"))

    def test_message_record(self) -> None:
        with self.assertRaisesRegex(ConfigError, "message"):
            ModeCmd()
        with self.assertRaisesRegex(ConfigError, "message"):
            self.plant.log.field("text")

    def test_non_field_value(self) -> None:
        with self.assertRaisesRegex(ConfigError, "takes a port field"):
            Est(q=self.plant.imu)  # pyright: ignore[reportArgumentType]


if __name__ == "__main__":
    unittest.main()
