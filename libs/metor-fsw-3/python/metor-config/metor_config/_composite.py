"""Composites: functions that add several systems as one reusable piece."""

from __future__ import annotations

import inspect
from functools import wraps
from typing import TYPE_CHECKING, Any, Callable, Concatenate, Generic, ParamSpec, TypeVar, overload

from ._config import ConfigError
from ._model import Loop, Record, System

if TYPE_CHECKING:
    from ._io import Kind, ResourceHandle
    from ._target import Target

P = ParamSpec("P")
R = TypeVar("R")
S = TypeVar("S", bound=System)
T = TypeVar("T", bound=Record)


C = TypeVar("C")
D = TypeVar("D")


class Deferred(Generic[C, R]):
    """A function and its arguments, waiting to be added. The function takes
    a ``C`` first: a `Scope` for a composite, a `Group` for a group."""

    def __init__(self, build: Callable[[C], R]) -> None:
        self._build = build


def defer(kind: Callable[[Callable[[C], R]], D], fn: Callable[Concatenate[C, P], R]) -> Callable[P, D]:
    """Wraps ``fn`` so that a call checks its arguments against ``fn``,
    raising `TypeError` if they do not fit, and returns a ``kind`` that runs
    ``fn`` when added."""
    signature = inspect.signature(fn)

    @wraps(fn)
    def bind(*args: P.args, **kwargs: P.kwargs) -> D:
        signature.bind(None, *args, **kwargs)
        return kind(lambda first: fn(first, *args, **kwargs))

    params = list(signature.parameters.values())[1:]
    bind.__signature__ = signature.replace(parameters=params)  # type: ignore[attr-defined]
    return bind


class Composite(Deferred["Scope", R]):
    """A composite waiting to be added. `Target.add` and `Scope.add` run it
    and return its result."""


def composite(fn: Callable[Concatenate[Scope, P], R]) -> Callable[P, Composite[R]]:
    """Makes ``fn`` a composite: calling it returns a `Composite` to add.

    ``fn`` takes a `Scope`, then its own inputs and params. Its body runs when
    the composite is added, and its result is what `add` returns. A
    ``NamedTuple`` of ports and handles can be passed as a port list.
    """
    return defer(Composite[R], fn)


class Scope:
    """Adds a composite's systems to its target, under the composite's name.

    A system ``nav`` in a composite added as ``gnc`` has the id ``gnc.nav``.
    """

    def __init__(self, target: Target, prefix: str) -> None:
        self._target = target
        self._prefix = prefix

    @overload
    def add(
        self,
        name: str,
        system: S,
        thread: str | None = None,
        channel: str | None = None,
        start: str | None = None,
    ) -> S: ...

    @overload
    def add(self, name: str, system: Composite[R]) -> R: ...

    def add(
        self,
        name: str,
        system: Any,
        thread: str | None = None,
        channel: str | None = None,
        start: str | None = None,
    ) -> Any:
        """Adds a system or composite. See `Target.add`.

        ``thread`` is not prefixed, so instances can share a thread. ``channel``
        is, so each instance has its own.
        """
        scoped = self._path(channel) if channel is not None else None
        return self._target.add(self._path(name), system, thread, scoped, start)

    def sim(self, name: str, system: S) -> S:
        """Adds a model. See `Target.sim`."""
        return self._target.sim(self._path(name), system)

    def resource(self, name: str, kind: Kind) -> ResourceHandle:
        """Creates a resource named ``{prefix}_{name}``. See `Target.resource`."""
        full = f"{self._prefix.replace('.', '_')}_{name}"
        if full in self._target._resources:
            taken = f"resource `{name}` of composite `{self._prefix}` is `{full}`, which is taken"
            raise ConfigError(taken)
        return self._target.resource(full, kind)

    def loop(self, record: type[T]) -> Loop[T]:
        """Creates a feedback edge. See `Target.loop`."""
        return self._target.loop(record)

    def _path(self, name: str) -> str:
        return f"{self._prefix}.{name}"
