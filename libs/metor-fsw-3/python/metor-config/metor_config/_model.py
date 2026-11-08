"""Records, ports, and the `System` base class that pack modules build on."""

from __future__ import annotations

import dataclasses
import os
import sys
from dataclasses import dataclass
from typing import (
    TYPE_CHECKING,
    Any,
    Callable,
    ClassVar,
    Generic,
    Iterable,
    Mapping,
    Sequence,
    TypeAlias,
    TypeVar,
    Union,
)

from ._config import ConfigError
from ._fields import Field, index, signature

T = TypeVar("T", bound="Record")


class Record:
    """Base class for record types.

    A record is a type of data that flows between systems. Each pack module
    declares one `Record` subclass per record it uses. ``_name`` is the
    record's name on the host, ``_kind`` is ``"frame"`` or ``"msg"``, and
    ``_pack`` is the pack that defines it. A frame also sets ``_fields``,
    each leaf's path and signature.
    """

    _name: str = ""
    _kind: str = "msg"
    _pack: Pack | None = None
    _fields: dict[str, str] = {}
    _mapping: dict[str, Field[Any]]
    _port: ClassVar[type[OutPort[Any]]]
    """The class of a port that carries this record."""

    def __init__(self, **fields: Field[Any] | Mapping[int, Field[Any]]) -> None:
        """Maps each destination path to a source `Field`, for a `Mux` output.

        A `Mapping` from index to `Field` maps elements of the destination.
        Raises `ConfigError` for a message record, an unknown path, a value
        that is not a `Field`, a signature that differs from the source's, or
        two destinations that overlap.
        """
        cls = type(self)
        if cls._kind != "frame":
            raise ConfigError(f"`{cls.__name__}` is a message; only a frame maps fields")
        pairs = [pair for path, value in fields.items() for pair in _expand(cls, path, value)]
        for path, source in pairs:
            _check_mapping(cls, path, source)
        _check_overlap(cls, [path for path, _ in pairs])
        self._mapping = dict(pairs)


def _expand(record: type[Record], path: str, value: Any) -> list[tuple[str, Any]]:
    """``value`` by destination path; a `Mapping` gives one path per index."""
    if not isinstance(value, Mapping):
        return [(path, value)]
    where = f"{record.__name__}.{path}"
    return [(f"{path}.{index(i, where)}", source) for i, source in value.items()]


def _check_overlap(record: type[Record], paths: list[str]) -> None:
    for i, a in enumerate(paths):
        for b in paths[i + 1 :]:
            if a == b or b.startswith(f"{a}.") or a.startswith(f"{b}."):
                raise ConfigError(f"`{record.__name__}` maps `{a}` and `{b}`")


def _check_mapping(record: type[Record], path: str, source: Any) -> None:
    if not isinstance(source, Field):
        raise ConfigError(f"`{record.__name__}.{path}` takes a port field, got {source!r}")
    expected = signature(record._fields, path)
    if expected is None:
        raise ConfigError(f"`{record.__name__}` has no field `{path}`")
    if expected != source.signature:
        raise ConfigError(
            f"`{record.__name__}.{path}` is `{expected}`, but "
            f"`{source.port.name}.{source.path}` is `{source.signature}`"
        )


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

    def field(self, path: str) -> Field[Any]:
        """Returns the leaf, or group of leaves, at ``path`` in this port's frame.

        Raises `ConfigError` for a message port or an unknown path.
        """
        if self.record is None or self.record._kind != "frame":
            raise ConfigError(f"`{self.name}` carries a message; only a frame has fields")
        found = signature(self.record._fields, path)
        if found is None:
            raise ConfigError(f"`{self.name}` has no field `{path}`")
        return Field(self, path, found)


Record._port = OutPort


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
    platform triple. ``wasm`` says whether `metor pack dev` also built the
    pack as a WebAssembly module.
    """

    id: str
    lib: str
    libs: str
    abi_version: int
    wasm: bool = False

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

    A port named ``fss.0`` is element 0 of the list ``fss``, and
    ``wheels.0.cmd`` is the ``cmd`` port of the first `PortGroup` in the list
    ``wheels``. A subclass whose inputs sit in lists or groups sets
    `_input_ports`, every input by its dotted name, and takes one keyword per
    list or group. Reading a list of outputs from a handle gives a tuple, and
    a group gives an `OutGroup`.

    Some system types take a list of ports from their config instead of
    declaring them. Those set `_takes_inputs` or `_takes_outputs`, and pass
    `items` or `records` through.
    """

    _pack: Pack
    _ty: str
    _ports: dict[str, type[Record]] = {}
    _input_ports: dict[str, type[Record]] = {}
    """Every input by its dotted name, when any sits in a list or group."""
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
        inputs: dict[str, Any],
        params: dict[str, Any],
        outputs: list[dict[str, Any]] | None = None,
        items: Sequence[Item] = (),
        records: Sequence[type[Record]] = (),
        grants: dict[str, Any] | None = None,
    ) -> None:
        self._location = _location(self)
        self._grants = dict(grants or {})
        flat = _flatten_inputs(type(self)._input_ports, inputs)
        self._inputs = {port: _sources(port, src) for port, src in flat.items()}
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

    def _grants_for(self, target: Any, index: int) -> dict[str, Any]:
        """Returns the resources this registration is granted, by parameter."""
        return self._grants

    def _bind(self, target: Any, name: str) -> None:
        """Creates the resources this system holds on ``target``, once it is
        added as ``name``."""


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


def _children(names: Iterable[str], path: str) -> list[str]:
    """The distinct segments right under ``path`` in ``names``, in order."""
    prefix = path + "."
    found: dict[str, None] = {}
    for name in names:
        if name.startswith(prefix):
            found.setdefault(name[len(prefix) :].split(".", 1)[0])
    return list(found)


def _is_index_run(children: list[str]) -> bool:
    return children == [str(i) for i in range(len(children))]


def _flatten_inputs(ports: dict[str, type[Record]], given: dict[str, Any]) -> dict[str, Any]:
    """Maps each keyword's value to the dotted inputs under it."""
    flat: dict[str, Any] = {}
    for name, value in given.items():
        _flatten_input(ports, name, value, flat)
    return flat


def _flatten_input(
    ports: dict[str, type[Record]], path: str, value: Any, flat: dict[str, Any]
) -> None:
    children = _children(ports, path)
    if value is None:
        return
    if not children:
        flat[path] = value
    elif _is_index_run(children):
        for i, item in enumerate(_list_of(path, value, len(children))):
            _flatten_input(ports, f"{path}.{i}", item, flat)
    elif isinstance(value, PortGroup):
        for child, item in value._flat.items():
            if f"{path}.{child}" not in ports:
                group = type(value).__qualname__
                raise ConfigError(f"input `{path}` takes another group than `{group}`")
            flat[f"{path}.{child}"] = item
    else:
        raise ConfigError(f"input `{path}` takes a port group, got {value!r}")


def _list_of(path: str, value: Any, length: int) -> Sequence[Any]:
    """Returns ``value`` if it is a list of ``length``."""
    if isinstance(value, (OutPort, Loop, str)) or not isinstance(value, Sequence):
        raise ConfigError(f"input `{path}` takes a list of {length}, got {value!r}")
    if len(value) != length:
        raise ConfigError(f"input `{path}` takes a list of {length}, got {len(value)}")
    return value


def _out_node(ports: dict[str, type[Record]], path: str, leaf: Callable[[str], Any]) -> Any:
    """Returns the output at ``path``: ``leaf(path)`` for a port, or a tuple
    for a list. Returns ``None`` when nothing sits at ``path``."""
    if path in ports:
        return leaf(path)
    children = _children(ports, path)
    if not children:
        return None
    nodes = {child: _out_node(ports, f"{path}.{child}", leaf) for child in children}
    if _is_index_run(children):
        return tuple(nodes.values())
    return OutGroup(path, nodes)


class PortGroup:
    """A group of a system's inputs, passed as one keyword.

    A pack module generates one subclass per group, nested on the system's
    class. `_input_ports` maps each input under the group, by its dotted path
    from the group, to its record. The constructor raises `ConfigError` on an
    unknown keyword or a source whose record differs from its input's.
    """

    _input_ports: ClassVar[dict[str, type[Record]]] = {}

    def __init__(self, **children: Any) -> None:
        cls = type(self)
        known = {name.split(".", 1)[0] for name in cls._input_ports}
        for name in children:
            if name not in known:
                raise ConfigError(f"`{cls.__qualname__}` has no input `{name}`")
        self._flat = _flatten_inputs(cls._input_ports, children)
        for path, value in self._flat.items():
            _check_records(cls, path, _sources(path, value))


def _check_records(group: type[PortGroup], path: str, sources: list[Source[Any]]) -> None:
    expected = group._input_ports[path]
    for source in sources:
        got = source.record
        if got is not None and got._name != expected._name:
            raise ConfigError(
                f"`{group.__qualname__}.{path}` takes `{expected.__name__}`, got `{got.__name__}`"
            )


class OutGroup:
    """A group of a system's outputs, read from its handle.

    Each output under the group is an attribute. A pack module generates one
    subclass per group to type them.
    """

    def __init__(self, path: str, children: dict[str, Any]) -> None:
        self._path = path
        self._children = children

    if not TYPE_CHECKING:
        # Hidden from type checkers, so a subclass names only its outputs.
        def __getattr__(self, name: str) -> Any:
            children = self.__dict__.get("_children", {})
            if name not in children:
                raise AttributeError(f"output group `{self._path}` has no output `{name}`")
            return children[name]

    def __repr__(self) -> str:
        return f"OutGroup({self._path!r}, {list(self._children)})"


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
