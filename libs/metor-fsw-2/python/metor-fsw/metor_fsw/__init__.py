"""The packaged `metor-fsw` binary and its locator.

The wheel ships the real CLI as package data (`metor_fsw/bin/metor-fsw`),
so consumers resolve it through Python's import machinery — pinned by the
resolver, immune to stale copies on `PATH` (`docs/packaging.md`).
`find()` is what tooling (the `metor-build` backend) calls; `main()` backs
the `metor-fsw` console script for humans.
"""

import os
import subprocess
import sys
from importlib import resources


def find() -> str:
    """The absolute path of the packaged `metor-fsw` binary."""
    exe = "metor-fsw.exe" if os.name == "nt" else "metor-fsw"
    return str(resources.files(__package__).joinpath("bin", exe))


def main() -> None:
    """Console-script entry: hand the process over to the binary."""
    argv = [find(), *sys.argv[1:]]
    if os.name == "nt":
        sys.exit(subprocess.run(argv).returncode)
    os.execv(argv[0], argv)
