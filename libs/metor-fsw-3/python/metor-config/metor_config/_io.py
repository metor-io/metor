"""Resources a target creates by name, and the records of the io steps."""

from __future__ import annotations

import ipaddress
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
    """A TCP listener bound to ``addr``, taking streams from the peers in the
    ``allow`` CIDR blocks, or from any peer when ``allow`` is empty."""

    addr: str
    allow: Sequence[str] = ()
    _key = "tcp_listen"

    def __post_init__(self) -> None:
        for block in self.allow:
            _check_cidr(block)

    def _fields(self) -> dict[str, Any]:
        fields: dict[str, Any] = {"addr": self.addr}
        if self.allow:
            fields["allow"] = list(self.allow)
        return fields


def _check_cidr(block: object) -> None:
    """Refuses what the target's parser refuses: anything but an IP address,
    a slash, and a prefix length no wider than the address."""
    bad = ConfigError(f"`{block}` is not a CIDR block such as `10.0.0.0/24`")
    if not isinstance(block, str):
        raise bad
    addr, _, prefix = block.partition("/")
    if not (prefix.isascii() and prefix.isdigit()) or "%" in addr:
        raise bad
    try:
        ip = ipaddress.ip_address(addr)
    except ValueError:
        raise bad from None
    if int(prefix) > ip.max_prefixlen:
        raise bad


@dataclass(frozen=True)
class TcpDial(Kind):
    """A TCP dialer: each stream connects to ``addr``, an IP literal."""

    addr: str
    _key = "tcp_dial"

    def _fields(self) -> dict[str, Any]:
        return {"addr": self.addr}


@dataclass(frozen=True)
class PeerDial(Kind):
    """A TCP dialer of ``link`` on the target named ``target``, in the same
    deployment. `metor build` resolves it to a `TcpDial`."""

    target: str
    link: str

    def to_json(self) -> dict[str, Any]:
        return {"peer": {"target": self.target, "link": self.link}}


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
