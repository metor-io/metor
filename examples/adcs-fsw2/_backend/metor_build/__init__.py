"""In-tree PEP 517/660 build backend: pack stubs as venv-only artifacts.

``uv sync`` (or ``pip install -e .``) lands here. ``build_editable`` runs
``metor-fsw stubgen`` into the mission's build directory (``.metor/packs``)
and installs a wheel whose only payload is a ``.pth`` exposing that directory
— plus any ``extra-pth`` roots, e.g. the ``metor_config`` checkout — to the
venv. The interpreter and pyright then resolve ``packs.*`` from freshly
generated stubs, and ``[tool.uv] cache-keys`` re-runs this build whenever the
pack sources change. Nothing generated is checked in; the manifest hash
recorded in each stub remains the staleness gate `resolve` enforces at load.

Editable-only by design: a non-editable wheel of a mission has no consumer
yet, so ``build_wheel``/``build_sdist`` refuse loudly.

Configuration, all from ``pyproject.toml``:

- ``[project]`` ``name``/``version`` — the installed distribution.
- ``[tool.metor.build] out-dir`` — the stub build dir, default ``.metor``.
- ``[tool.metor.build] extra-pth`` — extra roots (project-relative) to expose
  on the venv path.
- ``[tool.metor.build] stubgen-command`` — full override of the stubgen
  invocation. The default builds it from the cargo workspace
  (``cargo run -p metor-fsw-2 --bin metor-fsw -- stubgen …``); setting
  ``METOR_FSW_BIN`` to a prebuilt binary skips that compile.
"""

import os
import shutil
import subprocess
import tomllib

from . import _wheel

# <root>/_backend/metor_build/__init__.py — derive the project root from this
# file's location rather than trusting the hook's working directory.
_ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


def _config():
    """The (name, version, out_dir, extra_pth, stubgen_cmd) the hooks run on."""
    with open(os.path.join(_ROOT, "pyproject.toml"), "rb") as f:
        doc = tomllib.load(f)
    project = doc["project"]
    build = doc.get("tool", {}).get("metor", {}).get("build", {})
    out_dir = os.path.join(_ROOT, build.get("out-dir", ".metor"))
    extra = [os.path.normpath(os.path.join(_ROOT, p)) for p in build.get("extra-pth", [])]
    return project["name"], project["version"], out_dir, extra, build.get("stubgen-command")


def _run_stubgen(out_dir, override):
    packs = os.path.join(out_dir, "packs")
    # Regenerate from scratch: stubgen reconciles the modules it renders but
    # never removes one whose artifact left the pyproject.
    shutil.rmtree(packs, ignore_errors=True)
    if override is not None:
        cmd = list(override)
    elif fsw := os.environ.get("METOR_FSW_BIN"):
        cmd = [fsw, "stubgen", _ROOT, "--out-dir", packs]
    else:
        cmd = ["cargo", "run", "-q", "-p", "metor-fsw-2", "--bin", "metor-fsw", "--",
               "stubgen", _ROOT, "--out-dir", packs]
    # Output passes through to the frontend, which shows it on failure (or -v).
    subprocess.run(cmd, cwd=_ROOT, check=True)


def build_editable(wheel_directory, config_settings=None, metadata_directory=None):
    name, version, out_dir, extra, override = _config()
    _run_stubgen(out_dir, override)
    pth = "\n".join([out_dir, *extra]) + "\n"
    return _wheel.write_editable_wheel(wheel_directory, name, version, pth)


def prepare_metadata_for_build_editable(metadata_directory, config_settings=None):
    name, version, _, _, _ = _config()
    return _wheel.write_metadata(metadata_directory, name, version)


def get_requires_for_build_editable(config_settings=None):
    return []


def get_requires_for_build_wheel(config_settings=None):
    return []


def get_requires_for_build_sdist(config_settings=None):
    return []


def build_wheel(wheel_directory, config_settings=None, metadata_directory=None):
    raise NotImplementedError(
        "the metor prototype backend is editable-only; use `uv sync` or `pip install -e .`"
    )


def build_sdist(sdist_directory, config_settings=None):
    raise NotImplementedError(
        "the metor prototype backend is editable-only; use `uv sync` or `pip install -e .`"
    )
