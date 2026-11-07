"""Groups: functions that add several targets to a deployment as one
reusable piece."""

from __future__ import annotations

import inspect
from functools import wraps
from typing import TYPE_CHECKING, Callable, Concatenate, Generic, ParamSpec, TypeVar

from ._config import ConfigError
from ._model import Loop, Record
from ._target import Target

if TYPE_CHECKING:
    from ._deployment import Deployment

P = ParamSpec("P")
R = TypeVar("R")
T = TypeVar("T", bound=Record)


class GroupBuild(Generic[R]):
    """A group's function and arguments, waiting to be added.

    `Deployment.add` and `Group.add` run the function and return its result.
    """

    def __init__(self, build: Callable[[Group], R]) -> None:
        self._build = build


def group(fn: Callable[Concatenate[Group, P], R]) -> Callable[P, GroupBuild[R]]:
    """Makes ``fn`` a group: calling it returns a `GroupBuild` to add.

    ``fn`` takes a `Group`, then its own arguments. Its body runs when the
    group is added, and its result is what `add` returns. Arguments that do
    not fit ``fn`` raise `TypeError` at the call.
    """

    @wraps(fn)
    def bind(*args: P.args, **kwargs: P.kwargs) -> GroupBuild[R]:
        inspect.signature(fn).bind(None, *args, **kwargs)
        return GroupBuild(lambda g: fn(g, *args, **kwargs))

    return bind


class Group:
    """Adds a group's targets to its deployment, under the group's path.

    A target ``io`` in a group added as ``flight.cpu1`` has the namespace
    ``flight.cpu1.io``.
    """

    def __init__(self, deployment: Deployment, path: str) -> None:
        self._deployment = deployment
        self._path = path

    @property
    def path(self) -> str:
        """The group's dotted path."""
        return self._path

    def target(
        self,
        name: str | None = None,
        *,
        cycle_rate: float,
        sim_dt: float | None = None,
        ring_depth: int = 8,
        seed: int = 0,
        epoch: int | None = None,
    ) -> Target:
        """Creates a target named ``{path}.{name}``, or ``{path}`` without
        ``name``. The rest are `Target`'s arguments."""
        namespace = self._path if name is None else child(self._path, name)
        target = Target(cycle_rate, sim_dt, ring_depth, namespace, seed, epoch)
        self._deployment.targets.append(target)
        return target

    def add(self, name: str, build: GroupBuild[R]) -> R:
        """Adds a group under ``{path}.{name}`` and returns its result."""
        return self._deployment._add_group(child(self._path, name), build)

    def loop(self, record: type[T]) -> Loop[T]:
        """Creates a feedback edge. See `Target.loop`."""
        return Loop(record)


def child(path: str, name: str) -> str:
    """Returns ``{path}.{name}``, or ``name`` at the top. Raises `ConfigError`
    unless ``name`` is an identifier."""
    if not isinstance(name, str) or not name.isidentifier():
        raise ConfigError(f"a group or target name is an identifier, got {name!r}")
    return f"{path}.{name}" if path else name
