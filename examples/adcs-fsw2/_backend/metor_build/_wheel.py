"""Minimal editable-wheel writer (stdlib only).

The backend's whole payload is one ``.pth`` of absolute path lines plus the
required ``dist-info``, so the archive is written by hand rather than pulling
in a packaging dependency: ``build-system.requires`` stays empty and builds
work offline.
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


def metadata_contents(name: str, version: str) -> str:
    return f"Metadata-Version: 2.1\nName: {name}\nVersion: {version}\n"


def write_metadata(metadata_directory: str, name: str, version: str) -> str:
    """Write ``<dist>-<ver>.dist-info/METADATA``; returns the dist-info name."""
    dist_info = f"{_dist(name)}-{version}.dist-info"
    path = os.path.join(metadata_directory, dist_info)
    os.makedirs(path, exist_ok=True)
    with open(os.path.join(path, "METADATA"), "w", encoding="utf-8") as f:
        f.write(metadata_contents(name, version))
    return dist_info


def write_editable_wheel(wheel_directory: str, name: str, version: str, pth_content: str) -> str:
    """Write ``<dist>-<ver>-py3-none-any.whl`` whose payload is one ``.pth``.

    The ``.pth`` carries absolute path lines, which both the interpreter and
    pyright follow — no import hooks. Returns the wheel file name.
    """
    dist = _dist(name)
    dist_info = f"{dist}-{version}.dist-info"
    records: list[tuple[str, str, str]] = []

    def entry(path: str, data: bytes) -> tuple[str, bytes]:
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
        records.append((path, f"sha256={digest.decode()}", str(len(data))))
        return path, data

    wheel_file = (
        "Wheel-Version: 1.0\n"
        f"Generator: {_GENERATOR}\n"
        "Root-Is-Purelib: true\n"
        "Tag: py3-none-any\n"
    )
    files = [
        entry(f"_{dist}_editable.pth", pth_content.encode()),
        entry(f"{dist_info}/METADATA", metadata_contents(name, version).encode()),
        entry(f"{dist_info}/WHEEL", wheel_file.encode()),
    ]
    record = io.StringIO()
    writer = csv.writer(record, lineterminator="\n")
    writer.writerows(records)
    writer.writerow((f"{dist_info}/RECORD", "", ""))
    files.append((f"{dist_info}/RECORD", record.getvalue().encode()))

    wheel_name = f"{dist}-{version}-py3-none-any.whl"
    with zipfile.ZipFile(
        os.path.join(wheel_directory, wheel_name), "w", zipfile.ZIP_DEFLATED
    ) as zf:
        for path, data in files:
            zf.writestr(path, data)
    return wheel_name
