"""PEP 517 backend for the `metor-fsw` binary distribution.

`build_wheel` cargo-builds the release binary and packages it with the
`metor_fsw` locator module into a platform-tagged wheel. In-tree because
`backend-path` cannot leave the source tree; the wheel-writing machinery is
`metor_build._wheel`, reached through a sibling-path insert.

Editable installs are refused: a path-source binary wheel would go stale
against the checkout (its cache keys cannot see workspace-wide Rust
changes); monorepo development uses the cargo fallback in `metor_build`
instead, and this backend exists for `uv build` + publishing.
"""

import json
import os
import pathlib
import subprocess
import sys
import sysconfig
import tomllib

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[2] / "metor-build"))

from metor_build import _wheel  # noqa: E402


def _project(root):
    with open(os.path.join(root, "pyproject.toml"), "rb") as f:
        doc = tomllib.load(f)
    return doc["project"]


def _platform_tag() -> str:
    """The wheel platform tag for this host, `sysconfig` convention
    (`macosx_15_0_arm64`, `linux_x86_64`). The tag does not claim manylinux
    compliance."""
    return sysconfig.get_platform().replace("-", "_").replace(".", "_")


def _build_binary(root) -> bytes:
    """Cargo-build the release binary and return its bytes."""
    out = subprocess.run(
        [
            "cargo",
            "build",
            "--release",
            "-p",
            "metor-fsw-2",
            "--bin",
            "metor-fsw",
            "--message-format=json",
        ],
        cwd=root,
        check=True,
        stdout=subprocess.PIPE,
    )
    exe = None
    for line in out.stdout.splitlines():
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("reason") == "compiler-artifact" and msg.get("executable"):
            exe = msg["executable"]
    if exe is None:
        raise RuntimeError("cargo reported no executable for metor-fsw")
    with open(exe, "rb") as f:
        return f.read()


def build_wheel(wheel_directory, config_settings=None, metadata_directory=None):
    root = os.getcwd()
    project = _project(root)
    module = pathlib.Path(root) / "metor_fsw" / "__init__.py"
    exe = "metor-fsw.exe" if os.name == "nt" else "metor-fsw"
    files = [
        ("metor_fsw/__init__.py", module.read_bytes(), 0o644),
        (f"metor_fsw/bin/{exe}", _build_binary(root), 0o755),
    ]
    return _wheel.write_wheel(
        wheel_directory,
        project["name"],
        project["version"],
        files,
        tag=f"py3-none-{_platform_tag()}",
        requires=project.get("dependencies", []),
        requires_python=project.get("requires-python"),
        entry_points="[console_scripts]\nmetor-fsw = metor_fsw:main\n",
    )


def prepare_metadata_for_build_wheel(metadata_directory, config_settings=None):
    project = _project(os.getcwd())
    return _wheel.write_metadata(
        metadata_directory, project["name"], project["version"]
    )


def get_requires_for_build_wheel(config_settings=None):
    return []


def get_requires_for_build_sdist(config_settings=None):
    return []


def build_editable(wheel_directory, config_settings=None, metadata_directory=None):
    raise NotImplementedError(
        "metor-fsw has no editable form: a path-source binary wheel goes stale "
        "against the checkout; monorepo development uses metor_build's cargo fallback"
    )


def build_sdist(sdist_directory, config_settings=None):
    raise NotImplementedError("metor-fsw ships as binary wheels only")
