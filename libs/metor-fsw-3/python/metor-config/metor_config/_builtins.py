"""The links every target may add: `Publish` out and `Subscribe` in.

Both are built into the host rather than supplied by a pack, so their pack has
no library and `Target.to_config` leaves it out of the config's pack list.
"""

from __future__ import annotations

from typing import Any, Sequence, TypeAlias, Union

from ._config import ABI_VERSION, ConfigError
from ._model import OutPort, Pack, Record, Source, System
from ._target import SystemHandle, Target

Item: TypeAlias = Union[SystemHandle, OutPort[Any]]
"""What a `Publish` takes: a system's whole handle, or one of its ports."""

PACK = Pack(id="fsw", lib="", libs="", abi_version=ABI_VERSION)

MAX_CONNECTIONS = 8
PENDING_CAP = 1 << 20
INBOUND_CAP = 256


class Publish(System):
    """Serves the records `items` name to whoever connects.

    An item is a system handle, which publishes every port that handle knows,
    or one port. `all` publishes every system added before this one.
    """

    _pack = PACK
    _ty = "publish"
    _outputs = ("link_status",)
    _wants_target = True

    def __init__(
        self,
        items: Sequence[Item] = (),
        listen: str | None = None,
        connect: str | None = None,
        max_connections: int | None = None,
        pending_cap: int = PENDING_CAP,
        all: bool = False,
    ) -> None:
        super().__init__(
            {},
            {
                "transport": _transport(listen, connect, max_connections),
                "namespace": None,
                "link": "",
                "pending_cap": pending_cap,
            },
        )
        self._items = list(items)
        self._all = all
        self._inputs = _edges(_ports(self._items))

    def _inputs_for(self, target: Target, index: int) -> dict[str, list[Source[Any]]]:
        if not self._all:
            return self._inputs
        earlier = [target._handles[name] for name, _, _ in target._systems[:index]]
        return _edges(_ports(self._items) + _ports(earlier))


class Subscribe(System):
    """Writes the records `records` names onto ports of its own, as peers send them.

    Each record is one port, named after the record; `cmds.ping` reads it.
    """

    _pack = PACK
    _ty = "subscribe"
    _wants_target = True

    def __init__(
        self,
        records: Sequence[type[Record]] = (),
        listen: str | None = None,
        connect: str | None = None,
        max_connections: int | None = None,
        pending_cap: int = PENDING_CAP,
        inbound_cap: int = INBOUND_CAP,
    ) -> None:
        names = [_record_name(record) for record in records]
        for name in names:
            if names.count(name) > 1:
                raise ConfigError(f"a link subscribes to record `{name}` twice")
        super().__init__(
            {},
            {
                "transport": _transport(listen, connect, max_connections),
                "namespace": None,
                "link": "",
                "pending_cap": pending_cap,
                "inbound_cap": inbound_cap,
            },
            outputs=[{"port": name, "record": name} for name in names],
        )
        self._outputs = tuple(names) + ("link_status",)
        self._record_packs = tuple(
            record._pack for record in records if record._pack is not None
        )


def _transport(
    listen: str | None, connect: str | None, max_connections: int | None
) -> dict[str, Any]:
    if (listen is None) == (connect is None):
        raise ConfigError("a link takes exactly one of `listen` and `connect`")
    if listen is None:
        if max_connections is not None:
            raise ConfigError("a dialing link holds one connection, so it takes no `max_connections`")
        return {"connect": {"addr": connect}}
    slots = MAX_CONNECTIONS if max_connections is None else max_connections
    return {"listen": {"addr": listen, "max_connections": slots}}


def _record_name(record: type[Record]) -> str:
    if not isinstance(record, type) or not issubclass(record, Record):
        raise ConfigError(f"a link subscribes to record classes, got {record!r}")
    if not record._name:
        raise ConfigError(f"record class `{record.__name__}` names no record")
    return record._name


def _ports(items: Sequence[Item]) -> list[OutPort[Any]]:
    """Every port the items name, a handle standing for all of its own."""
    ports: list[OutPort[Any]] = []
    for item in items:
        if isinstance(item, SystemHandle):
            ports.extend(OutPort(item._name, port) for port in item._outputs)
        elif isinstance(item, OutPort):
            ports.append(item)
        else:
            raise ConfigError(f"a link publishes handles and ports, got {item!r}")
    return ports


def _edges(ports: Sequence[OutPort[Any]]) -> dict[str, list[Source[Any]]]:
    """One input per port, named `{producer}.{port}`, first mention winning."""
    edges: dict[str, list[Source[Any]]] = {}
    for port in ports:
        edges.setdefault(f"{port.ref.system}.{port.ref.port}", [port])
    return edges
