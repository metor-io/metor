"""Record a metor-fsw mission as the ``Wiring`` IR the host resolves.

A mission file builds a :class:`Mission`, declares its artifacts, adds systems
and slots, and connects their ports; each call records a plain-data spec. At
interpreter exit (or an explicit :func:`emit`) the recorded mission is
serialized to the JSON ``Wiring`` the Rust host ingests.

serde's representation is the contract, so the shapes here mirror
``libs/metor-fsw-2/src/ir.rs`` exactly: externally tagged enums (a unit variant
is its bare name, a data variant a single-key object), the struct field names
verbatim (``in_`` for a consumer input port, ``alarm`` for the alarm list), and
absent optionals emitted as ``null``. Any divergence is a bug here, never a
reason to bend the Rust side.

The surface is an explicit builder with no global state beyond the
exactly-one-``Mission`` rule. Blocks are plain functions over :class:`PortRef`
values; :meth:`Mission.scope` gives them collision-free instance names and a
place in the IR scope tree.
"""

from __future__ import annotations

import atexit
import json
import os
import sys
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Any, Iterator

__version__ = "0.1.0"

# The IR model version this recorder emits; must match `ir::IR_VERSION`.
IR_VERSION = 1

# Reserved instance name of the coordinator (command plane).
COORDINATOR = "coordinator"

# Every live Mission, so emission can enforce the exactly-one rule.
_missions: list["Mission"] = []


# ---------------------------------------------------------------------------
# Provenance and value hygiene
# ---------------------------------------------------------------------------


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


def _json_scalar(value: Any, key: str) -> Any:
    """Coerce a params value to something serde's JSON codec accepts, naming
    the offending key when it cannot."""
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, (list, tuple)):
        return [_json_scalar(v, key) for v in value]
    if isinstance(value, dict):
        return {str(k): _json_scalar(v, f"{key}.{k}") for k, v in value.items()}
    raise TypeError(f"param {key!r} is not JSON-representable: {type(value).__name__}")


def _params(kwargs: dict[str, Any]) -> dict[str, Any]:
    return {k: _json_scalar(v, k) for k, v in kwargs.items()}


def _cdylib_file_name(stem: str) -> str:
    """The platform shared-object file name, matching `wiring::cdylib_file_name`."""
    if sys.platform == "darwin":
        return f"lib{stem}.dylib"
    if sys.platform == "win32":
        return f"{stem}.dll"
    return f"lib{stem}.so"


# ---------------------------------------------------------------------------
# Specs, handles, and port references
# ---------------------------------------------------------------------------


class Spec:
    """A recorded ``(type, artifact, params)`` triple an ``add``/``allow`` uses.

    ``artifact`` is ``None`` for a registry (statically linked) system and the
    declaring artifact id for a loaded one.
    """

    def __init__(self, ty: str | None, artifact: str | None, params: dict[str, Any]):
        self.ty = ty
        self.artifact = artifact
        self.params = _params(params)

    def _param_source(self) -> Any:
        # `ParamSource`, externally tagged: no params -> the unit "None".
        return {"Value": self.params} if self.params else "None"


def static_system(ty: str, **params: Any) -> Spec:
    """A spec for a registry (statically linked) system named by ``ty``."""
    return Spec(ty, None, params)


class _EntryCallable:
    """One pack entry; calling it records a spec bound to its artifact."""

    def __init__(self, artifact_id: str, entry: str):
        self._artifact = artifact_id
        self._entry = entry

    def __call__(self, **params: Any) -> Spec:
        return Spec(self._entry, self._artifact, params)


class ArtifactHandle:
    """A declared artifact. Attribute or item access yields an entry callable
    (``adcs.Plant`` or ``adcs["Plant"]``)."""

    def __init__(self, artifact_id: str):
        self._id = artifact_id

    def __getattr__(self, entry: str) -> _EntryCallable:
        if entry.startswith("_"):
            raise AttributeError(entry)
        return _EntryCallable(self._id, entry)

    def __getitem__(self, entry: str) -> _EntryCallable:
        return _EntryCallable(self._id, entry)


class PortRef:
    """A ``(instance, port)`` pair naming one end of an edge."""

    def __init__(self, instance: str, port: str):
        self.instance = instance
        self.port = port


class SystemHandle:
    """A registered system or slot; attribute access yields a :class:`PortRef`."""

    def __init__(self, name: str):
        self.name = name

    def port(self, name: str) -> PortRef:
        """The explicit spelling of ``handle.<name>``, for generated code."""
        return PortRef(self.name, name)

    def __getattr__(self, name: str) -> PortRef:
        if name.startswith("_"):
            raise AttributeError(name)
        return PortRef(self.name, name)


# ---------------------------------------------------------------------------
# Alarm data helpers (frozen dataclasses: unknown kwargs raise at eval time)
# ---------------------------------------------------------------------------


def _drop_none(d: dict[str, Any]) -> dict[str, Any]:
    """Omit ``None`` values so serde's Option-missing-is-None rule applies."""
    return {k: v for k, v in d.items() if v is not None}


@dataclass(frozen=True)
class Target:
    """The component value an alarm monitors: an instance-prefixed component id
    plus an optional element index into its shape."""

    component: str
    element: int | None = None

    def to_json(self) -> dict[str, Any]:
        return _drop_none({"component": self.component, "element": self.element})


@dataclass(frozen=True)
class band:  # noqa: N801 - a data literal, spelled lowercase by design
    """A pair of optional thresholds bounding acceptable values at one severity."""

    above: float | None = None
    below: float | None = None

    def to_json(self) -> dict[str, Any]:
        return _drop_none({"above": self.above, "below": self.below})


@dataclass(frozen=True)
class Alarm:
    """A limit alarm over a single component value. Band containment and the
    warning/critical-required rule are validated on the Rust deserialize path,
    the one source of truth."""

    id: str
    name: str
    target: Target
    description: str = ""
    warning: band | None = None
    critical: band | None = None
    debounce: int | None = None
    hysteresis: float | None = None
    latching: bool | None = None
    severity: str | None = None

    def to_json(self) -> dict[str, Any]:
        return _drop_none(
            {
                "id": self.id,
                "name": self.name,
                "description": self.description,
                "target": self.target.to_json(),
                "warning": self.warning.to_json() if self.warning else None,
                "critical": self.critical.to_json() if self.critical else None,
                "debounce": self.debounce,
                "hysteresis": self.hysteresis,
                "latching": self.latching,
                "severity": self.severity,
            }
        )


def Alarms(alarms: list[Alarm]) -> Spec:  # noqa: N802 - a system-type wrapper
    """The built-in alarm engine, its ``AlarmsParams`` carrying one entry per
    alarm under the ``alarm`` field the Rust struct declares."""
    return static_system("Alarms", alarm=[a.to_json() for a in alarms])


def TcpUplink(addr: str, msgs: list[str] | None = None) -> Spec:  # noqa: N802
    """The built-in TCP command uplink (``UplinkParams``)."""
    return static_system("TcpUplink", **_drop_none({"addr": addr, "msgs": msgs}))


def TcpDownlink(  # noqa: N802
    addr: str,
    instances: list[str] | None = None,
    frames: list[str] | None = None,
) -> Spec:
    """The built-in TCP telemetry downlink (``DownlinkParams``); omitting both
    subset lists taps everything."""
    return static_system(
        "TcpDownlink",
        **_drop_none({"addr": addr, "instances": instances, "frames": frames}),
    )


# ---------------------------------------------------------------------------
# The mission
# ---------------------------------------------------------------------------

_INIT_STATES = {"empty": "Empty", "loaded": "Loaded", "running": "Running"}


class Mission:
    """A mission under construction. Exactly one may exist at emission time.

    ``sim_dt`` (seconds) selects a free-running simulated clock; without it the
    loop paces a wall clock at ``cycle_rate``. ``default_depth`` is the in-flight
    record depth for a buffer with no rate hint. These are the only knobs
    ``CoordinatorSpec`` carries.
    """

    def __init__(
        self,
        cycle_rate: float,
        sim_dt: float | None = None,
        default_depth: int | None = None,
    ):
        self.cycle_rate = float(cycle_rate)
        self.sim_dt = sim_dt
        self.default_depth = default_depth
        self.coordinator = SystemHandle(COORDINATOR)
        self._artifacts: list[dict[str, Any]] = []
        self._systems: list[dict[str, Any]] = []
        self._slots: list[dict[str, Any]] = []
        self._edges: list[dict[str, Any]] = []
        self._scopes: list[dict[str, Any]] = []
        self._scope_stack: list[int] = []
        self._names: set[str] = set()
        _missions.append(self)

    # -- artifacts ----------------------------------------------------------

    def artifact(self, id: str, crate: str, lib: str) -> ArtifactHandle:
        """Declare a loadable pack cdylib and return its entry-callable handle."""
        self._artifacts.append(
            {
                "id": id,
                "crate_name": crate,
                "cdylib": _cdylib_file_name(lib),
                "path": None,
                # A hand-declared artifact carries no generated-stub hash, so
                # the host's staleness check skips it (as it does for KDL).
                "manifest_hash": None,
                "src": _source_ref(),
            }
        )
        return ArtifactHandle(id)

    # -- scopes -------------------------------------------------------------

    @contextmanager
    def scope(self, name: str) -> Iterator[None]:
        """Prefix instance names with ``name.`` and record the scope in the IR,
        nesting through the enclosing scope."""
        parent = self._scope_stack[-1] if self._scope_stack else None
        prefix = self._scopes[parent]["path"] if parent is not None else None
        path = f"{prefix}.{name}" if prefix else name
        index = len(self._scopes)
        self._scopes.append({"path": path, "parent": parent, "src": _source_ref()})
        self._scope_stack.append(index)
        try:
            yield
        finally:
            self._scope_stack.pop()

    def _scoped(self, name: str) -> tuple[str, int | None]:
        if not self._scope_stack:
            return name, None
        index = self._scope_stack[-1]
        return f"{self._scopes[index]['path']}.{name}", index

    def _claim(self, name: str) -> None:
        if name in self._names:
            raise ValueError(f"duplicate instance name {name!r}")
        self._names.add(name)

    # -- systems and slots --------------------------------------------------

    def add(self, name: str, spec: Spec, process: bool = False) -> SystemHandle:
        """Register ``spec`` under ``name`` (scope-prefixed) and return its handle."""
        full, scope = self._scoped(name)
        self._claim(full)
        self._systems.append(
            {
                "name": full,
                "ty": spec.ty,
                "artifact": spec.artifact,
                "params": spec._param_source(),
                "process": bool(process),
                "src": _source_ref(),
                "scope": scope,
            }
        )
        return SystemHandle(full)

    def slot(
        self,
        name: str,
        inputs: list[str],
        outputs: list[str],
        allow: list[Spec],
        initial: str | None = None,
        initial_state: str = "running",
        process: bool = False,
    ) -> SystemHandle:
        """Register a runtime-loadable slot. ``allow`` occupants use the same
        call convention as system specs; ``initial`` names one of them."""
        full, scope = self._scoped(name)
        self._claim(full)
        occupants = [
            {
                "occupant": s.ty,
                "artifact": s.artifact,
                "params": s._param_source(),
                "src": _source_ref(),
            }
            for s in allow
        ]
        initial_spec = None
        if initial is not None:
            if initial not in {o["occupant"] for o in occupants}:
                raise ValueError(f"initial occupant {initial!r} is not in the allow set")
            state = _INIT_STATES.get(initial_state)
            if state is None:
                raise ValueError(f"unknown initial_state {initial_state!r}")
            initial_spec = {"occupant": initial, "state": state}
        self._slots.append(
            {
                "name": full,
                "inputs": list(inputs),
                "outputs": list(outputs),
                "allow": occupants,
                "initial": initial_spec,
                "process": bool(process),
                "src": _source_ref(),
                "scope": scope,
            }
        )
        return SystemHandle(full)

    # -- edges --------------------------------------------------------------

    def connect(self, src: PortRef, dst: PortRef, delayed: bool = False) -> None:
        """Record a component-frame edge from ``src`` to ``dst``."""
        self._edge(src, dst, "Frame", bool(delayed))

    def route(self, src: SystemHandle, dst: SystemHandle, msg: str) -> None:
        """Record a message edge carrying ``msg`` from ``src`` to ``dst``. A
        message edge is log-delivery pub/sub, so it has no ``delayed`` form."""
        self._edges.append(
            {
                "from": src.name,
                "out": msg,
                "to": dst.name,
                "in_": msg,
                "delayed": False,
                "kind": "Msg",
                "src": _source_ref(),
            }
        )

    def _edge(self, src: PortRef, dst: PortRef, kind: str, delayed: bool) -> None:
        self._edges.append(
            {
                "from": src.instance,
                "out": src.port,
                "to": dst.instance,
                "in_": dst.port,
                "delayed": delayed,
                "kind": kind,
                "src": _source_ref(),
            }
        )

    # -- emission -----------------------------------------------------------

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
            "artifacts": self._artifacts,
            "systems": self._systems,
            "slots": self._slots,
            "edges": self._edges,
            "scopes": self._scopes,
        }


# ---------------------------------------------------------------------------
# Emission
# ---------------------------------------------------------------------------


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
    # Emit only for a well-formed single-mission module; a broken module has
    # already raised, and its traceback is the error surface.
    if len(_missions) == 1:
        emit(_missions[0])


atexit.register(_emit_at_exit)
