"""PEP 517/660 build backend for metor pack crates.

``uv sync`` of a mission with a path-source pack dependency lands here.
``build_editable`` runs ``metor-fsw pack dev`` in the pack crate — building
the host triple and laying out ``.metor/<module>/`` (the typed module plus
its ``_libs`` payload) — and installs a wheel whose only payload is a
``.pth`` naming ``.metor``, so the interpreter and pyright resolve the pack
module from the venv. ``[tool.uv] cache-keys`` in the pack's ``pyproject``
re-runs this build whenever the pack sources change.

Editable-only this phase: the non-editable ``build_wheel`` (the published
fat wheel) lands with the phase-2 publish pipeline
(``docs/design-packaging.md`` §7).

Configuration, all from the pack's ``pyproject.toml``:

- ``[project]`` ``name``/``version`` — the installed distribution.
- ``[tool.metor.build] extra-pth`` — extra roots (pack-relative) to expose
  on the venv path.
- ``[tool.metor.build] pack-dev-command`` — full override of the layout
  invocation. The default resolves ``metor-fsw`` from ``$METOR_FSW_BIN``,
  then an installed ``metor-fsw`` dist (``metor_fsw.find()`` — present
  exactly when the pack's ``build-system.requires`` names it, the
  resolver-pinned path), then ``PATH``, then a ``cargo run`` of the
  workspace (monorepo development).
"""

import os
import shutil
import subprocess
import tomllib

from . import _wheel


def _config(root):
    """The (name, version, extra_pth, pack_dev_cmd) the hooks run on."""
    with open(os.path.join(root, "pyproject.toml"), "rb") as f:
        doc = tomllib.load(f)
    project = doc["project"]
    build = doc.get("tool", {}).get("metor", {}).get("build", {})
    extra = [os.path.normpath(os.path.join(root, p)) for p in build.get("extra-pth", [])]
    return project["name"], project["version"], extra, build.get("pack-dev-command")


def _supports_pack(fsw):
    """Whether a `metor-fsw` binary knows the `pack` subcommand — the
    capability handshake that keeps a stale binary on `PATH` from serving a
    build it cannot perform."""
    try:
        probe = subprocess.run(
            [fsw, "pack", "--help"], capture_output=True, timeout=30
        )
    except OSError:
        return False
    return probe.returncode == 0


def _packaged_fsw():
    """The `metor-fsw` binary of an installed `metor-fsw` dist, present
    exactly when the pack declared it in `build-system.requires` and the
    frontend installed it into this (isolated) build env — the
    resolver-pinned path, immune to whatever is on `PATH`."""
    try:
        from metor_fsw import find
    except ImportError:
        return None
    return find()


def _run_pack_dev(root, override):
    if override is not None:
        cmd = list(override)
    elif fsw := os.environ.get("METOR_FSW_BIN"):
        # An explicit override that cannot serve is an error, not a fallback.
        if not _supports_pack(fsw):
            raise RuntimeError(
                f"$METOR_FSW_BIN ({fsw}) does not support `pack dev`; "
                "point it at a newer metor-fsw"
            )
        cmd = [fsw, "pack", "dev", root]
    elif fsw := _packaged_fsw():
        cmd = [fsw, "pack", "dev", root]
    elif (fsw := shutil.which("metor-fsw")) and _supports_pack(fsw):
        cmd = [fsw, "pack", "dev", root]
    else:
        cmd = ["cargo", "run", "-q", "-p", "metor-fsw-2", "--bin", "metor-fsw", "--",
               "pack", "dev", root]
    # Output passes through to the frontend, which shows it on failure (or -v).
    subprocess.run(cmd, cwd=root, check=True)


def build_editable(wheel_directory, config_settings=None, metadata_directory=None):
    # PEP 517 runs hooks with the source tree (the pack crate) as cwd.
    root = os.getcwd()
    name, version, extra, override = _config(root)
    _run_pack_dev(root, override)
    pth = "\n".join([os.path.join(root, ".metor"), *extra]) + "\n"
    return _wheel.write_editable_wheel(wheel_directory, name, version, pth)


def prepare_metadata_for_build_editable(metadata_directory, config_settings=None):
    name, version, _, _ = _config(os.getcwd())
    return _wheel.write_metadata(metadata_directory, name, version)


def get_requires_for_build_editable(config_settings=None):
    return []


def get_requires_for_build_wheel(config_settings=None):
    return []


def get_requires_for_build_sdist(config_settings=None):
    return []


def build_wheel(wheel_directory, config_settings=None, metadata_directory=None):
    raise NotImplementedError(
        "metor-build is editable-only until the phase-2 publish pipeline; "
        "use `uv sync` or `pip install -e .`"
    )


def build_sdist(sdist_directory, config_settings=None):
    raise NotImplementedError(
        "metor packs are published as binary wheels, never sdists "
        "(docs/design-packaging.md §7)"
    )
