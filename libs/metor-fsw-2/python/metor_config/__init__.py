"""Record a metor-fsw mission as the ``Wiring`` IR the host resolves.

A mission file builds a :class:`Mission`, adds systems, and connects their
ports; each call records a plain-data spec. At interpreter exit (or an explicit
:func:`emit`) the recorded mission is serialized to the JSON ``Wiring`` the Rust
host ingests -- serde's representation is the contract, so the shapes here mirror
``libs/metor-fsw-2/src/ir.rs`` exactly (externally tagged enums, field names,
``in_`` for a consumer input port).

This is the Phase 1 core: coordinator config, static systems, and frame edges.
The full authoring surface (loaded packs, slots, scopes, alarms, message routes)
lands with the rest of the recorder library.
"""

from __future__ import annotations

import atexit
import json
import os
import sys
from typing import Any

__version__ = "0.1.0"

# The IR model version this recorder emits; must match `ir::IR_VERSION`.
IR_VERSION = 1

# Every live Mission, so emission can enforce the exactly-one rule.
_missions: list["Mission"] = []


def _source_ref() -> dict[str, Any]:
    """Anchor the caller: the first stack frame outside this package."""
    frame = sys._getframe(1)
    pkg_dir = os.path.dirname(__file__)
    while frame is not None:
        file = frame.f_code.co_filename
        if os.path.dirname(os.path.abspath(file)) != pkg_dir:
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


def _check_json(value: Any, key: str) -> Any:
    """Reject a params value serde's JSON codec cannot represent."""
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (list, tuple)):
        return [_check_json(v, key) for v in value]
    if isinstance(value, dict):
        return {str(k): _check_json(v, f"{key}.{k}") for k, v in value.items()}
    raise TypeError(f"param {key!r} is not JSON-representable: {type(value).__name__}")


class Spec:
    """A recorded ``(type, artifact, params)`` triple an ``add`` registers."""

    def __init__(self, ty: str | None, artifact: str | None, params: dict[str, Any]):
        self.ty = ty
        self.artifact = artifact
        self.params = {k: _check_json(v, k) for k, v in params.items()}

    def _param_source(self) -> Any:
        # `ParamSource`, externally tagged: no params -> the unit "None".
        if not self.params:
            return "None"
        return {"Value": self.params}


def static_system(ty: str, **params: Any) -> Spec:
    """A spec for a registry (statically linked) system named by ``ty``."""
    return Spec(ty, None, params)


class PortRef:
    """A ``(instance, port)`` pair naming one end of an edge."""

    def __init__(self, instance: str, port: str):
        self.instance = instance
        self.port = port


class SystemHandle:
    """A registered system; attribute access yields a :class:`PortRef`."""

    def __init__(self, name: str):
        self.name = name

    def port(self, name: str) -> PortRef:
        """The explicit spelling of ``handle.<name>``."""
        return PortRef(self.name, name)

    def __getattr__(self, name: str) -> PortRef:
        if name.startswith("_"):
            raise AttributeError(name)
        return PortRef(self.name, name)


class Mission:
    """A mission under construction. Exactly one may exist at emission time."""

    def __init__(
        self,
        cycle_rate: float,
        sim_dt: float | None = None,
        default_depth: int | None = None,
    ):
        self.cycle_rate = float(cycle_rate)
        self.sim_dt = sim_dt
        self.default_depth = default_depth
        self._systems: list[dict[str, Any]] = []
        self._edges: list[dict[str, Any]] = []
        self._names: set[str] = set()
        _missions.append(self)

    def add(self, name: str, spec: Spec, process: bool = False) -> SystemHandle:
        """Register ``spec`` under ``name`` and return its handle."""
        if name in self._names:
            raise ValueError(f"duplicate instance name {name!r}")
        self._names.add(name)
        self._systems.append(
            {
                "name": name,
                "ty": spec.ty,
                "artifact": spec.artifact,
                "params": spec._param_source(),
                "process": bool(process),
                "src": _source_ref(),
                "scope": None,
            }
        )
        return SystemHandle(name)

    def connect(self, src: PortRef, dst: PortRef, delayed: bool = False) -> None:
        """Record a component-frame edge from ``src`` to ``dst``."""
        self._edges.append(
            {
                "from": src.instance,
                "out": src.port,
                "to": dst.instance,
                "in_": dst.port,
                "delayed": bool(delayed),
                "kind": "Frame",
                "src": _source_ref(),
            }
        )

    def _clock(self) -> Any:
        if self.sim_dt is not None:
            return {"Simulated": {"dt_secs": float(self.sim_dt)}}
        return "Wall"

    def to_ir(self) -> dict[str, Any]:
        """The serialized ``Wiring`` this mission describes."""
        return {
            "ir_version": IR_VERSION,
            "metor_config_version": __version__,
            "coordinator": {
                "cycle_rate": self.cycle_rate,
                "default_depth": self.default_depth,
                "clock": self._clock(),
            },
            "artifacts": [],
            "systems": self._systems,
            "slots": [],
            "edges": self._edges,
            "scopes": [],
        }


def _the_mission() -> Mission:
    if len(_missions) != 1:
        raise RuntimeError(
            f"exactly one Mission must exist at emission, found {len(_missions)}"
        )
    return _missions[0]


def emit(mission: Mission | None = None) -> None:
    """Write the mission IR to ``$METOR_IR_OUT`` (stdout if unset)."""
    ir = (mission or _the_mission()).to_ir()
    text = json.dumps(ir, indent=2)
    out = os.environ.get("METOR_IR_OUT")
    if out:
        with open(out, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)


def _emit_at_exit() -> None:
    # Only emit for a well-formed single-mission module; a broken module already
    # raised, and its traceback is the error surface.
    if len(_missions) == 1:
        emit(_missions[0])


atexit.register(_emit_at_exit)
