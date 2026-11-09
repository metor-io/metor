"""Fields of a frame port, which a `Mux` maps into a new frame."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Generic, TypeVar, cast

from ._config import ConfigError

if TYPE_CHECKING:
    from ._model import OutPort

S = TypeVar("S", bound=str)
E = TypeVar("E", bound="Field[Any]")


class Field(Generic[S]):
    """One leaf, or a group of leaves, of an output port.

    ``signature`` is the leaf's type and shape, as ``"f64[3]"``, or a group's
    leaves by relative path, as ``"{a:f64,b:f64[3]}"``.
    """

    def __init__(self, port: OutPort[Any], path: str, signature: str) -> None:
        self.port = port
        self.path = path
        self.signature = signature

    def __repr__(self) -> str:
        return f"Field({self.port.name}.{self.path}: {self.signature})"


class ArrayField(Field[S], Generic[S, E]):
    """A shaped leaf, or a split array, whose elements are ``E``."""

    def __getitem__(self, i: int) -> E:
        """Element ``i`` of a shaped leaf, or child ``i`` of a split array."""
        where = f"{self.port.name}.{self.path}"
        return cast(E, self.port.field(f"{self.path}.{index(i, where)}"))

    __iter__ = None
    """Indexing does not make a field iterable."""


def indexable(fields: dict[str, str], path: str, sig: str) -> bool:
    """Whether ``path`` is shaped, or a group whose children are ``0..N``."""
    if not sig.startswith("{"):
        return bool(parse(sig)[1])
    prefix = f"{path}."
    children = {leaf[len(prefix) :].split(".")[0] for leaf in fields if leaf.startswith(prefix)}
    return children == {str(i) for i in range(len(children))}


def index(i: object, where: str) -> int:
    """``i`` as an element index of ``where``; refuses a non-int, a bool, and a negative."""
    if isinstance(i, bool) or not isinstance(i, int) or i < 0:
        raise ConfigError(f"`{where}` takes a non-negative int index, got {i!r}")
    return i


def parse(sig: str) -> tuple[str, tuple[int, ...]]:
    """A leaf signature's type and dimensions: ``"f64[6,6]"`` is ``("f64", (6, 6))``."""
    ty, bracket, dims = sig.partition("[")
    if not bracket:
        return sig, ()
    return ty, tuple(int(d) for d in dims.rstrip("]").split(","))


def render(ty: str, dims: tuple[int, ...]) -> str:
    """The inverse of `parse`."""
    if not dims:
        return ty
    return f"{ty}[{','.join(str(d) for d in dims)}]"


def signature(fields: dict[str, str], path: str) -> str | None:
    """Returns the signature of ``path`` in a record's ``_fields``.

    A leaf has its own signature. A prefix of leaves has a group signature
    over its leaves' relative paths, sorted. An element of a shaped leaf
    drops one dimension per index. Anything else is ``None``.
    """
    if path in fields:
        return fields[path]
    prefix = f"{path}."
    group = sorted(
        (leaf[len(prefix) :], sig) for leaf, sig in fields.items() if leaf.startswith(prefix)
    )
    if not group:
        return _element(fields, path)
    return "{" + ",".join(f"{leaf}:{sig}" for leaf, sig in group) + "}"


def _element(fields: dict[str, str], path: str) -> str | None:
    """The signature of an element of the leaf that is a proper prefix of ``path``.

    Raises `ConfigError` for an index at or past its dimension.
    """
    leaf = next((leaf for leaf in fields if path.startswith(f"{leaf}.")), None)
    if leaf is None:
        return None
    segments = path[len(leaf) + 1 :].split(".")
    ty, dims = parse(fields[leaf])
    if len(segments) > len(dims) or not all(_canonical(s) for s in segments):
        return None
    for i, d in zip(map(int, segments), dims):
        if i >= d:
            raise ConfigError(f"`{path}`: index {i} is past dimension {d}")
    return render(ty, dims[len(segments) :])


def _canonical(segment: str) -> bool:
    """Digits only, no leading zero but ``"0"``."""
    return segment.isascii() and segment.isdigit() and (segment == "0" or segment[0] != "0")


def _given(fields: dict[str, Any]) -> dict[str, Any]:
    """Drops the keywords a generated ``__init__`` was not given."""
    return {key: value for key, value in fields.items() if value is not None}
