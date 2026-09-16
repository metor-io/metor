"""The few lines of zip a `.pth`-only editable wheel needs."""

from __future__ import annotations

import base64
import hashlib
import zipfile
from pathlib import Path

TAG = "py3-none-any"


def write_editable(
    wheel_directory: Path,
    dist: str,
    version: str,
    abi_version: str,
    path: Path,
) -> str:
    """Write `<dist>-<version>-py3-none-any.whl` holding one `.pth` line for `path`.

    Returns the wheel's file name, as PEP 660 asks of `build_editable`.
    """
    name = dist.replace("-", "_")
    info = f"{name}-{version}.dist-info"
    files = {
        f"{name}_editable.pth": f"{path}\n",
        f"{info}/METADATA": _metadata(dist, version, abi_version),
        f"{info}/WHEEL": _wheel(),
    }
    record = "".join(_record_line(member, text) for member, text in files.items())
    files[f"{info}/RECORD"] = record + f"{info}/RECORD,,\n"

    wheel_directory.mkdir(parents=True, exist_ok=True)
    filename = f"{name}-{version}-{TAG}.whl"
    with zipfile.ZipFile(wheel_directory / filename, "w", zipfile.ZIP_DEFLATED) as archive:
        for member, text in files.items():
            archive.writestr(member, text)
    return filename


def _metadata(dist: str, version: str, abi_version: str) -> str:
    return (
        "Metadata-Version: 2.1\n"
        f"Name: {dist}\n"
        f"Version: {version}\n"
        f"Requires-Dist: metor-fsw-abi=={abi_version}\n"
        "Requires-Dist: metor-config\n"
    )


def _wheel() -> str:
    return (
        "Wheel-Version: 1.0\n"
        "Generator: metor-build\n"
        "Root-Is-Purelib: true\n"
        f"Tag: {TAG}\n"
    )


def _record_line(member: str, text: str) -> str:
    """One RECORD row: the member, its urlsafe base64 sha256, and its size."""
    raw = text.encode("utf-8")
    digest = base64.urlsafe_b64encode(hashlib.sha256(raw).digest()).rstrip(b"=").decode("ascii")
    return f"{member},sha256={digest},{len(raw)}\n"
