"""Group the targets one file configures, builds, and runs together."""

from __future__ import annotations

from typing import Any
from ._version import __version__, IR_VERSION
from ._builtins import _Ingest, _Subscribe
from ._model import _handle_name, _params
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
            for name, ingest in target._ingests.items():
                ingest.params = _params(self._resolve_ingest(name, ingest))
            _check_command_overlap(target)
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

    def _resolve_ingest(self, name: str, ingest: _Ingest) -> dict[str, Any]:
        """The source one ingest dials: which member serves the named link, on
        which port, and which command tokens it accepts. Everything here needs
        both members in scope, which is why it lives at the deployment and not
        at the ``add``."""
        source = ingest.source
        member = source.target
        if member not in self.targets:
            raise ValueError(
                f"`{name}`: `{source.name}` belongs to a target outside this "
                "deployment; list every member in `Deployment(targets=…)`"
            )
        if member.namespace is None:
            raise ValueError(
                f"`{name}`: the target serving `{source.name}` has no "
                "`namespace`; an ingest dials a member by namespace"
            )
        state = _state(member, source.name)
        if state["ty"] != "TcpServer":
            raise ValueError(
                f"`{name}`: `{source.name}` is a `{state['ty']}` state; an "
                "ingest dials a member's `TcpServer` link"
            )
        port = _port(state)
        if port == 0:
            raise ValueError(
                f"`{name}`: `{member.namespace}` serves `{source.name}` on "
                "port 0; an ingested link declares the port the gateway dials"
            )
        accepted = _uplink_msgs(member, source.name)
        if ingest.commands is None:
            commands = list(accepted)
        else:
            for token in ingest.commands:
                if token not in accepted:
                    listed = ", ".join(accepted) or "none"
                    raise ValueError(
                        f"`{name}`: `{member.namespace}` accepts no `{token}` "
                        f"on `{source.name}`; its uplinks there list {listed}"
                    )
            commands = list(ingest.commands)
        return {
            "namespace": member.namespace,
            "link": source.name,
            "port": port,
            "commands": commands,
        }

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


def _uplink_msgs(target: Target, link: str) -> list[str]:
    """The command tokens a member accepts on one link: the ``msgs`` of the
    ``Uplink``s attached to it, in config order."""
    tokens: list[str] = []
    for entry in target._systems:
        if entry["ty"] != "Uplink" or entry["attach"] != link:
            continue
        params = entry["params"]
        if params == "None":
            continue
        tokens += params["Value"].get("msgs") or []
    return tokens


def _check_command_overlap(gateway: Target) -> None:
    """Two ingests of one gateway may not forward the same token: the uplink
    that receives it would be picked by dial order."""
    by_token: dict[tuple[str | None, str], tuple[str, _Ingest]] = {}
    for name, ingest in gateway._ingests.items():
        for token in ingest.params["commands"]:
            first = by_token.get((ingest.attach, token))
            if first is not None:
                other, spec = first
                raise ValueError(
                    f"`{name}` forwards `{token}` to "
                    f"`{ingest.params['namespace']}` and `{other}` forwards it "
                    f"to `{spec.params['namespace']}`; one gateway sends a "
                    "token to one member, so narrow one with `commands=[…]`"
                )
            by_token[(ingest.attach, token)] = (name, ingest)
