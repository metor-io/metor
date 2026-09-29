"""The systems every target may add: `Publish` out, `Subscribe` in, `Db`,
the embedded database a gateway records into, and `DlSlot` and
`ChannelRegistry` for channels.

All are built into the host rather than supplied by a pack, so their pack has
no library and `Target.to_config` leaves it out of the config's pack list. Each
link owns one transport: a `listen` address, a `connect` address, or a `peer`
link of the deployment that `metor build` turns into an address.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Sequence, TypeVar

from ._config import ABI_VERSION, ConfigError, check_abi
from ._model import (
    FRAMEWORK_PORTS,
    Item,
    OutPort,
    Pack,
    Record,
    SequenceRegistry,
    Source,
    Sources,
    System,
    _edges,
    _item_ports,
    _json,
    _unique,
)
from ._target import SystemHandle, Target

T = TypeVar("T", bound=Record)

PACK = Pack(id="fsw", lib="", libs="", abi_version=ABI_VERSION)

MAX_CONNECTIONS = 8
CONN_CAP = 1 << 20
INBOUND_CAP = 256


class LinkStatus(Record):
    """What a link reports about its sockets."""

    _name = "link_status"
    _kind = "frame"
    _pack = PACK


class DbStatus(Record):
    """What a `Db` reports about what it ingested."""

    _name = "db_status"
    _kind = "frame"
    _pack = PACK


class Link(System):
    """A system that owns one transport and is named on the wire by its
    target's namespace and its own instance name."""

    _pack = PACK

    def __init__(
        self,
        params: dict[str, Any],
        listen: str | None,
        connect: str | None,
        peer: SystemHandle | None,
        max_connections: int | None,
        items: Sequence[Item] = (),
        records: Sequence[type[Record]] = (),
    ) -> None:
        self._peer = _peer(peer)
        self._transport = _transport(listen, connect, peer, max_connections)
        super().__init__({}, params, items=items, records=records)

    def _params_for(self, target: Any, index: int) -> Any:
        link = target._systems[index][0]
        return {
            "transport": self._transport_json(),
            "namespace": target.namespace,
            "link": link,
            **self._params,
        }

    def _transport_json(self) -> dict[str, Any]:
        if self._peer is None:
            return self._transport
        return {"peer": {"target": _namespace(self._peer), "link": self._peer._name}}

    def listen_port(self) -> int | None:
        """The port this link binds, or `None` for one that dials."""
        listen = self._transport.get("listen") if self._peer is None else None
        if listen is None:
            return None
        return int(listen["addr"].rsplit(":", 1)[1])


class Sink(Link):
    """A link fed by the ports `items` name; `all` feeds it every system added
    before it."""

    _takes_inputs = True

    def __init__(
        self,
        params: dict[str, Any],
        listen: str | None,
        connect: str | None,
        peer: SystemHandle | None,
        max_connections: int | None,
        items: Sequence[Item],
        all: bool,
    ) -> None:
        super().__init__(params, listen, connect, peer, max_connections, items=items)
        self._items = list(items)
        self._all = all

    def _inputs_for(self, target: Target, index: int) -> dict[str, list[Source[Any]]]:
        if not self._all:
            return self._inputs
        earlier = [target._handles[name] for name, _, _ in target._systems[:index]]
        return _edges(_item_ports(self._items) + _item_ports(earlier))


class Publish(Sink):
    """Serves the records `items` name to whoever connects.

    An item is a system handle, which publishes every port that handle knows,
    or one port. `all` publishes every system added before this one.
    """

    _ty = "publish"
    _ports = {"link_status": LinkStatus, **FRAMEWORK_PORTS}

    def __init__(
        self,
        items: Sequence[Item] = (),
        listen: str | None = None,
        connect: str | None = None,
        peer: SystemHandle | None = None,
        max_connections: int | None = None,
        conn_cap: int = CONN_CAP,
        all: bool = False,
    ) -> None:
        super().__init__({"conn_cap": conn_cap}, listen, connect, peer, max_connections, items, all)


class Db(Sink):
    """The embedded database: records every port `items` name and serves the
    result on `listen`. `path` is the store's directory; unset, a fresh
    temporary one. `all` records every system added before this one.
    """

    _ty = "db"
    _ports = {"db_status": DbStatus, **FRAMEWORK_PORTS}

    def __init__(
        self,
        listen: str,
        path: str | None = None,
        items: Sequence[Item] = (),
        max_connections: int | None = None,
        all: bool = False,
    ) -> None:
        super().__init__(
            {"path": path, "command_ids": []}, listen, None, None, max_connections, items, all
        )


class Subscribe(Link):
    """Writes the records it is sent onto ports of its own.

    `records` lists message classes, one port each named after the record,
    resolved against this target's packs. With a `peer`, `items` lists the
    peer's published ports to copy, each keeping its `{system}.{port}` name;
    a message record arrives on one port however many the peer publishes it
    on, the first listed. No `items` and no `records` copies everything the
    peer publishes. `port()` returns this system's copy of a published port.
    """

    _ty = "subscribe"
    _takes_outputs = True
    _ports = {"link_status": LinkStatus, **FRAMEWORK_PORTS}

    def __init__(
        self,
        records: Sequence[type[Record]] = (),
        listen: str | None = None,
        connect: str | None = None,
        peer: SystemHandle | None = None,
        items: Sequence[Item] = (),
        max_connections: int | None = None,
        conn_cap: int = CONN_CAP,
        inbound_cap: int = INBOUND_CAP,
    ) -> None:
        if items and peer is None:
            raise ConfigError("`items` names a peer's ports, so it needs `peer`")
        super().__init__(
            {"conn_cap": conn_cap, "inbound_cap": inbound_cap},
            listen,
            connect,
            peer,
            max_connections,
            records=records,
        )
        self._items = list(items)
        self._records = list(records)
        self._copies: list[Copy] | None = None

    def _port_map(self) -> dict[str, type[Record]]:
        return {**{copy.port: copy.record for copy in self._peer_copies()}, **self._ports}

    def _outputs_for(self, target: Any, index: int) -> list[dict[str, Any]]:
        if self._peer is None:
            return self._dyn_outputs
        peer = {"target": _namespace(self._peer), "link": self._peer._name}
        copies = [
            {"port": copy.port, "peer": {**peer, "port": copy.source}}
            for copy in self._peer_copies()
        ]
        return copies + self._dyn_outputs

    def _port(self, source: OutPort[T], name: str, target: Any) -> OutPort[T]:
        for copy in self._peer_copies():
            if copy.target is not source.target:
                continue
            same_port = copy.source == source.name
            same_msg = (
                source.kind == "msg"
                and source.record is not None
                and copy.record._name == source.record._name
            )
            if same_port or same_msg:
                return OutPort(name, copy.port, copy.record, target)
        raise ConfigError(f"`{name}` does not copy `{source.name}`")

    def _peer_copies(self) -> list[Copy]:
        """One copy per published port this subscriber takes."""
        if self._copies is None:
            self._copies = _copies(self._peer, self._items, self._records) if self._peer else []
        return self._copies


@dataclass(frozen=True)
class Copy:
    """One output copied from a peer: its name here, its record, and the
    published port it comes from."""

    port: str
    record: type[Record]
    source: str
    target: Any


def _copies(peer: SystemHandle, items: Sequence[Item], records: Sequence[type[Record]]) -> list[Copy]:
    published = _published(peer)
    if items:
        wanted = _item_ports(items)
    elif records:
        wanted = []
    else:
        wanted = published
    copies: list[Copy] = []
    for port in wanted:
        if not any(p.ref == port.ref and p.target is port.target for p in published):
            raise ConfigError(f"`{peer._name}` on `{_namespace(peer)}` does not publish `{port.name}`")
        record = port.record
        if record is None:
            raise ConfigError(f"`{port.name}` carries no record class")
        taken = any(
            copy.port == port.name or (record._kind == "msg" and copy.record._name == record._name)
            for copy in copies
        )
        if not taken:
            copies.append(Copy(port.name, record, port.name, port.target))
    return copies


def _published(peer: SystemHandle) -> list[OutPort[Any]]:
    """Every port the peer link publishes, after `all` expansion."""
    system = peer._system
    if not isinstance(system, Publish):
        return []
    inputs = system._inputs_for(peer._target, peer._target.index(peer._name))
    return [sources[0] for sources in inputs.values() if isinstance(sources[0], OutPort)]


def _peer(peer: SystemHandle | None) -> SystemHandle | None:
    if peer is None:
        return None
    if not isinstance(peer, SystemHandle) or not isinstance(peer._system, Link):
        raise ConfigError(f"`peer` takes a link's handle, got {peer!r}")
    return peer


def _namespace(peer: SystemHandle) -> str:
    namespace = peer._target.namespace
    if not namespace:
        raise ConfigError(f"the target of peer `{peer._name}` names no namespace")
    return namespace


def _transport(
    listen: str | None,
    connect: str | None,
    peer: SystemHandle | None,
    max_connections: int | None,
) -> dict[str, Any]:
    given = [x is not None for x in (listen, connect, peer)]
    if given.count(True) != 1:
        raise ConfigError("a link takes exactly one of `listen`, `connect`, and `peer`")
    if listen is None:
        if max_connections is not None:
            raise ConfigError("a dialing link holds one connection, so it takes no `max_connections`")
        return {"connect": {"addr": connect}} if connect is not None else {}
    slots = MAX_CONNECTIONS if max_connections is None else max_connections
    if slots < 1:
        raise ConfigError("a listening link takes at least one connection")
    return {"listen": {"addr": listen, "max_connections": slots}}


class DlSlot(System):
    """One place in the schedule that loads a pack system on its channel's `Load`.

    ``allow`` lists the pack systems it may load, each named by its type; every
    one takes the first's ports. ``initial`` names the one loaded at boot.
    Inputs are wired on the slot, as keywords of the first system's inputs.
    The slot needs ``channel=`` on `Target.add`.
    """

    _ty = "dl_slot"
    _pack = PACK

    def __init__(
        self, allow: Sequence[System], initial: str | None = None, **inputs: Sources[Any]
    ) -> None:
        if not allow:
            raise ConfigError("a slot allows at least one system")
        names = _unique([system._ty for system in allow], "a slot allows")
        if initial is not None and initial not in names:
            raise ConfigError(f"`initial` names `{initial}`, which `allow` does not list")
        declared = allow[0]._inputs
        for port in inputs:
            if port not in declared:
                raise ConfigError(f"`{names[0]}` has no input `{port}`")
        candidates = [_candidate(system) for system in allow]
        super().__init__(inputs, {"allow": candidates, "initial": initial})
        self._ports = dict(type(allow[0])._ports)
        self._names = names


def _candidate(system: System) -> dict[str, Any]:
    """One `allow` entry: the system's type, params, and the pack `metor build` opens."""
    if not isinstance(system, System) or isinstance(system, (Link, DlSlot)):
        raise ConfigError(f"a slot allows pack systems, got {system!r}")
    check_abi(system._pack.id, system._pack.abi_version)
    if any(system._inputs.values()):
        raise ConfigError(f"`{system._ty}` in a slot takes its inputs on the slot")
    return {
        "name": system._ty,
        "ty": system._ty,
        "params": _json(system._params) or None,
        "source": {"pack": system._pack.to_json()},
    }


class ChannelRegistry(System):
    """Publishes every channel of its target, with the names each slot may
    load, on its first cycle and then once a second."""

    _ty = "channel_registry"
    _pack = PACK
    _ports = {"registry": SequenceRegistry, **FRAMEWORK_PORTS}

    def __init__(self) -> None:
        super().__init__({}, {})

    def _params_for(self, target: Any, index: int) -> Any:
        channels = [
            {"name": name, "available": system._names if isinstance(system, DlSlot) else []}
            for name, system in target._channels()
        ]
        return {"channels": channels, "period": max(1, round(target.cycle_rate))}

    registry: OutPort[SequenceRegistry]
