"""Tests how `metor-build` finds the cargo workspace root."""

from __future__ import annotations

import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from metor_build import _cargo_root  # noqa: E402


class CargoRootTest(unittest.TestCase):
    def setUp(self) -> None:
        self._cwd = os.getcwd()
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name).resolve()

    def tearDown(self) -> None:
        os.chdir(self._cwd)
        self._dir.cleanup()

    def test_finds_workspace_above(self) -> None:
        (self.root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
        pack = self.root / "packs" / "echo"
        pack.mkdir(parents=True)
        (pack / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
        os.chdir(pack)
        self.assertEqual(_cargo_root(), self.root)

    def test_no_workspace_raises(self) -> None:
        (self.root / "Cargo.toml").write_text("[package]\n", encoding="utf-8")
        os.chdir(self.root)
        with self.assertRaises(RuntimeError):
            _cargo_root()


if __name__ == "__main__":
    unittest.main()
