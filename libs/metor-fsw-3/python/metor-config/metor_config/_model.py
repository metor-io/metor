"""Records, ports, and the `System` base a generated pack module subclasses."""

from __future__ import annotations

import dataclasses
from dataclasses import dataclass
from typing import Any, Generic, Sequence, TypeAlias, TypeVar, Union

from ._config import ConfigError

T = TypeVar("T", bound="Record")


class Record:
    """Marker base of a record type; a pack module declares one class per record name.

    ``_name`` names the record on the host, ``_kind`` says whether it is a
    ``"frame"`` or a ``"msg"``, and ``_pack`` supplies its descriptor.
    """

    _name: str = ""
    _kind: str = "msg"
    _pack: Pack | None = None


class LogEvent(Record):
    """The log every system writes."""

    _name = "log"
    _kind = "msg"


class SystemStatus(Record):
    """The status the coordinator writes for every system."""

    _name = "status"
    _kind = "frame"


FRAMEWORK_PORTS: dict[str, type[Record]] = {"log": LogEvent, "status": SystemStatus}
"""The outputs every system has whatever it declared."""


class SequenceCommand(Record):
    """A command addressed to a channel by name."""

    _name = "sequence_command"
    _kind = "msg"


class SequenceChannelEvent(Record):
    """What a channel did with a command, or how its system ended."""

    _name = "sequence_channel_event"
    _kind = "msg"


class ChannelStatus(Record):
    """A channel's state, written every cycle."""

    _name = "channel_status"
    _kind = "frame"


class SequenceRegistry(Record):
    """Every channel of a target and the names a `Load` on each may name."""

    _name = "sequence_registry"
    _kind = "msg"


CHANNEL_PORTS: dict[str, type[Record]] = {
    "events": SequenceChannelEvent,
    "channel_status": ChannelStatus,
}
"""The outputs a channel adds to the system it wraps."""


@dataclass(frozen=True)
class PortRef:
    """One end of an edge: an output port of a named system."""

    system: str
    port: str

    def to_json(self) -> dict[str, str]:
        return {"system": self.system, "port": self.port}


class OutPort(Generic[T]):
    """An output port carrying record ``T``, read off a handle returned by `Target.add`.

    ``record`` is the port's record class and ``target`` the `Target` it is on,
    which a deployment needs to tell two targets' `plant.imu` apart.
    """

    def __init__(
        self, system: str, port: str, record: type[Record] | None = None, target: Any = None
    ) -> None:
        self.ref = PortRef(system, port)
        self.record = record
        self.target = target

    @property
    def kind(self) -> str:
        return self.record._kind if self.record is not None else "msg"

    @property
    def name(self) -> str:
        """`{system}.{port}`, the name a port list gives this port."""
        return f"{self.ref.system}.{self.ref.port}"


class Loop(Generic[T]):
    """A one-cycle delayed edge carrying record ``T``.

    `Target.loop` creates it before its producer exists; `connect` names the
    producer once.
    """

    def __init__(self, record: type[T]) -> None:
        self.record = record
        self.producer: OutPort[T] | None = None

    def connect(self, port: OutPort[T]) -> None:
        if self.producer is not None:
            raise ConfigError(
                f"loop of `{self.record.__name__}` is already connected to "
                f"`{self.producer.ref.system}.{self.producer.ref.port}`"
            )
        self.producer = port


Source: TypeAlias = Union[OutPort[T], Loop[T]]
"""What an input keyword takes: a producer's port or a loop."""

Sources: TypeAlias = Union[Source[T], Sequence[Source[T]]]
"""One source or, for fan-in, a sequence of them in edge order."""

Item: TypeAlias = Union["SystemHandle", OutPort[Any]]
"""What a port list takes: a system's whole handle, or one of its ports."""


@dataclass(frozen=True)
class Pack:
    """A pack as its generated module declares it: the id prefixing every `ty`,
    the cdylib stem, and the per-triple library directory the module locates."""

    id: str
    lib: str
    libs: str
    abi_version: int

    def to_json(self) -> dict[str, Any]:
        return {"id": self.id, "lib": self.lib, "libs": str(self.libs)}


class System:
    """Base of a generated pack-entry class.

    A generated module sets three class attributes and forwards its keywords,
    inputs first, params second::

        class Nav(System):
            _pack = PACK
            _ty = "nav"
            _ports = {"est": Est, "log": LogEvent, "status": SystemStatus}

            def __init__(self, *, imu: Sources[Imu] = (), gain: float = 0.1) -> None:
                super().__init__({"imu": imu}, {"gain": gain})

            est: OutPort[Est]
            log: OutPort[LogEvent]
            status: OutPort[SystemStatus]

    `_ty` is the pack's table key, `_ports` every output with its record.

    A type that takes the ports its config lists sets `_takes_inputs` or
    `_takes_outputs`, and forwards `items` or `records`.
    """

    _pack: Pack
    _ty: str
    _ports: dict[str, type[Record]] = {}
    _record_packs: tuple[Pack, ...] = ()
    _takes_inputs: bool = False
    """Whether this type takes an `items` port list of its own."""
    _takes_outputs: bool = False
    """Whether this type takes a `records` list, one output port each."""
    _name: str = ""
    """The instance name `Target.add` registered this system under."""
    _target: Any = None
    """The `Target` this system was added to."""

    def __init__(
        self,
        inputs: dict[str, Sources[Any]],
        params: dict[str, Any],
        outputs: list[dict[str, Any]] | None = None,
        items: Sequence[Item] = (),
        records: Sequence[type[Record]] = (),
    ) -> None:
        self._inputs = {port: _sources(port, src) for port, src in inputs.items()}
        self._inputs.update(_edges(_item_ports(items)))
        self._params = {name: _json(value) for name, value in params.items()}
        names = _record_names(records)
        self._dyn_outputs = list(outputs or [])
        self._dyn_outputs += [{"port": name, "record": name} for name in names]
        self._ports = {**dict(zip(names, records)), **type(self)._ports}
        self._record_packs = tuple(
            record._pack for record in records if record._pack is not None
        )

    def _port_map(self) -> dict[str, type[Record]]:
        """Every output this instance has, with its record."""
        return self._ports

    def _inputs_for(self, target: Any, index: int) -> dict[str, list[Source[Any]]]:
        """Resolve inputs for this registration without changing the system."""
        return self._inputs

    def _outputs_for(self, target: Any, index: int) -> list[dict[str, Any]]:
        """The `outputs` config entries for this registration."""
        return self._dyn_outputs

    def _params_for(self, target: Any, index: int) -> Any:
        """The `params` config entry for this registration."""
        return self._params or None

    def port(self, source: OutPort[T]) -> OutPort[T]:
        """This system's copy of ``source``, on a system that copies ports."""
        return self._port(source, self._name, self._target)

    def _port(self, source: OutPort[T], name: str, target: Any) -> OutPort[T]:
        raise ConfigError(f"`{name}` copies no ports; only a Subscribe with a peer does")


def _item_ports(items: Sequence[Item]) -> list[OutPort[Any]]:
    """Every port the items name, a handle standing for all of its own."""
    from ._target import SystemHandle

    ports: list[OutPort[Any]] = []
    for item in items:
        if isinstance(item, SystemHandle):
            ports.extend(getattr(item, port) for port in item._port_map())
        elif isinstance(item, OutPort):
            ports.append(item)
        else:
            raise ConfigError(f"a port list takes handles and ports, got {item!r}")
    return ports


def _edges(ports: Sequence[OutPort[Any]]) -> dict[str, list[Source[Any]]]:
    """One input per port, named `{producer}.{port}`, first mention winning."""
    edges: dict[str, list[Source[Any]]] = {}
    for port in ports:
        edges.setdefault(port.name, [port])
    return edges


def _record_names(records: Sequence[type[Record]]) -> list[str]:
    """One port name per record, each named once."""
    names = [_record_name(record) for record in records]
    for name in names:
        if names.count(name) > 1:
            raise ConfigError(f"a system takes record `{name}` twice")
    return names


def _record_name(record: type[Record]) -> str:
    if not isinstance(record, type) or not issubclass(record, Record):
        raise ConfigError(f"a record list takes record classes, got {record!r}")
    if not record._name:
        raise ConfigError(f"record class `{record.__name__}` names no record")
    return record._name


def _sources(port: str, sources: Sources[Any]) -> list[Source[Any]]:
    if isinstance(sources, (OutPort, Loop)):
        return [sources]
    listed = list(sources)
    for source in listed:
        if not isinstance(source, (OutPort, Loop)):
            raise ConfigError(f"input `{port}` takes ports and loops, got {source!r}")
    return listed


def _json(value: Any) -> Any:
    """Params as JSON: nested param dataclasses become objects, sequences arrays."""
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return {f.name: _json(getattr(value, f.name)) for f in dataclasses.fields(value)}
    if isinstance(value, dict):
        return {key: _json(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json(item) for item in value]
    return value
