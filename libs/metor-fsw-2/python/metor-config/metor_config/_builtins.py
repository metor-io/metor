"""Configuration helpers for the built-in systems."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, cast
from ._model import (
    H,
    Spec,
    StateHandle,
    SystemHandle,
    static_system,
    _drop_none,
    _handle_name,
    _params,
)
from ._dashboard import Preset


@dataclass(frozen=True)
class Component:
    """The component value an alarm monitors: an instance-prefixed component id
    plus an optional element index into its shape."""

    component: str
    element: int | None = None

    def to_json(self) -> dict[str, Any]:
        return _drop_none({"component": self.component, "element": self.element})


@dataclass(frozen=True)
class band:  # noqa: N801 - a data literal, spelled lowercase by design
    """A pair of optional thresholds bounding acceptable values at one severity."""

    above: float | None = None
    below: float | None = None

    def to_json(self) -> dict[str, Any]:
        return _drop_none({"above": self.above, "below": self.below})


@dataclass(frozen=True)
class Alarm:
    """A limit alarm over a single component value. Band containment and the
    warning/critical-required rule are validated on the Rust deserialize path,
    the one source of truth."""

    id: str
    name: str
    target: Component
    description: str = ""
    warning: band | None = None
    critical: band | None = None
    debounce: int | None = None
    hysteresis: float | None = None
    latching: bool | None = None
    severity: str | None = None

    def to_json(self) -> dict[str, Any]:
        return _drop_none(
            {
                "id": self.id,
                "name": self.name,
                "description": self.description,
                "target": self.target.to_json(),
                "warning": self.warning.to_json() if self.warning else None,
                "critical": self.critical.to_json() if self.critical else None,
                "debounce": self.debounce,
                "hysteresis": self.hysteresis,
                "latching": self.latching,
                "severity": self.severity,
            }
        )


def Alarms(alarms: list[Alarm]) -> Spec:  # noqa: N802 - a system-type wrapper
    """The built-in alarm engine, its ``AlarmsParams`` carrying one entry per
    alarm under the ``alarm`` field the Rust struct declares."""
    return static_system("Alarms", alarm=[a.to_json() for a in alarms])


def TcpServer(addr: str, name: str | None = None) -> Spec:  # noqa: N802
    """The built-in link server state (``LinkParams``): the FSW listens on
    ``addr``; ground tools connect to it for the downlink stream and command
    ingest alike. Declare it with :meth:`Target.state`.

    ``name`` is the human node name advertised over mDNS for discovery; when
    omitted the FSW falls back to the OS hostname."""
    return static_system("TcpServer", **_drop_none({"addr": addr, "name": name}))


def _attached(spec: Spec, state: StateHandle) -> Spec:
    """Attach ``spec`` to the pack-shared ``state`` and return it."""
    spec.attach = state.name
    return spec


def Uplink(state: StateHandle, msgs: list[str] | None = None) -> Spec:  # noqa: N802
    """The built-in command uplink (``UplinkParams``), draining the ``state``
    link server (the handle :meth:`Target.state` returned for a
    :func:`TcpServer`). Add it before its consumers and a command is consumed
    the same cycle it arrives."""
    return _attached(static_system("Uplink", **_drop_none({"msgs": msgs})), state)


def Downlink(  # noqa: N802
    state: StateHandle,
    instances: list[str] | None = None,
    frames: list[str] | None = None,
) -> Spec:
    """The built-in telemetry downlink (``DownlinkParams``), streaming over the
    ``state`` link server (the handle :meth:`Target.state` returned for a
    :func:`TcpServer`); omitting both subset lists taps everything."""
    return _attached(
        static_system(
            "Downlink", **_drop_none({"instances": instances, "frames": frames})
        ),
        state,
    )


class _Publish(Spec):
    """A :func:`Publish`'s spec: a `Downlink` tapping the named instances,
    which must belong to the target that adds it."""

    def __init__(self, state: StateHandle, instances: "list[SystemHandle | Spec]"):
        super().__init__(
            "Downlink", None, {"instances": [_handle_name(h) for h in instances]}
        )
        self.attach = state.name
        self.state = state
        # Handles are typed as their spec class, so the target behind one is
        # read dynamically, as `_handle_name` reads the name.
        self._instances: list[Any] = list(instances)

    def _bind(self, target: Any) -> None:
        for handle in self._instances:
            if handle.target is not target:
                raise ValueError(
                    f"Publish: `{handle.name}` belongs to another target; a "
                    "downlink taps instances of the target it runs on"
                )


def Publish(  # noqa: N802 - a system-type wrapper
    state: StateHandle, instances: "list[SystemHandle | Spec]"
) -> Spec:
    """Offer ``instances`` to peers over the ``state`` link server: a
    :func:`Downlink` filtered to exactly them, which a peer member's
    :func:`Subscribe` mirrors. Serve it on its own `TcpServer`, so a ground
    downlink and a peer publication do not share an announce set."""
    return _Publish(state, instances)


class _Subscribe(Spec):
    """A :func:`Subscribe`'s spec: the peer instance's own type and artifact,
    run by the built-in subscriber. ``peer`` is filled by
    :class:`Deployment`, the only scope that sees both members."""

    def __init__(
        self, of: SystemHandle, via: "SystemHandle | Spec | None", telemetered: bool
    ):
        spec = of.spec
        if spec is None:
            raise TypeError(
                f"Subscribe: `{of.name}` is not a system instance of a peer "
                "target; mirror a pack system or a built-in, not a `@system` "
                "or a slot"
            )
        super().__init__(spec.ty, spec.artifact, {})
        decl = getattr(spec, "artifact_decl", None)
        if decl is not None:
            self.artifact_decl = decl
        self.of = of
        self.via = via
        self.telemetered = telemetered
        self.peer: dict[str, Any] | None = None

    def _bind(self, target: Any) -> None:
        if self.of.target is target:
            raise ValueError(
                f"Subscribe: `{self.of.name}` runs on this target; a mirror "
                "names an instance of another member"
            )

    def _peer_json(self) -> dict[str, Any]:
        if self.peer is None:
            raise RuntimeError(
                f"Subscribe: `{self.of.name}` was never resolved against a "
                "Deployment; list every member in one `Deployment(targets=…)`"
            )
        return self.peer


def Subscribe(  # noqa: N802 - a system-type wrapper
    of: H, via: "SystemHandle | Spec | None" = None, telemetered: bool = True
) -> H:
    """Mirror a peer member's instance ``of``: the same ports, one cycle late,
    fed by a client of the server that :func:`Publish`es it. Typed as the peer
    instance's own class, so an edge out of the mirror checks like a local one.

    ``via`` names the :func:`Publish` to dial when the peer offers the instance
    on more than one server. ``telemetered=False`` keeps the mirrored ports out
    of this target's own downlink."""
    return cast(H, _Subscribe(cast(SystemHandle, of), via, telemetered))


class _Presets(Spec):
    """The preset broadcaster's spec. Its component references are
    namespace-relative, like alarm targets, and are qualified when a target
    registers it — so the ids match what that target announces."""

    def __init__(self, presets: list[Preset]):
        super().__init__("Presets", None, {})
        self._presets = presets

    def _bind(self, target: Any) -> None:
        self.params = _params(
            {"preset": [p.to_json(target.namespace) for p in self._presets]}
        )


def Presets(presets: list[Preset]) -> Spec:  # noqa: N802 - a system-type wrapper
    """The built-in preset broadcaster, its ``PresetsParams`` carrying one
    entry per preset under the ``preset`` field the Rust struct declares."""
    return _Presets(presets)


def Db(  # noqa: N802 - a system-type wrapper
    addr: str,
    path: str | None = None,
    name: str | None = None,
    store: str | None = None,
    max_bytes: int | None = None,
    max_age_secs: float | None = None,
) -> Spec:
    """The built-in embedded db state (``DbParams``): the gateway serves it on
    ``addr``, which is what the panel dials. Declare it with
    :meth:`Target.state` and attach :func:`Ingest`s and a :func:`Record` to it.

    ``path`` is the data directory, a fresh temp dir per run when omitted;
    ``name`` is the mDNS instance name, the namespace by default. ``store``
    turns on tiering into that directory, bounded by ``max_bytes`` and
    ``max_age_secs``; without a store nothing is evicted."""
    return static_system(
        "Db",
        **_drop_none(
            {
                "addr": addr,
                "path": path,
                "name": name,
                "store": store,
                "max_bytes": max_bytes,
                "max_age_secs": max_age_secs,
            }
        ),
    )


class _Ingest(Spec):
    """An :func:`Ingest`'s spec: the member's link, resolved to a
    ``(namespace, link, port, commands)`` source by :class:`Deployment`, the
    only scope that sees both members."""

    def __init__(
        self, db: StateHandle, source: StateHandle, commands: list[str] | None
    ):
        super().__init__("Ingest", None, {})
        self.attach = db.name
        self.db = db
        self.source = source
        self.commands = commands

    def _bind(self, target: Any) -> None:
        if self.source.target is target:
            raise ValueError(
                f"Ingest: `{self.source.name}` is a link of this target; an "
                "ingest dials another member's link"
            )

    def _params_json(self) -> Any:
        if not self.params:
            raise RuntimeError(
                f"Ingest: `{self.source.name}` was never resolved against a "
                "Deployment; list every member in one `Deployment(targets=…)`"
            )
        return self._param_source()


def Ingest(  # noqa: N802 - a system-type wrapper
    db: StateHandle, source: StateHandle, commands: list[str] | None = None
) -> Spec:
    """Stream one member's telemetry into ``db``: a client of the ``source``
    link server (the handle :meth:`Target.state` returned for that member's
    :func:`TcpServer`), and the path its commands take back up. Add one per
    member; the name is what its status frame is called
    (``gw.plant.source_status``).

    ``commands`` narrows the tokens forwarded up this link to a subset of the
    member's :func:`Uplink` ``msgs``; ``None`` is the member's whole set,
    ``[]`` is none. Two ingests of one gateway may not forward the same
    token."""
    return _Ingest(db, source, commands)


def Record(db: StateHandle) -> Spec:  # noqa: N802 - a system-type wrapper
    """Store this target's own telemetered outputs into ``db``, straight from
    the rings the :func:`Downlink` taps: the coordinator's and each system's
    ``system_status`` and ``log``, and every :func:`Ingest`'s status frame.
    One per db."""
    return _attached(static_system("Record"), db)
