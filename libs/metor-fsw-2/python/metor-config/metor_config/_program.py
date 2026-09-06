"""Capture Python declarations for compilation by the host."""

from __future__ import annotations

import inspect
import os
import textwrap
from dataclasses import dataclass
from typing import Any
from ._model import PortRef, SystemHandle, _drop_none


_program: list[dict[str, Any]] = []


_frames: set[str] = set()


def _capture(obj: Any) -> dict[str, Any]:
    """Record a declaration's source and provenance for the target program."""
    lines, line = inspect.getsourcelines(obj)
    file = inspect.getsourcefile(obj)
    if file is not None:
        try:
            file = os.path.relpath(file)
        except ValueError:
            pass
    entry: dict[str, Any] = {
        "name": obj.__name__,
        "source": textwrap.dedent("".join(lines)),
        "src": {"file": file, "line": line, "col": 1},
        "layout": None,
        "system": False,
    }
    _program.append(entry)
    return entry


class Frame:
    """Base of a per-frame marker class.

    A generated pack module's markers are field-less (checker-only) and name
    host frames. A subclass with annotated fields is a compiled frame: its
    source is captured into the target program (see :func:`system`)."""

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        if "__annotations__" in cls.__dict__:
            _frames.add(cls.__name__)
            _capture(cls)


class Tensor:
    """The tensor annotation (``Tensor[f64, 3]``), evaluated only so class
    bodies run: the compiler reads the annotation from source, not from this
    object."""

    def __class_getitem__(cls, item: Any) -> Any:
        return cls


f64 = float


i64 = int


class ExprHandle(SystemHandle):
    """A captured ``@system`` declaration, and — once :meth:`Target.add`
    registers it — the instance's handle. ``.out`` names its one output port;
    any other attribute is a :class:`PortRef` like any handle's. Until the
    add, ``name`` is the declaration's own (ports resolve only after
    registration, like a native spec's)."""

    def __init__(self, name: str, entry: dict[str, Any], out: str):
        super().__init__(name)
        self._entry = entry
        self._out = out

    @property
    def out(self) -> Any:
        return PortRef(self.name, self._out)


def _snake(name: str) -> str:
    """``RateEstimate`` to ``rate_estimate`` — the compiler's frame naming."""
    out = ""
    for i, c in enumerate(name):
        if c.isupper() and i > 0:
            out += "_"
        out += c.lower()
    return out


def _output_frame(func: Any) -> str:
    """The output port's frame name: the snake case of a declared Frame class
    named by the return annotation, else the function's own name (the
    anonymous one-field frame of the sugar form)."""
    ret = func.__annotations__.get("return")
    name = ret if isinstance(ret, str) else getattr(ret, "__name__", None)
    if isinstance(name, str) and name in _frames:
        return _snake(name)
    return func.__name__


def _system(func: Any) -> ExprHandle:
    entry = _capture(func)
    entry["system"] = True
    # The instance name the handle was added under; `None` until then.
    entry["added"] = None
    placed = getattr(func, "_metor_node", None)
    if placed is not None:
        entry["layout"] = placed
    return ExprHandle(func.__name__, entry, _output_frame(func))


def system(*args: Any, **kwargs: Any) -> Any:
    """Declare a Python system that can run on the vehicle.

    Bare (``@system``) over Frame-annotated parameters, or parameterized
    (``@system("imu.omega_b")``, ``bind=``, ``on=``, ``rate=``) — the
    arguments configure the *compiled* system and are read from source by the
    host, so they are accepted and ignored here. Decoration only captures the
    declaration; :meth:`Target.add` registers an instance, exactly like a
    native pack entry. Returns the handle ``add`` takes: after registration
    ``handle.out`` is the output port for :meth:`Target.connect`."""
    if len(args) == 1 and not kwargs and inspect.isfunction(args[0]):
        return _system(args[0])
    return _system


def node(x: float, y: float) -> Any:
    """Pin a declaration's canvas card at ``(x, y)``. Stacks with ``@system``
    in either order; the position rides the IR as the system's layout."""

    def place(obj: Any) -> Any:
        if isinstance(obj, ExprHandle):
            obj._entry["layout"] = [float(x), float(y)]
        else:
            obj._metor_node = [float(x), float(y)]
        return obj

    return place


@dataclass(frozen=True)
class State:
    """One row of a :class:`StateChip` table: the code, what it means, and
    optionally the colour it shows in.

    Also the base of a compiled system's state record: a subclass with
    annotated, defaulted fields is captured into the target program exactly
    like a :class:`Frame` subclass (see :func:`system`)."""

    value: float
    label: str
    color: str | None = None

    def __init_subclass__(cls, **kwargs: Any) -> None:
        super().__init_subclass__(**kwargs)
        if "__annotations__" in cls.__dict__:
            _capture(cls)

    def _json(self) -> dict[str, Any]:
        return _drop_none(
            {"value": float(self.value), "label": self.label, "color": self.color}
        )
