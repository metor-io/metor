"""The editable wheel's members, digests, and `.pth` line."""

from __future__ import annotations

import base64
import hashlib
import os
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from metor_build._wheel import write_editable  # noqa: E402


def built(root: Path, dist: str = "echo-pack", version: str = "0.1.0") -> zipfile.ZipFile:
    name = write_editable(
        root / "wheels",
        dist=dist,
        version=version,
        abi_version="1",
        path=root / "pack" / ".metor",
    )
    return zipfile.ZipFile(root / "wheels" / name)


class WheelTest(unittest.TestCase):
    def test_wheel_metadata_and_path(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with built(root) as wheel:
                self.assertEqual(
                    sorted(wheel.namelist()),
                    [
                        "echo_pack-0.1.0.dist-info/METADATA",
                        "echo_pack-0.1.0.dist-info/RECORD",
                        "echo_pack-0.1.0.dist-info/WHEEL",
                        "echo_pack_editable.pth",
                    ],
                )
                pth = wheel.read("echo_pack_editable.pth").decode()
                self.assertEqual(pth, f"{root / 'pack' / '.metor'}\n")
                self.assertTrue(Path(pth.strip()).is_absolute())
                metadata = wheel.read("echo_pack-0.1.0.dist-info/METADATA").decode()
                self.assertIn("Requires-Dist: metor-fsw-abi==1", metadata)
                self.assertIn("Requires-Dist: metor-config", metadata)

    def test_wheel_record_digests(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            with built(Path(tmp)) as wheel:
                rows = wheel.read("echo_pack-0.1.0.dist-info/RECORD").decode().splitlines()
                seen = {}
                for row in rows:
                    member, digest, size = row.split(",")
                    seen[member] = (digest, size)
                self.assertEqual(seen["echo_pack-0.1.0.dist-info/RECORD"], ("", ""))
                for member, (digest, size) in seen.items():
                    if not digest:
                        continue
                    raw = wheel.read(member)
                    want = base64.urlsafe_b64encode(hashlib.sha256(raw).digest())
                    self.assertEqual(digest, f"sha256={want.rstrip(b'=').decode()}")
                    self.assertEqual(size, str(len(raw)))
                self.assertEqual(len(seen), len(wheel.namelist()))

    def test_normalize_distribution_name(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with built(root, dist="my-adcs-pack", version="2.1") as wheel:
                self.assertIn("my_adcs_pack_editable.pth", wheel.namelist())
                self.assertIn("my_adcs_pack-2.1.dist-info/WHEEL", wheel.namelist())


if __name__ == "__main__":
    unittest.main()
