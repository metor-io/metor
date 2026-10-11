"""The links every target may add: `Publish` out and `Subscribe` in.

Both are built into the host rather than supplied by a pack, so their pack has
no library and `Target.to_config` leaves it out of the config's pack list.
"""

from __future__ import annotations

from typing import Any, Sequence

from ._config import ABI_VERSION, ConfigError
from ._model import Item, OutPort, Pack, Record, Source, System, _edges, _ports
from ._target import Target

PACK = Pack(id="fsw", lib="", libs="", abi_version=ABI_VERSION)

MAX_CONNECTIONS = 8
CONN_CAP = 1 << 20
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
    _takes_inputs = True

    def __init__(
        self,
        items: Sequence[Item] = (),
        listen: str | None = None,
        connect: str | None = None,
        max_connections: int | None = None,
        conn_cap: int = CONN_CAP,
        all: bool = False,
    ) -> None:
        super().__init__(
            {},
            {
                "transport": _transport(listen, connect, max_connections),
                "namespace": None,
                "link": "",
                "conn_cap": conn_cap,
            },
            items=items,
        )
        self._items = list(items)
        self._all = all

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
    _outputs = ("link_status",)
    _wants_target = True
    _takes_outputs = True

    def __init__(
        self,
        records: Sequence[type[Record]] = (),
        listen: str | None = None,
        connect: str | None = None,
        max_connections: int | None = None,
        conn_cap: int = CONN_CAP,
        inbound_cap: int = INBOUND_CAP,
    ) -> None:
        super().__init__(
            {},
            {
                "transport": _transport(listen, connect, max_connections),
                "namespace": None,
                "link": "",
                "conn_cap": conn_cap,
                "inbound_cap": inbound_cap,
            },
            records=records,
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
    if slots < 1:
        raise ConfigError("a listening link takes at least one connection")
    return {"listen": {"addr": listen, "max_connections": slots}}
