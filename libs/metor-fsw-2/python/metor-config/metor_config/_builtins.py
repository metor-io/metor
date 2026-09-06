"""Configuration helpers for the built-in systems."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any
from ._model import Spec, StateHandle, static_system, _drop_none


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
