"""Minimal wheel writer (stdlib only).

Writes editable wheels (one ``.pth`` payload) and real wheels (arbitrary
files with modes, entry points, dependency metadata, a platform tag) by
hand, so ``build-system.requires`` stays empty and builds work offline.
"""

import base64
import csv
import hashlib
import io
import os
import re
import zipfile

_GENERATOR = "metor-build 0.1"


def _dist(name: str) -> str:
    """The distribution name as it appears in file names (PEP 427/503)."""
    return re.sub(r"[-_.]+", "_", name).lower()


def metadata_contents(
    name: str,
    version: str,
    requires: list[str] | None = None,
    requires_python: str | None = None,
) -> str:
    lines = [f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"]
    if requires_python:
        lines.append(f"Requires-Python: {requires_python}\n")
    for req in requires or []:
        lines.append(f"Requires-Dist: {req}\n")
    return "".join(lines)


def write_metadata(metadata_directory: str, name: str, version: str) -> str:
    """Write ``<dist>-<ver>.dist-info/METADATA``; returns the dist-info name."""
    dist_info = f"{_dist(name)}-{version}.dist-info"
    path = os.path.join(metadata_directory, dist_info)
    os.makedirs(path, exist_ok=True)
    with open(os.path.join(path, "METADATA"), "w", encoding="utf-8") as f:
        f.write(metadata_contents(name, version))
    return dist_info


def write_wheel(
    wheel_directory: str,
    name: str,
    version: str,
    files: list[tuple[str, bytes, int]],
    *,
    tag: str = "py3-none-any",
    requires: list[str] | None = None,
    requires_python: str | None = None,
    entry_points: str | None = None,
) -> str:
    """Write ``<dist>-<ver>-<tag>.whl`` from ``(arcname, data, mode)`` files.

    Modes ride the zip entries' external attributes, which installers
    preserve — how a packaged binary stays executable. Returns the wheel
    file name.
    """
    dist = _dist(name)
    dist_info = f"{dist}-{version}.dist-info"
    records: list[tuple[str, str, str]] = []
    payload: list[tuple[str, bytes, int]] = []

    def entry(path: str, data: bytes, mode: int = 0o644) -> None:
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
        records.append((path, f"sha256={digest.decode()}", str(len(data))))
        payload.append((path, data, mode))

    for path, data, mode in files:
        entry(path, data, mode)
    entry(
        f"{dist_info}/METADATA",
        metadata_contents(name, version, requires, requires_python).encode(),
    )
    entry(
        f"{dist_info}/WHEEL",
        (
            "Wheel-Version: 1.0\n"
            f"Generator: {_GENERATOR}\n"
            f"Root-Is-Purelib: {'true' if tag.endswith('-any') else 'false'}\n"
            f"Tag: {tag}\n"
        ).encode(),
    )
    if entry_points is not None:
        entry(f"{dist_info}/entry_points.txt", entry_points.encode())

    record = io.StringIO()
    writer = csv.writer(record, lineterminator="\n")
    writer.writerows(records)
    writer.writerow((f"{dist_info}/RECORD", "", ""))
    payload.append((f"{dist_info}/RECORD", record.getvalue().encode(), 0o644))

    wheel_name = f"{dist}-{version}-{tag}.whl"
    with zipfile.ZipFile(
        os.path.join(wheel_directory, wheel_name), "w", zipfile.ZIP_DEFLATED
    ) as zf:
        for path, data, mode in payload:
            info = zipfile.ZipInfo(path)
            info.external_attr = (0o100000 | mode) << 16
            info.compress_type = zipfile.ZIP_DEFLATED
            zf.writestr(info, data)
    return wheel_name


def write_editable_wheel(wheel_directory: str, name: str, version: str, pth_content: str) -> str:
    """Write ``<dist>-<ver>-py3-none-any.whl`` whose payload is one ``.pth``.

    The ``.pth`` carries absolute path lines, which both the interpreter and
    pyright follow — no import hooks. Returns the wheel file name.
    """
    return write_wheel(
        wheel_directory,
        name,
        version,
        [(f"_{_dist(name)}_editable.pth", pth_content.encode(), 0o644)],
    )
