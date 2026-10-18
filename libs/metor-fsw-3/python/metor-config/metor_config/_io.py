"""Resources a target creates by name, and the records of the io steps."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Sequence

from ._config import ConfigError


class Kind:
    """A real resource: a serial port, a socket, a file, and so on."""

    _key: str = ""

    def _fields(self) -> dict[str, Any]:
        raise NotImplementedError

    def to_json(self) -> dict[str, Any]:
        return {"real": {self._key: self._fields()}}


@dataclass(frozen=True)
class Tty(Kind):
    """A serial port, opened in raw mode at ``baud``."""

    path: str
    baud: int = 9600
    _key = "tty"

    def _fields(self) -> dict[str, Any]:
        return {"path": self.path, "baud": self.baud}


@dataclass(frozen=True)
class Can(Kind):
    """A raw CAN socket on ``iface``. Each filter is an ``(id, mask)`` pair."""

    iface: str
    filters: Sequence[tuple[int, int]] = ()
    _key = "can"

    def _fields(self) -> dict[str, Any]:
        filters = [{"id": id, "mask": mask} for id, mask in self.filters]
        return {"iface": self.iface, "filters": filters}


@dataclass(frozen=True)
class GpioLine(Kind):
    """One GPIO line of a chip, as an ``"input"`` or an ``"output"``."""

    chip: str
    line: int
    direction: str = "input"
    _key = "gpio_line"

    def _fields(self) -> dict[str, Any]:
        return {"chip": self.chip, "line": self.line, "direction": self.direction}


@dataclass(frozen=True)
class Udp(Kind):
    """A UDP socket bound to ``bind``, and connected to ``connect`` if set."""

    bind: str
    connect: str | None = None
    _key = "udp"

    def _fields(self) -> dict[str, Any]:
        return {"bind": self.bind, "connect": self.connect}


@dataclass(frozen=True)
class File(Kind):
    """A file, opened for ``"read"``, ``"write"``, or ``"read_write"``."""

    path: str
    access: str = "read_write"
    _key = "file"

    def _fields(self) -> dict[str, Any]:
        return {"path": self.path, "access": self.access}


@dataclass(frozen=True)
class Dir(Kind):
    """A directory that files are opened under."""

    path: str
    _key = "dir"

    def _fields(self) -> dict[str, Any]:
        return {"path": self.path}


@dataclass(frozen=True)
class TcpListen(Kind):
    """A TCP listener bound to ``addr``."""

    addr: str
    _key = "tcp_listen"

    def _fields(self) -> dict[str, Any]:
        return {"addr": self.addr}


class SimKind(Kind):
    """A simulated resource: a wire with two ends. Drivers hold its host end
    and models its `device` end, each through a `Cap`. The target names both
    holders in the resource's entry."""


@dataclass(frozen=True)
class SimUart(SimKind):
    """A simulated serial port."""

    _key = "uart"


@dataclass(frozen=True)
class SimGpioLine(SimKind):
    """A simulated GPIO line."""

    _key = "gpio_line"


@dataclass(frozen=True, eq=False)
class ResourceHandle:
    """A resource created with `Target.resource`. Grant it to a system's
    `Cap` parameter by passing it as that keyword.

    The handle names the resource's host end. A sim resource's other end is
    `device`, which a model takes.
    """

    name: str
    kind: Kind
    target: Any = field(repr=False)
    end: str = "host"

    @property
    def device(self) -> ResourceHandle:
        """The device end of a sim resource, for the model that serves it."""
        if not isinstance(self.kind, SimKind):
            raise ConfigError(f"`{self.name}` is real, so it has no device end")
        return ResourceHandle(self.name, self.kind, self.target, "device")

    def to_json(self) -> dict[str, Any]:
        """The resource's backing, as the target's `resources` map holds it."""
        return self.kind.to_json()
