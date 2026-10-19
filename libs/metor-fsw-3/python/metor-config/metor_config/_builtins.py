"""Built-in systems that any target can add.

`Publish` sends records to other targets, and `Subscribe` receives them.
`DlSlot` and `ChannelRegistry` support channels.

These systems are built into the host rather than supplied by a pack, so
their pack has no library and is left out of the config's pack list.

The two link systems (`Publish` and `Subscribe`) each take exactly one
transport: a `listen` address or a `connect` address.
"""

from __future__ import annotations

from typing import Any, Sequence, TypeVar

from . import _generated
from ._config import ConfigError
from ._generated import IO_RECORDS, SequenceRegistry
from ._model import (
    Item,
    OutPort,
    Record,
    Source,
    Sources,
    System,
    _edges,
    _item_ports,
    _json,
    _unique,
)
from ._target import Target

T = TypeVar("T", bound=Record)

MAX_CONNECTIONS = 8
CONN_CAP = 1 << 20
INBOUND_CAP = 256


class Link(System):
    """Base class for systems that send or receive records over a network
    connection.

    Peers identify a link by its target's namespace and its own name.
    """

    def __init__(
        self,
        params: dict[str, Any],
        listen: str | None,
        connect: str | None,
        max_connections: int | None,
        items: Sequence[Item] = (),
        records: Sequence[type[Record]] = (),
    ) -> None:
        self._transport = _transport(listen, connect, max_connections)
        super().__init__({}, params, items=items, records=records)

    def _params_for(self, target: Any, index: int) -> Any:
        link = target._systems[index].name
        return {
            "transport": self._transport,
            "namespace": target.namespace,
            "link": link,
            **self._params,
        }


class Sink(Link):
    """A link that reads the ports listed in `items`.

    With `all`, it also reads every port of every system added before it.
    """

    def __init__(
        self,
        params: dict[str, Any],
        listen: str | None,
        connect: str | None,
        max_connections: int | None,
        items: Sequence[Item],
        all: bool,
    ) -> None:
        super().__init__(params, listen, connect, max_connections, items=items)
        self._items = list(items)
        self._all = all

    def _inputs_for(self, target: Target, index: int) -> dict[str, list[Source[Any]]]:
        if not self._all:
            return self._inputs
        earlier = [target._handles[entry.name] for entry in target._systems[:index]]
        ports = [p for p in _item_ports(earlier) if p.record is None or p.record._name not in IO_RECORDS]
        return _edges(_item_ports(self._items) + ports)


class Publish(Sink, _generated.Publish):
    """Sends records to every connected peer.

    `items` lists what to publish. An item is either a system handle, which
    publishes all of that system's ports, or a single port. With `all`, it
    publishes every system added before this one.
    """

    def __init__(
        self,
        items: Sequence[Item] = (),
        listen: str | None = None,
        connect: str | None = None,
        max_connections: int | None = None,
        conn_cap: int = CONN_CAP,
        all: bool = False,
    ) -> None:
        super().__init__({"conn_cap": conn_cap}, listen, connect, max_connections, items, all)


class Subscribe(Link, _generated.Subscribe):
    """Receives records from peers and writes them to its own output ports.

    `records` lists message classes to accept. Each one gets an output port
    named after the record.
    """

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
            {"conn_cap": conn_cap, "inbound_cap": inbound_cap},
            listen,
            connect,
            max_connections,
            records=records,
        )


def _transport(
    listen: str | None,
    connect: str | None,
    max_connections: int | None,
) -> dict[str, Any]:
    if (listen is None) == (connect is None):
        raise ConfigError("a link takes exactly one of `listen` and `connect`")
    if listen is None:
        if max_connections is not None:
            raise ConfigError("a dialing link holds one connection, so it takes no `max_connections`")
        return {"connect": {"addr": connect}}
    slots = MAX_CONNECTIONS if max_connections is None else max_connections
    return {"listen": {"addr": listen, "max_connections": slots}}


class DlSlot(_generated.DlSlot):
    """A slot in the schedule that can swap which pack system runs in it.

    A `Load` command on the slot's channel picks the system to run.
    ``allow`` lists the pack systems it can load; each is named by its type,
    and all must have the same ports as the first. ``initial`` names the
    system loaded at boot. Inputs are wired on the slot itself, using the
    first system's input names as keywords. The slot must be added with
    ``channel=`` on `Target.add`.
    """

    def __init__(
        self, allow: Sequence[System], initial: str | None = None, **inputs: Sources[Any]
    ) -> None:
        if not allow:
            raise ConfigError("a slot allows at least one system")
        names = _unique([system._ty for system in allow], "a slot allows")
        if initial is not None and initial not in names:
            raise ConfigError(f"`initial` names `{initial}`, which `allow` does not list")
        candidates = [_candidate(system) for system in allow]
        super().__init__(inputs, {"allow": candidates, "initial": initial})
        self._ports = dict(type(allow[0])._ports)
        self._names = names


def _candidate(system: System) -> dict[str, Any]:
    """Returns one `allow` entry: the system's type and params, and the pack
    that `metor build` opens to resolve it."""
    if not isinstance(system, System) or isinstance(system, (Link, DlSlot)):
        raise ConfigError(f"a slot allows pack systems, got {system!r}")
    if any(system._inputs.values()):
        raise ConfigError(f"`{system._ty}` in a slot takes its inputs on the slot")
    return {
        "name": system._ty,
        "ty": system._ty,
        "params": _json(system._params) or None,
        "pack": system._pack.to_json(),
    }


class ChannelRegistry(_generated.ChannelRegistry):
    """Publishes a list of the target's channels.

    The list includes the candidates each slot can load. It is written on
    the first cycle and then once a second.
    """

    def __init__(self) -> None:
        super().__init__({}, {})

    def _params_for(self, target: Any, index: int) -> Any:
        channels = [
            {"name": name, "available": system._names if isinstance(system, DlSlot) else []}
            for name, system in target._channels()
        ]
        return {"channels": channels, "period": max(1, round(target.cycle_rate))}

    registry: OutPort[SequenceRegistry]
