"""Built-in systems that any target can add.

`Publish` sends records to other targets, and `Subscribe` receives them. `Db`
is an embedded database, which is how a gateway records telemetry. `DlSlot`
and `ChannelRegistry` support channels.

These systems are built into the host rather than supplied by a pack, so
their pack has no library and is left out of the config's pack list.

`Publish` and `Subscribe` each take one `transport`: `Listen` on an
address, `Connect` to an address, or dial a `Peer` link in the same
deployment, which `metor build` turns into an address. `Target.add` makes
the transport a resource named ``{name}_ep`` and grants it to the link's
`ep`. A `Db` only listens.
"""

from __future__ import annotations

import ipaddress
import socket
from dataclasses import dataclass
from typing import Any, Sequence, TypeVar

from . import _generated
from ._config import ConfigError
from ._generated import IO_RECORDS, SequenceRegistry
from ._io import Kind, PeerDial, TcpDial, TcpListen
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
from ._target import SystemHandle, Target, _targets

T = TypeVar("T", bound=Record)

MAX_CONNECTIONS = 8
CONN_CAP = 1 << 20


@dataclass(frozen=True)
class Listen:
    """Listen on ``addr`` and serve up to ``max_connections`` peers at once,
    taking peers from the ``allow`` CIDR blocks, or any peer when ``allow``
    is empty."""

    addr: str
    max_connections: int = MAX_CONNECTIONS
    allow: Sequence[str] = ()

    def _kind(self) -> Kind:
        return TcpListen(self.addr, tuple(self.allow))


@dataclass(frozen=True)
class Connect:
    """Dial ``addr`` and keep one connection open. A host name resolves
    when the config is built."""

    addr: str

    def _kind(self) -> Kind:
        return TcpDial(_resolve(self.addr))


@dataclass(frozen=True)
class Peer:
    """Dial ``link``, a link on another target in the same deployment."""

    link: SystemHandle

    def __post_init__(self) -> None:
        if not isinstance(self.link, SystemHandle):
            raise ConfigError(f"`Peer` takes a link's handle, got {self.link!r}")

    def _kind(self) -> Kind:
        return PeerDial(_namespace(self.link), self.link._name)


Transport = Listen | Connect | Peer
"""How a link gets its connections."""


def _resolve(addr: str) -> str:
    """Returns ``addr`` with its host resolved to an IP address."""
    host, sep, port = addr.rpartition(":")
    if not sep or not port.isdigit():
        raise ConfigError(f"`{addr}` is not a `host:port` address")
    bracketed = host.startswith("[") and host.endswith("]")
    host = host.removeprefix("[").removesuffix("]")
    if ":" in host and not bracketed:
        raise ConfigError(f"`{addr}` names an IPv6 host without brackets, as `[::1]:2240`")
    try:
        ipaddress.ip_address(host)
        return addr
    except ValueError:
        pass
    try:
        found = socket.getaddrinfo(host, int(port), type=socket.SOCK_STREAM)
    except OSError as e:
        raise ConfigError(f"`{addr}` does not resolve: {e}") from None
    family, ip = found[0][0], found[0][4][0]
    return f"[{ip}]:{port}" if family == socket.AF_INET6 else f"{ip}:{port}"


class Link(System):
    """Base class for systems that send or receive records over a network
    connection.

    Peers identify a link by its target's namespace and its own name.
    """

    _caps = ("ep",)

    def __init__(
        self,
        params: dict[str, Any],
        transport: Transport,
        items: Sequence[Item] = (),
        records: Sequence[type[Record]] = (),
    ) -> None:
        if not isinstance(transport, (Listen, Connect, Peer)):
            raise ConfigError(
                f"a link's transport is a `Listen`, `Connect`, or `Peer`, got {transport!r}"
            )
        self._transport = transport
        super().__init__({}, params, items=items, records=records)

    @property
    def _peer(self) -> SystemHandle | None:
        return self._transport.link if isinstance(self._transport, Peer) else None

    def _bind(self, target: Target, name: str) -> None:
        if not name.isidentifier():
            raise ConfigError(f"a link's name is an identifier, got {name!r}")
        target.resource(_endpoint(name), self._transport._kind())

    def _grants_for(self, target: Target, index: int) -> dict[str, Any]:
        return {"ep": target._resources[_endpoint(target._systems[index].name)]}

    def _params_for(self, target: Any, index: int) -> Any:
        params = {
            "namespace": target.namespace,
            "link": target._systems[index].name,
            **self._params,
        }
        if isinstance(self._transport, Listen):
            params["max_connections"] = self._transport.max_connections
        else:
            params["dialing"] = True
        return params


def _endpoint(link: str) -> str:
    """The name of the resource link ``link`` is granted as its `ep`."""
    return f"{link}_ep"


class _Sink(System):
    """A system that reads the ports listed in `items`.

    With `all`, it also reads every port of every system added before it.
    """

    _items: list[Item] = []
    _all: bool = False

    def _sink(self, items: Sequence[Item], all: bool) -> None:
        self._items = list(items)
        self._all = all

    def _inputs_for(self, target: Target, index: int) -> dict[str, list[Source[Any]]]:
        if not self._all:
            return self._inputs
        earlier = [target._handles[entry.name] for entry in target._systems[:index]]
        ports = [p for p in _item_ports(earlier) if p.record is None or p.record._name not in IO_RECORDS]
        return _edges(_item_ports(self._items) + ports)


class Publish(_Sink, Link, _generated.Publish):
    """Sends records to every connected peer.

    `items` lists what to publish. An item is either a system handle, which
    publishes all of that system's ports, or a single port. With `all`, it
    publishes every system added before this one.
    """

    def __init__(
        self,
        items: Sequence[Item] = (),
        *,
        transport: Transport,
        conn_cap: int = CONN_CAP,
        all: bool = False,
    ) -> None:
        super().__init__({"conn_cap": conn_cap}, transport, items=items)
        self._sink(items, all)


class Db(_Sink, _generated.Db):
    """An embedded database that records telemetry.

    It stores every record from the ports in `items`, and serves the database
    to clients on `listen`. `path` is the database directory. When unset, a
    fresh temporary directory is used. With `all`, it records every system
    added before this one.

    Its commands are the message records of every `Subscribe` whose `peer` is
    this db. They are collected when the config is emitted.
    """

    def __init__(
        self,
        listen: str,
        path: str | None = None,
        items: Sequence[Item] = (),
        all: bool = False,
    ) -> None:
        super().__init__({}, {}, items=items)
        self._sink(items, all)
        self._listen = listen
        self._path = path

    def _params_for(self, target: Any, index: int) -> Any:
        return {
            "listen": self._listen,
            "path": self._path,
            "namespace": target.namespace,
            "commands": self._commands(),
        }

    def _commands(self) -> list[str]:
        """Returns the message record names that subscribers of this db take."""
        names: set[str] = set()
        for target in _targets:
            for entry in target._systems:
                system = entry.system
                if not isinstance(system, Subscribe) or system._peer is None:
                    continue
                if system._peer._system is not self:
                    continue
                records = [copy.record for copy in system._peer_copies()] + system._records
                names.update(record._name for record in records if record._kind == "msg")
        return sorted(names)


class Subscribe(Link, _generated.Subscribe):
    """Receives records from peers and writes them to its own output ports.

    `records` lists message classes to accept. Each one gets an output port
    named after the record.

    With a `Peer` transport, the subscriber copies ports that the peer
    `Publish` serves. `items` lists which of the peer's ports to copy, and each copy
    keeps its `{system}.{port}` name. A message record gets only one port,
    even if the peer publishes it on several; the first one listed wins.
    With neither `items` nor `records`, it copies everything the peer
    publishes. Use `port()` to get this subscriber's copy of a peer's port.
    """

    def __init__(
        self,
        records: Sequence[type[Record]] = (),
        *,
        transport: Transport,
        items: Sequence[Item] = (),
        conn_cap: int = CONN_CAP,
    ) -> None:
        if items and not isinstance(transport, Peer):
            raise ConfigError("`items` names a peer's ports, so it needs a `Peer` transport")
        super().__init__({"conn_cap": conn_cap}, transport, records=records)
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

    def port(self, source: OutPort[T]) -> OutPort[T]:
        """Returns this subscriber's copy of ``source``, a port of its peer's
        target."""
        return self._port(source, self._name, self._target)

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
        """Returns one copy for each of the peer's ports this subscriber
        takes."""
        if self._copies is None:
            self._copies = _copies(self._peer, self._items, self._records) if self._peer else []
        return self._copies


@dataclass(frozen=True)
class Copy:
    """An output port that copies one of a peer's published ports.

    ``port`` is the name on this subscriber, ``record`` is its record class,
    and ``source`` is the name of the peer's port it copies.
    """

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
    """Returns every port the peer link publishes, including those added by
    `all`."""
    system = peer._system
    if not isinstance(system, Publish):
        return []
    inputs = system._inputs_for(peer._target, peer._target.index(peer._name))
    return [sources[0] for sources in inputs.values() if isinstance(sources[0], OutPort)]


def _namespace(peer: SystemHandle) -> str:
    namespace = peer._target.namespace
    if not namespace:
        raise ConfigError(f"the target of peer `{peer._name}` names no namespace")
    return namespace


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
    if not isinstance(system, System) or isinstance(system, (Link, Db, DlSlot)):
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
