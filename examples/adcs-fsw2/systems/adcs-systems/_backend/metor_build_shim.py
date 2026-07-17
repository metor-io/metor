"""Monorepo shim for the `metor_build` backend.

PEP 517's ``backend-path`` must stay inside the source tree (the pack
crate), so this one-file backend puts the in-repo ``metor_build`` package on
the path at import time and re-exports its hooks. A published `metor-build`
distribution replaces it (`docs/design-packaging.md` phase 0): drop this
file and the ``backend-path`` line, and set ``requires = ["metor-build"]``.
"""

import pathlib
import sys

sys.path.insert(
    0,
    str(
        pathlib.Path(__file__).resolve().parents[5]
        / "libs"
        / "metor-fsw-2"
        / "python"
        / "metor-build"
    ),
)

from metor_build import *  # noqa: E402,F401,F403
