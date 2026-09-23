"""Several targets in one file, emitted as one envelope."""

from __future__ import annotations

import atexit
import json
import os
import sys
from typing import Any, Mapping, Sequence

from ._config import CONFIG_VERSION, OUT_ENV, ConfigError
from ._target import Target, _targets

_deployments: list["Deployment"] = []
_emitted = False


class Deployment:
    """The targets one file deploys together, and where peers dial each.

    ``hosts`` maps a target's namespace to its address; a target absent from
    it is at loopback. A target is a member by identity, and every target the
    file built must be one.
    """

    def __init__(self, targets: Sequence[Target], hosts: Mapping[str, str] | None = None) -> None:
        self.targets = list(targets)
        self.hosts = dict(hosts or {})
        _deployments.append(self)

    def to_config(self) -> dict[str, Any]:
        """The envelope: every member's config under its namespace."""
        names = _namespaces(self.targets)
        for host in self.hosts:
            if host not in names:
                raise ConfigError(f"hosts names `{host}`, which is not a target")
        return _envelope(self.targets, self.hosts)


def _namespaces(targets: Sequence[Target]) -> list[str]:
    """Every member's namespace; several members each need one of their own."""
    if len(targets) == 1:
        return [targets[0].namespace or ""]
    names: list[str] = []
    for target in targets:
        if not target.namespace:
            raise ConfigError("every target of a deployment names a namespace")
        if target.namespace in names:
            raise ConfigError(f"two targets are namespaced `{target.namespace}`")
        names.append(target.namespace)
    return names


def _envelope(targets: Sequence[Target], hosts: Mapping[str, str]) -> dict[str, Any]:
    return {
        "config_version": CONFIG_VERSION,
        "hosts": dict(hosts),
        "targets": [target.to_config() for target in targets],
    }


def to_config() -> dict[str, Any]:
    """What this file emits: its one `Deployment`, else its one `Target` as a
    deployment of one."""
    if len(_deployments) > 1:
        raise ConfigError(f"a file builds at most one Deployment, found {len(_deployments)}")
    if _deployments:
        deployment = _deployments[0]
        left_out = [t for t in _targets if not any(t is member for member in deployment.targets)]
        if left_out:
            raise ConfigError(f"the Deployment leaves out {len(left_out)} target(s) this file built")
        return deployment.to_config()
    if len(_targets) != 1:
        raise ConfigError(
            f"a file without a Deployment builds exactly one Target, found {len(_targets)}"
        )
    return _envelope(_targets, {})


def emit(path: str | None = None) -> None:
    """Write this file's config to ``path``, else ``$METOR_CONFIG_OUT``, else
    stdout."""
    global _emitted
    text = json.dumps(to_config(), indent=2, sort_keys=False) + "\n"
    out = path or os.environ.get(OUT_ENV)
    if out:
        with open(out, "w", encoding="utf-8") as file:
            file.write(text)
    else:
        sys.stdout.write(text)
    _emitted = True


def _emit_at_exit() -> None:
    # A target file is a script that ends; a file that raised has already
    # printed its traceback.
    if not _emitted and (_targets or _deployments):
        emit()


atexit.register(_emit_at_exit)
