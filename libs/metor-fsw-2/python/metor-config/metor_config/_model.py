"""Specs, typed ports, and source locations."""

from __future__ import annotations

import json
import os
import sys
from dataclasses import dataclass
from typing import Any, Generic, TypeVar


def _source_ref() -> dict[str, Any]:
    """Anchor the caller: the first stack frame outside this package."""
    frame: Any = sys._getframe(1)
    pkg_dir = os.path.dirname(os.path.abspath(__file__))
    while frame is not None:
        file = os.path.abspath(frame.f_code.co_filename)
        if os.path.dirname(file) != pkg_dir:
            break
        frame = frame.f_back
    if frame is None:
        return {"file": None, "line": 0, "col": 1}
    path = frame.f_code.co_filename
    try:
        path = os.path.relpath(path)
    except ValueError:
        pass
    # Python line numbers are 1-based; columns are not reliably available before
    # 3.11, so col 1 is the honest anchor.
    return {"file": path, "line": frame.f_lineno, "col": 1}


def _params(kwargs: dict[str, Any]) -> dict[str, Any]:
    return json.loads(json.dumps(kwargs))


def _handle_name(handle: Any) -> str:
    """The instance name of a routing endpoint. Endpoints are handles at
    runtime (``add``/``slot`` returns a :class:`SystemHandle`, ``coordinator``
    is one), but a generated entry's handle is *typed* as its spec class, so
    the name is read dynamically."""
    return handle.name


class Spec:
    """A recorded ``(type, artifact, params)`` triple an ``add``/``allow`` uses.

    ``artifact`` is ``None`` for a registry (statically linked) system and the
    declaring artifact id for a loaded one.
    """

    def __init__(self, ty: str | None, artifact: str | None, params: dict[str, Any]):
        self.ty = ty
        self.artifact = artifact
        self.params = _params(params)
        # The pack-shared state this system attaches to, set by a shared-state
        # spec helper (`Uplink`/`Downlink`) from the handle its constructor
        # takes; `None` for an ordinary system. Rendered into the IR by
        # `Target.add`.
        self.attach: str | None = None

    def _param_source(self) -> Any:
        # `ParamSource`, externally tagged: no params -> the unit "None".
        return {"Value": self.params} if self.params else "None"


def static_system(ty: str, **params: Any) -> Spec:
    """A spec for a registry (statically linked) system named by ``ty``."""
    return Spec(ty, None, params)


F = TypeVar("F")


H = TypeVar("H", bound="Spec")


class Msg:
    """Marker for a self-describing (postcard) message port (checker-only)."""


class OutPort(Generic[F]):
    """A producer port carrying frame ``F``. Never instantiated: a handle's
    attribute access returns a :class:`PortRef` at runtime; the annotation is
    for the checker. The two fields mirror :class:`PortRef` so ``connect`` can
    read them under either type."""

    instance: str
    port: str


class InPort(Generic[F]):
    """A consumer port carrying frame ``F`` (see :class:`OutPort`)."""

    instance: str
    port: str


@dataclass(frozen=True)
class Artifact:
    """A loadable pack, as a generated module declares it in its ``ARTIFACT``
    constant. Using an entry from the module auto-registers this on the target
    (:meth:`Target.add`); ``manifest_hash`` is the staleness anchor the host
    checks at resolve.

    A prebuilt pack's module (``metor-fsw pack dev``, or an installed pack
    wheel) also carries ``prebuilt`` — the directory of per-triple libraries
    the host provisions from instead of running cargo — plus the generating
    ABI version and, for a published pack, its distribution name/version.

    ``kind`` is ``"cdylib"`` (the default, requiring ``crate``/``lib``) or
    ``"wasm"`` — one arch-neutral module with no crate behind it."""

    id: str
    crate: str | None = None
    lib: str | None = None
    manifest_hash: str | None = None
    prebuilt: str | None = None
    abi_version: int | None = None
    dist: str | None = None
    dist_version: str | None = None
    kind: str = "cdylib"


class System(Spec):
    """Base of a generated pack-entry class (and the occupant callables' return
    type). Carries the declaring :class:`Artifact` so registration is implicit;
    otherwise it is an ordinary :class:`Spec`."""

    def __init__(self, entry: str, artifact: "Artifact", **params: Any):
        super().__init__(entry, artifact.id, params)
        self.artifact_decl = artifact


class PortRef:
    """A ``(instance, port)`` pair naming one end of an edge."""

    def __init__(self, instance: str, port: str):
        self.instance = instance
        self.port = port


class StateHandle:
    """A declared pack-shared state (:meth:`Target.state`). Pass it to a
    shared-state system's constructor (``Downlink(link)``, ``Uplink(link,
    …)``) to attach that system to this state; the handle carries only the
    state's declaration name."""

    def __init__(self, name: str):
        self.name = name


class SystemHandle:
    """A registered system or slot. Attribute access yields a :class:`PortRef`
    at runtime; the annotation is :data:`Any` because an untyped handle (a slot,
    the coordinator, the programmatic escape hatch) has no per-port frame
    types. A generated entry's handle is *typed* as its `System` subclass, so
    its ports resolve against that class's annotations instead of here — that is
    where frame checking lives."""

    def __init__(self, name: str):
        self.name = name

    def __getattr__(self, name: str) -> Any:
        if name.startswith("_"):
            raise AttributeError(name)
        return PortRef(self.name, name)


def _drop_none(d: dict[str, Any]) -> dict[str, Any]:
    """Omit ``None`` values so serde's Option-missing-is-None rule applies."""
    return {k: v for k, v in d.items() if v is not None}
