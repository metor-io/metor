"""Build a pack as an editable install.

`build_editable` runs `metor pack dev` over the pack's directory and writes a
wheel whose one `.pth` line puts the generated `<root>/.metor` on the path.
A packed wheel is a later slice.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path
from typing import Any

from ._wheel import write_editable

__all__ = [
    "build_editable",
    "build_sdist",
    "build_wheel",
    "get_requires_for_build_editable",
]


def get_requires_for_build_editable(config_settings: dict[str, Any] | None = None) -> list[str]:
    return []


def build_editable(
    wheel_directory: str,
    config_settings: dict[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    """Run `metor pack dev` over this directory and write the `.pth` wheel."""
    root = Path.cwd().resolve()
    _run_metor(["pack", "dev", str(root)])
    abi_version = _run_metor(["abi-version"], capture=True).strip()
    project = _project(root)
    return write_editable(
        Path(wheel_directory),
        dist=project["name"],
        version=project["version"],
        abi_version=abi_version,
        path=root / ".metor",
    )


def build_wheel(
    wheel_directory: str,
    config_settings: dict[str, Any] | None = None,
    metadata_directory: str | None = None,
) -> str:
    raise NotImplementedError("a pack ships its cdylib through `metor pack build`, a later slice")


def build_sdist(sdist_directory: str, config_settings: dict[str, Any] | None = None) -> str:
    raise NotImplementedError("a pack's source is its cargo crate, which pip cannot build")


def _project(root: Path) -> dict[str, str]:
    with open(root / "pyproject.toml", "rb") as file:
        project = tomllib.load(file)["project"]
    return {"name": project["name"], "version": project.get("version", "0.0.0")}


def _run_metor(args: list[str], capture: bool = False) -> str:
    """Run the `metor` binary: `$METOR_BIN`, else `PATH`, else `cargo run`."""
    command = _binary() + args
    result = subprocess.run(
        command,
        cwd=_cargo_root() if command[0] == "cargo" else None,
        stdout=subprocess.PIPE if capture else None,
        stderr=None,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"`{' '.join(command)}` failed with status {result.returncode}")
    return result.stdout if capture else ""


def _binary() -> list[str]:
    from_env = os.environ.get("METOR_BIN")
    if from_env:
        return [from_env]
    found = shutil.which("metor")
    if found:
        return [found]
    return ["cargo", "run", "-q", "-p", "metor-fsw-3", "--bin", "metor", "--"]


def _cargo_root() -> Path:
    """The workspace root, for the `cargo run` fallback in an in-repo checkout."""
    for directory in [Path.cwd().resolve(), *Path.cwd().resolve().parents]:
        manifest = directory / "Cargo.toml"
        if manifest.is_file() and "[workspace]" in manifest.read_text(encoding="utf-8"):
            return directory
    print("metor-build: no cargo workspace above this pack", file=sys.stderr)
    return Path.cwd()
