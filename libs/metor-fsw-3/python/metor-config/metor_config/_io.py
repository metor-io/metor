"""Resources a target creates by name, and the records of the io steps."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Sequence

from ._config import ConfigError
from ._model import Record


class ResourceStatus(Record):
    """One resource's op counts, written by `io_complete` when they change."""

    _name = "io_status"
    _kind = "frame"


IO_RECORDS = ("io_sqe", "io_cqe")
"""The records the io steps carry, which no link publishes."""


class Kind:
    """A real resource: a serial port, a socket, a file, and so on.

    ``interface`` is the `Cap` interface the resource provides.
    """

    interface: str = ""
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
    interface = "uart"
    _key = "tty"

    def _fields(self) -> dict[str, Any]:
        return {"path": self.path, "baud": self.baud}


@dataclass(frozen=True)
class Can(Kind):
    """A raw CAN socket on ``iface``. Each filter is an ``(id, mask)`` pair."""

    iface: str
    filters: Sequence[tuple[int, int]] = ()
    interface = "can"
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
    interface = "gpio_line"
    _key = "gpio_line"

    def _fields(self) -> dict[str, Any]:
        if self.direction not in ("input", "output"):
            raise ConfigError(f"a GPIO line is `input` or `output`, got {self.direction!r}")
        return {"chip": self.chip, "line": self.line, "direction": self.direction}


@dataclass(frozen=True)
class Udp(Kind):
    """A UDP socket bound to ``bind``, and connected to ``connect`` if set."""

    bind: str
    connect: str | None = None
    interface = "udp"
    _key = "udp"

    def _fields(self) -> dict[str, Any]:
        return {"bind": self.bind, "connect": self.connect}


@dataclass(frozen=True)
class File(Kind):
    """A file, opened for ``"read"``, ``"write"``, or ``"read_write"``."""

    path: str
    access: str = "read_write"
    interface = "file"
    _key = "file"

    def _fields(self) -> dict[str, Any]:
        if self.access not in ("read", "write", "read_write"):
            raise ConfigError(f"a file's access is `read`, `write`, or `read_write`, got {self.access!r}")
        return {"path": self.path, "access": self.access}


@dataclass(frozen=True)
class Dir(Kind):
    """A directory that files are opened under."""

    path: str
    interface = "dir"
    _key = "dir"

    def _fields(self) -> dict[str, Any]:
        return {"path": self.path}


@dataclass(frozen=True)
class TcpListen(Kind):
    """A TCP listener bound to ``addr``."""

    addr: str
    interface = "listen"
    _key = "tcp_listen"

    def _fields(self) -> dict[str, Any]:
        return {"addr": self.addr}


@dataclass(frozen=True)
class TcpConnect(Kind):
    """A TCP address to dial. Each `connect` makes a new stream."""

    addr: str
    interface = "dial"
    _key = "tcp_connect"

    def _fields(self) -> dict[str, Any]:
        return {"addr": self.addr}


class SimKind(Kind):
    """A simulated resource: a wire with two ends. Drivers hold its host end
    and models its `device` end, each through a `Cap`. The target names both
    holders in the resource's entry."""


@dataclass(frozen=True)
class SimUart(SimKind):
    """A simulated serial port."""

    interface = "uart"
    _key = "uart"


@dataclass(frozen=True)
class SimGpioLine(SimKind):
    """A simulated GPIO line."""

    interface = "gpio_line"
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
