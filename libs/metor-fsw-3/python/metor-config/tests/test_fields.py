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


class ElementsTest(unittest.TestCase):
    def setUp(self) -> None:
        target = Target(10.0)
        self.plant = target.add("plant", Plant())
        self.nav = target.add("nav", Nav(imu=self.plant.imu))
        self.gyro = self.plant.imu.field("gyro")
        self.p = self.nav.est.field("p")

    def test_element_of_a_shaped_leaf(self) -> None:
        element = self.gyro[1]
        self.assertEqual((element.path, element.signature), ("gyro.1", "f64"))
        by_path = self.plant.imu.field("gyro.1")
        self.assertEqual((by_path.path, by_path.signature), ("gyro.1", "f64"))

    def test_element_maps_from_a_scalar(self) -> None:
        cmd = MotorCmd(**{"torque.0": self.nav.est.field("bias.a")})
        self.assertEqual(list(cmd._mapping), ["torque.0"])

    def test_mapping_expands_to_paths(self) -> None:
        cmd = MotorCmd(torque={0: self.gyro[2], 1: self.gyro[0]})
        self.assertEqual(list(cmd._mapping), ["torque.0", "torque.1"])
        self.assertEqual(cmd._mapping["torque.0"].path, "gyro.2")

    def test_last_index(self) -> None:
        self.assertEqual(self.plant.imu.field("css")[5].path, "css.5")
        MotorCmd(torque={2: self.gyro[2]})

    def test_two_dimensional(self) -> None:
        self.assertEqual(self.p[1].signature, "f64[6]")
        element = self.p[1][2]
        self.assertEqual((element.path, element.signature), ("p.1.2", "f64"))
        Est(**{"bias.a": element})

    def test_split_array(self) -> None:
        gains = self.nav.est.field("gains")
        self.assertEqual((gains[1].path, gains[1].signature), ("gains.1", "f64"))
        Est(gains={0: self.gyro[0]}, **{"gains.1": gains[0]})

    def test_index_past_the_dimension(self) -> None:
        with self.assertRaisesRegex(ConfigError, "index 3 is past dimension 3"):
            self.gyro[3]
        with self.assertRaisesRegex(ConfigError, "index 6 is past dimension 6"):
            self.nav.est.field("p.0.6")
        with self.assertRaisesRegex(ConfigError, "index 3 is past dimension 3"):
            MotorCmd(torque={3: self.gyro[0]})
        with self.assertRaisesRegex(ConfigError, "no field `gains.2`"):
            self.nav.est.field("gains")[2]

    def test_bad_indices(self) -> None:
        for bad in (-1, True, "1", 1.0):
            with self.assertRaisesRegex(ConfigError, "non-negative int index"):
                self.gyro[bad]  # pyright: ignore[reportArgumentType]
        with self.assertRaisesRegex(ConfigError, "non-negative int index"):
            MotorCmd(torque={-1: self.gyro[0]})

    def test_non_canonical_paths(self) -> None:
        for path in ("gyro.01", "gyro.+1", "gyro.x", "gyro.0.0", "gyro."):
            with self.assertRaisesRegex(ConfigError, "no field"):
                self.plant.imu.field(path)

    def test_indexing_a_scalar_or_a_group(self) -> None:
        with self.assertRaisesRegex(ConfigError, "no field `bias.a.0`"):
            self.nav.est.field("bias.a")[0]
        with self.assertRaisesRegex(ConfigError, "no field `bias.0`"):
            self.nav.est.field("bias")[0]

    def test_field_is_not_iterable(self) -> None:
        with self.assertRaises(TypeError):
            list(self.gyro)  # pyright: ignore[reportArgumentType]

    def test_leaf_and_its_element_overlap(self) -> None:
        with self.assertRaisesRegex(ConfigError, "maps `torque` and `torque.1`"):
            MotorCmd(torque=self.gyro, **{"torque.1": self.gyro[0]})

    def test_element_mapped_twice(self) -> None:
        with self.assertRaisesRegex(ConfigError, "maps `torque.1` and `torque.1`"):
            MotorCmd(torque={1: self.gyro[0]}, **{"torque.1": self.gyro[2]})


if __name__ == "__main__":
    unittest.main()
