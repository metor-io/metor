"""Records, ports, and the `System` base class that pack modules build on."""

from __future__ import annotations

import dataclasses
import os
import sys
from dataclasses import dataclass
from typing import Any, Generic, Sequence, TypeAlias, TypeVar, Union

from ._config import ConfigError

T = TypeVar("T", bound="Record")


class Record:
    """Base class for record types.

    A record is a type of data that flows between systems. Each pack module
    declares one `Record` subclass per record it uses. ``_name`` is the
    record's name on the host, ``_kind`` is ``"frame"`` or ``"msg"``, and
    ``_pack`` is the pack that defines it.
    """

    _name: str = ""
    _kind: str = "msg"
    _pack: Pack | None = None


@dataclass(frozen=True)
class PortRef:
    """A reference to one output port of a system, by name."""

    system: str
    port: str

    def to_json(self) -> dict[str, str]:
        return {"system": self.system, "port": self.port}


class OutPort(Generic[T]):
    """An output port that carries record ``T``.

    Get one as an attribute of the handle `Target.add` returns, then pass it
    to another system's input. ``record`` is the port's record class.
    ``target`` is the `Target` the port belongs to, so that a deployment can
    tell ``plant.imu`` on one target from ``plant.imu`` on another.
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
        """The port's name in a port list: `{system}.{port}`."""
        return f"{self.ref.system}.{self.ref.port}"


class Loop(Generic[T]):
    """A feedback edge that carries record ``T`` with a one-cycle delay.

    A loop lets a system read from a producer that is added after it. Create
    one with `Target.loop`, pass it as an input, and call `connect` once the
    producer exists.
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
"""A single input source: an output port, or a loop."""

Sources: TypeAlias = Union[Source[T], Sequence[Source[T]]]
"""One source, or a sequence of sources for fan-in, in edge order."""

Item: TypeAlias = Union["SystemHandle", OutPort[Any]]
"""An entry in a port list: a system handle, which stands for all of its
ports, or a single port."""


@dataclass(frozen=True)
class Pack:
    """A pack, as declared by its generated module.

    ``id`` is the prefix of every system type in the pack. ``lib`` is the
    cdylib's file stem, and ``libs`` is the directory holding one library per
    platform triple.
    """

    id: str
    lib: str
    libs: str
    abi_version: int

    def to_json(self) -> dict[str, Any]:
        return {"id": self.id, "lib": self.lib, "libs": str(self.libs)}


class System:
    """Base class for the system classes in a generated pack module.

    Each subclass sets three class attributes and passes its keyword
    arguments to ``super().__init__``, inputs first, then params::

        class Nav(System):
            _pack = PACK
            _ty = "nav"
            _ports = {"est": Est, "log": LogEvent, "status": SystemStatus}

            def __init__(self, *, imu: Sources[Imu] = (), gain: float = 0.1) -> None:
                super().__init__({"imu": imu}, {"gain": gain})

            est: OutPort[Est]
            log: OutPort[LogEvent]
            status: OutPort[SystemStatus]

    `_ty` is the system's type name within the pack, and `_ports` maps every
    output to its record. `_caps` names each `Cap` parameter; the subclass
    passes the granted resources as ``grants``.

    Some system types take a list of ports from their config instead of
    declaring them. Those set `_takes_inputs` or `_takes_outputs`, and pass
    `items` or `records` through.
    """

    _pack: Pack
    _ty: str
    _ports: dict[str, type[Record]] = {}
    _caps: tuple[str, ...] = ()
    """The name of each `Cap` parameter."""
    _record_packs: tuple[Pack, ...] = ()
    _takes_inputs: bool = False
    """Whether this type takes an `items` list of input ports."""
    _takes_outputs: bool = False
    """Whether this type takes a `records` list, with one output port per
    record."""
    _name: str = ""
    """The name this system was given in `Target.add`."""
    _target: Any = None
    """The `Target` this system was added to."""
    _location: dict[str, Any] | None = None
    """The file and line of the code that created this system."""

    def __init__(
        self,
        inputs: dict[str, Sources[Any]],
        params: dict[str, Any],
        outputs: list[dict[str, Any]] | None = None,
        items: Sequence[Item] = (),
        records: Sequence[type[Record]] = (),
        grants: dict[str, Any] | None = None,
    ) -> None:
        self._location = _location(self)
        self._grants = dict(grants or {})
        self._inputs = {port: _sources(port, src) for port, src in inputs.items()}
        self._inputs.update(_edges(_item_ports(items)))
        self._params = {name: _json(value) for name, value in params.items()}
        names = _unique([_record_name(record) for record in records], "a system takes record")
        self._dyn_outputs = list(outputs or [])
        self._dyn_outputs += [{"port": name, "record": name} for name in names]
        self._ports = {**dict(zip(names, records)), **type(self)._ports}
        self._record_packs = tuple(
            record._pack for record in records if record._pack is not None
        )

    def _port_map(self) -> dict[str, type[Record]]:
        """Returns every output of this instance, mapped to its record."""
        return self._ports

    def _inputs_for(self, target: Any, index: int) -> dict[str, list[Source[Any]]]:
        """Returns the inputs for this registration, without changing the
        system."""
        return self._inputs

    def _outputs_for(self, target: Any, index: int) -> list[dict[str, Any]]:
        """Returns the `outputs` config entries for this registration."""
        return self._dyn_outputs

    def _params_for(self, target: Any, index: int) -> Any:
        """Returns the `params` config entry for this registration."""
        return self._params or None

    def port(self, source: OutPort[T]) -> OutPort[T]:
        """Returns this system's copy of ``source``.

        Only systems that copy ports from a peer, like `Subscribe`, support
        this. Others raise `ConfigError`.
        """
        return self._port(source, self._name, self._target)

    def _port(self, source: OutPort[T], name: str, target: Any) -> OutPort[T]:
        raise ConfigError(f"`{name}` copies no ports; only a Subscribe with a peer does")


_PACKAGE = os.path.dirname(os.path.abspath(__file__))


def _location(system: System) -> dict[str, Any] | None:
    """Returns the file and line of the code that created ``system``.

    That is the first caller that is neither one of ``system``'s
    constructors nor part of this package. The file is relative to the
    working directory, as `metor run` prints it.
    """
    frame = sys._getframe(1)
    while frame is not None:
        path = os.path.abspath(frame.f_code.co_filename)
        own = frame.f_locals.get("self") is system or path.startswith(_PACKAGE + os.sep)
        if not own:
            return {"file": os.path.relpath(path), "line": frame.f_lineno}
        frame = frame.f_back
    return None


def _item_ports(items: Sequence[Item]) -> list[OutPort[Any]]:
    """Expands port list items into ports. A handle expands to all of its
    ports."""
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
    """Makes one input per port, named `{producer}.{port}`. If a port appears
    twice, the first one wins."""
    edges: dict[str, list[Source[Any]]] = {}
    for port in ports:
        edges.setdefault(port.name, [port])
    return edges


def _unique(names: list[str], what: str) -> list[str]:
    """Returns `names`, raising `ConfigError` if any name repeats."""
    for name in names:
        if names.count(name) > 1:
            raise ConfigError(f"{what} `{name}` twice")
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
    """Converts params to JSON values.

    Nested param dataclasses become objects, and sequences become arrays.
    """
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return {f.name: _json(getattr(value, f.name)) for f in dataclasses.fields(value)}
    if isinstance(value, dict):
        return {key: _json(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json(item) for item in value]
    return value
