"""Fields of a frame port, which a `Mux` maps into a new frame."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Generic, TypeVar

if TYPE_CHECKING:
    from ._model import OutPort

S = TypeVar("S", bound=str)


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


def signature(fields: dict[str, str], path: str) -> str | None:
    """Returns the signature of ``path`` in a record's ``_fields``.

    A leaf has its own signature. A prefix of leaves has a group signature
    over its leaves' relative paths, sorted. Anything else is ``None``.
    """
    if path in fields:
        return fields[path]
    prefix = f"{path}."
    group = sorted(
        (leaf[len(prefix) :], sig) for leaf, sig in fields.items() if leaf.startswith(prefix)
    )
    if not group:
        return None
    return "{" + ",".join(f"{leaf}:{sig}" for leaf, sig in group) + "}"


def _given(fields: dict[str, Field[Any] | None]) -> dict[str, Field[Any]]:
    """Drops the keywords a generated ``__init__`` was not given."""
    return {key: value for key, value in fields.items() if value is not None}
