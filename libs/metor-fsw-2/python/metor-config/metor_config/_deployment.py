"""Group the targets one file configures, builds, and runs together."""

from __future__ import annotations

from typing import Any
from ._version import __version__, IR_VERSION
from ._builtins import _Subscribe
from ._model import _handle_name
from ._program import _warn_unadded
from ._target import Target


_deployments: list["Deployment"] = []


class Deployment:
    """The targets one ``target.py`` declares. One may exist per file; a file
    that constructs a bare :class:`Target` and no deployment emits a
    deployment of one.

    Members are listed in the order given — the order the IR carries them and
    the order ``metor-fsw build`` provisions them. A member's identity is its
    ``namespace``, so a deployment of more than one target requires every
    member to carry one (checked on the Rust side, with the rest of the
    namespace rules).

    ``hosts`` maps a member's namespace to the host it runs on, for the deploy
    renderer. A local run needs none: a mirror dials loopback first, then finds
    its peer over mDNS.
    """

    def __init__(self, targets: list[Target], hosts: dict[str, str] | None = None):
        if _deployments:
            raise RuntimeError("exactly one Deployment may exist in a target file")
        for target in targets:
            if not isinstance(target, Target):
                raise TypeError(
                    f"a Deployment member must be a Target, found "
                    f"`{type(target).__name__}`"
                )
        if len({id(t) for t in targets}) != len(targets):
            raise ValueError("a Target may belong to one deployment, and once")
        self.targets = list(targets)
        self.hosts = dict(hosts or {})
        for target in self.targets:
            for name, mirror in target._subscribes.items():
                mirror.peer = self._resolve_peer(name, mirror)
        _deployments.append(self)

    def _resolve_peer(self, name: str, mirror: _Subscribe) -> dict[str, Any]:
        """The ``peer`` spec one mirror records: which member publishes its
        instance, on which server, and on which port. Everything here needs
        both members in scope, which is why it lives at the deployment and not
        at the ``add``."""
        peer = mirror.of.target
        if peer not in self.targets:
            raise ValueError(
                f"`{name}`: `{mirror.of.name}` belongs to a target outside this "
                "deployment; list every member in `Deployment(targets=…)`"
            )
        if peer.namespace is None:
            raise ValueError(
                f"`{name}`: the target publishing `{mirror.of.name}` has no "
                "`namespace`; a mirror dials a member by namespace"
            )
        instance = mirror.of.name
        downlink = self._publisher(name, peer, instance, mirror.via)
        state = _state(peer, downlink["attach"])
        port = _port(state)
        if port == 0:
            raise ValueError(
                f"`{name}`: `{peer.namespace}` publishes `{instance}` on state "
                f"`{state['name']}`, which binds port 0; a published server "
                "declares the port its peers dial"
            )
        spec: dict[str, Any] = {
            "namespace": peer.namespace,
            "link": state["name"],
            "port": port,
            "instance": instance,
        }
        if not mirror.telemetered:
            spec["telemetered"] = False
        return spec

    def _publisher(
        self, name: str, peer: Target, instance: str, via: Any
    ) -> dict[str, Any]:
        """The peer's downlink carrying ``instance``: the one ``via`` names, the
        only one listing it, or, failing that, the only unfiltered one."""
        downlinks = [e for e in peer._systems if e["ty"] == "Downlink"]
        if via is not None:
            chosen = _handle_name(via)
            for entry in downlinks:
                if entry["name"] == chosen:
                    return entry
            raise ValueError(
                f"`{name}`: `via=` names `{chosen}`, which is not a downlink "
                f"of target `{peer.namespace}`"
            )
        carrying = [e for e in downlinks if _lists(e, instance)] or [
            e for e in downlinks if _unfiltered(e)
        ]
        if len(carrying) > 1:
            names = ", ".join(e["name"] for e in carrying)
            raise ValueError(
                f"`{name}`: target `{peer.namespace}` publishes `{instance}` on "
                f"more than one downlink ({names}); pick one with `via=`"
            )
        if not carrying:
            servers = (
                ", ".join(s["name"] for s in peer._states if s["ty"] == "TcpServer")
                or "none"
            )
            raise ValueError(
                f"`{name}`: target `{peer.namespace}` publishes no `{instance}`; "
                f"add `Publish(<server>, [{instance}])` there (its servers: {servers})"
            )
        return carrying[0]

    def to_ir(self) -> dict[str, Any]:
        """The serialized deployment: the emitter's version and one member
        ``Wiring`` per target, in the order given."""
        _warn_unadded()
        ir = {
            "ir_version": IR_VERSION,
            "metor_config_version": __version__,
            "targets": [t.to_ir() for t in self.targets],
        }
        if self.hosts:
            ir["hosts"] = dict(self.hosts)
        return ir


def _instances(downlink: dict[str, Any]) -> list[str] | None:
    params = downlink["params"]
    return None if params == "None" else params["Value"].get("instances")


def _lists(downlink: dict[str, Any], instance: str) -> bool:
    """Whether a downlink's instance filter names ``instance`` explicitly."""
    return instance in (_instances(downlink) or ())


def _unfiltered(downlink: dict[str, Any]) -> bool:
    """Whether a downlink taps everything (no instance filter)."""
    return _instances(downlink) is None


def _state(target: Target, name: str) -> dict[str, Any]:
    """The state entry a downlink attaches to."""
    return next(s for s in target._states if s["name"] == name)


def _port(state: dict[str, Any]) -> int:
    """The port a server state binds, from its `addr` params."""
    addr = state["params"]["Value"]["addr"]
    return int(addr.rsplit(":", 1)[1])
