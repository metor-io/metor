"""`Deployment`, which groups several targets into one config file."""

from __future__ import annotations

import atexit
import json
import os
import sys
from typing import Any, Mapping, Sequence

from ._builtins import Db, Link, Subscribe, _namespace
from ._config import CONFIG_VERSION, OUT_ENV, ConfigError
from ._target import SystemHandle, Target, _targets

LOOPBACK = "127.0.0.1"

_deployments: list["Deployment"] = []
_emitted = False


class Deployment:
    """A group of targets that are deployed together and talk to each other.

    ``hosts`` maps a target's namespace to the address its peers dial. A
    target that is not listed is dialed on loopback. Every `Target` the file
    creates must be in the deployment. Each target in a deployment of more
    than one needs its own namespace.
    """

    def __init__(self, targets: Sequence[Target], hosts: Mapping[str, str] | None = None) -> None:
        self.targets = list(targets)
        self.hosts = dict(hosts or {})
        _deployments.append(self)

    def to_config(self) -> dict[str, Any]:
        """Returns the deployment config, with one entry per target."""
        names = _namespaces(self.targets)
        for host in self.hosts:
            if host not in names:
                raise ConfigError(f"hosts names `{host}`, which is not a target")
        return _envelope(self.targets, self.hosts)


def _namespaces(targets: Sequence[Target]) -> list[str]:
    """Returns every target's namespace.

    A lone target may have no namespace. When there are several, each needs a
    unique one.
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
    return names


def _envelope(targets: Sequence[Target], hosts: Mapping[str, str]) -> dict[str, Any]:
    _check_peers(targets)
    _check_ports(targets, hosts)
    return {
        "config_version": CONFIG_VERSION,
        "hosts": dict(hosts),
        "targets": [target.to_config() for target in targets],
    }


def _links(targets: Sequence[Target]) -> list[tuple[Target, Link]]:
    return [
        (target, system)
        for target in targets
        for _, system, _ in target._systems
        if isinstance(system, Link)
    ]


def _check_peers(targets: Sequence[Target]) -> None:
    """Checks that every peer a link dials is in the deployment and listens
    on a fixed port."""
    for target, link in _links(targets):
        peer = link._peer
        if peer is None:
            continue
        where = f"`{link._name}` on `{target.namespace}`"
        if not any(member is peer._target for member in targets):
            raise ConfigError(f"{where} names a peer on a target this deployment does not include")
        port = peer._system.listen_port()
        if port is None:
            raise ConfigError(f"{where} names peer `{peer._name}`, which dials, so nothing can dial it")
        if port == 0:
            raise ConfigError(f"{where} names peer `{peer._name}`, which takes any port; give it one")


def _check_ports(targets: Sequence[Target], hosts: Mapping[str, str]) -> None:
    """Checks that no two listeners on the same host bind the same port."""
    bound: dict[tuple[str, int], str] = {}
    for target, link in _links(targets):
        port = link.listen_port()
        if port is None or port == 0:
            continue
        host = hosts.get(target.namespace or "", LOOPBACK)
        name = f"{target.namespace}.{link._name}"
        if (host, port) in bound:
            raise ConfigError(f"`{name}` and `{bound[host, port]}` both bind {host}:{port}")
        bound[host, port] = name


def gateway(
    namespace: str,
    peers: Sequence[SystemHandle],
    listen: str,
    cycle_rate: float = 10.0,
    path: str | None = None,
) -> tuple[Target, SystemHandle]:
    """Creates a gateway target that records telemetry from other targets.

    The gateway subscribes to each link in ``peers`` and records everything
    it receives in a `Db`, which clients reach on ``listen``. Returns the
    gateway target and the db's handle.
    """
    gw = Target(cycle_rate=cycle_rate, namespace=namespace)
    for peer in peers:
        gw.add(_namespace(peer), Subscribe(peer=peer))
    db = gw.add("db", Db(listen=listen, path=path, all=True), thread="db")
    return gw, db


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
