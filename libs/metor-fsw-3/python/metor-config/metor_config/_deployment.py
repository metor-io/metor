"""`Deployment`, which groups several targets into one config file."""

from __future__ import annotations

import atexit
import json
import os
import sys
from typing import Any, Mapping, Sequence, TypeVar

from ._config import OUT_ENV, ConfigError
from ._generated import CONFIG_VERSION
from ._group import Group, GroupBuild, child
from ._target import Target, _targets

R = TypeVar("R")

_deployments: list["Deployment"] = []
_emitted = False


class Deployment:
    """A group of targets that are deployed together and talk to each other.

    ``targets`` are targets made outside any group; `add` adds a group's
    targets. ``hosts`` maps a namespace, or a group's path, to the address
    peers dial; the longest match wins. A target that no key matches is
    dialed on loopback. Every `Target` the file creates must be in the
    deployment. Each target in a deployment of more than one needs its own
    namespace, and no namespace may sit under another.
    """

    def __init__(
        self, targets: Sequence[Target] = (), hosts: Mapping[str, str] | None = None
    ) -> None:
        self.targets = list(targets)
        self.hosts = dict(hosts or {})
        self._groups: set[str] = set()
        _deployments.append(self)

    def add(self, name: str, build: GroupBuild[R]) -> R:
        """Adds a group at the top of the tree and returns its result."""
        return self._add_group(child("", name), build)

    def _add_group(self, path: str, build: GroupBuild[R]) -> R:
        if not isinstance(build, GroupBuild):
            raise ConfigError(f"`{path}` takes a group, got {build!r}")
        if path in self._groups:
            raise ConfigError(f"group `{path}` is already added")
        self._groups.add(path)
        return build._build(Group(self, path))

    def to_config(self) -> dict[str, Any]:
        """Returns the deployment config, with one entry per target."""
        names = _namespaces(self.targets)
        return _envelope(self.targets, _hosts(names, self.hosts))


def _namespaces(targets: Sequence[Target]) -> list[str]:
    """Returns every target's namespace.

    A lone target may have no namespace. When there are several, each needs a
    unique one, and none may sit under another.
    """
    if len(targets) == 1:
        return [targets[0].namespace or ""]
    names: list[str] = []
    for target in targets:
        if not target.namespace:
            raise ConfigError("every target of a deployment names a namespace")
        if target.namespace in names:
            raise ConfigError(f"two targets are namespaced `{target.namespace}`")
        names.append(target.namespace)
    for name in names:
        for other in names:
            if name.startswith(f"{other}."):
                raise ConfigError(f"target `{name}` sits under target `{other}`")
    return names


def _hosts(names: Sequence[str], hosts: Mapping[str, str]) -> dict[str, str]:
    """Returns each target's host: the value of the longest key that is its
    namespace or a group above it. Raises `ConfigError` for a key that
    matches no target."""
    for key in hosts:
        if not any(_under(name, key) for name in names):
            raise ConfigError(f"hosts names `{key}`, which matches no target")
    resolved: dict[str, str] = {}
    for name in names:
        keys = [key for key in hosts if _under(name, key)]
        if keys:
            resolved[name] = hosts[max(keys, key=len)]
    return resolved


def _under(name: str, key: str) -> bool:
    return name == key or name.startswith(f"{key}.")


def _envelope(targets: Sequence[Target], hosts: Mapping[str, str]) -> dict[str, Any]:
    return {
        "config_version": CONFIG_VERSION,
        "hosts": dict(hosts),
        "targets": [target.to_config() for target in targets],
    }


def to_config() -> dict[str, Any]:
    """Returns the config this file emits.

    That is the file's `Deployment` if it has one. Otherwise, the file must
    create exactly one `Target`, which is emitted as a deployment of one.
    """
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
    """Writes this file's config.

    The config goes to ``path`` if given, otherwise to ``$METOR_CONFIG_OUT``,
    otherwise to stdout. A target file that does not call this emits at exit.
    """
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
    # A target file is a script that runs to the end, so emit when it exits.
    # A file that raised has already printed its traceback.
    if not _emitted and (_targets or _deployments):
        emit()


atexit.register(_emit_at_exit)
