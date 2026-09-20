"""Records, ports, and the `System` base a generated pack module subclasses."""

from __future__ import annotations

import dataclasses
from dataclasses import dataclass
from typing import Any, Generic, Sequence, TypeAlias, TypeVar, Union

from ._config import ConfigError

T = TypeVar("T", bound="Record")


class Record:
    """Marker base of a record type; a pack module declares one class per record name.

    ``_name`` names the record on the host. ``_pack`` supplies its descriptor.
    """

    _name: str = ""
    _pack: Pack | None = None


@dataclass(frozen=True)
class PortRef:
    """One end of an edge: an output port of a named system."""

    system: str
    port: str

    def to_json(self) -> dict[str, str]:
        return {"system": self.system, "port": self.port}


class OutPort(Generic[T]):
    """An output port carrying record ``T``, read off a handle returned by `Target.add`."""

    def __init__(self, system: str, port: str) -> None:
        self.ref = PortRef(system, port)


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
            _outputs = ("est",)

            def __init__(self, *, imu: Sources[Imu] = (), gain: float = 0.1) -> None:
                super().__init__({"imu": imu}, {"gain": gain})

            est: OutPort[Est]
            log: OutPort[LogEvent]
            status: OutPort[SystemStatus]

    `_ty` is the pack's table key, `_outputs` the ports a handle resolves
    (`log` and `status` are outputs of every system and need not be listed).
    """

    _pack: Pack
    _ty: str
    _outputs: tuple[str, ...] = ()
    _record_packs: tuple[Pack, ...] = ()
    _wants_target: bool = False
    """Whether `Target.to_config` fills this system's `namespace` and `link` params."""

    def __init__(
        self,
        inputs: dict[str, Sources[Any]],
        params: dict[str, Any],
        outputs: list[dict[str, str]] | None = None,
    ) -> None:
        self._inputs = {port: _sources(port, src) for port, src in inputs.items()}
        self._params = {name: _json(value) for name, value in params.items()}
        self._dyn_outputs = list(outputs or [])

    def _inputs_for(self, target: Any, index: int) -> dict[str, list[Source[Any]]]:
        """Resolve inputs for this registration without changing the system."""
        return self._inputs


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
