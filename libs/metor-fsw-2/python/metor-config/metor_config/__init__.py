"""Record a metor-fsw target as the ``Wiring`` IR the host resolves.

A target file builds a :class:`Target`, declares its artifacts, adds systems
and slots, and connects their ports; each call records a plain-data spec. At
interpreter exit (or an explicit :func:`emit`) the recorded target is
serialized to the JSON ``Wiring`` the Rust host ingests.

serde's representation is the contract, so the shapes here mirror
``libs/metor-fsw-2/src/ir.rs`` exactly: externally tagged enums (a unit variant
is its bare name, a data variant a single-key object), the struct field names
verbatim (``in_`` for a consumer input port, ``alarm`` for the alarm list), and
absent optionals emitted as ``null``. Any divergence is a bug here, never a
reason to bend the Rust side.

The surface is an explicit builder with no global state beyond the
exactly-one-``Target`` rule. Blocks are plain functions over :class:`PortRef`
values; :meth:`Target.scope` gives them collision-free instance names and a
place in the IR scope tree.
"""

from __future__ import annotations

import atexit
import json
import os
import sys
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Any, Generic, Iterator, TypeVar, cast

__version__ = "0.3.0"

# The IR model version this recorder emits; must match `ir::IR_VERSION`.
# v2 dropped the KDL front-end's `ParamSource::Kdl` variant (phase 4).
# v3 replaced the artifact's recorded shared-object file name with the bare
# `lib` stem (the file name is derived per target triple by the host) and
# added the prebuilt-artifact fields.
# v4 added the `states` list and replaced the outbound TcpDownlink/TcpUplink
# built-ins with the in-FSW link server (a `TcpServer` state the `Downlink`/
# `Uplink` systems attach to).
# v5 made attachment explicit: a system names the state it attaches to via the
# `attach` field (the built-in `Downlink`/`Uplink` take the state handle in
# their constructor), replacing the host's compiled-in by-type association.
IR_VERSION = 5

# Reserved instance name of the coordinator (command plane).
COORDINATOR = "coordinator"

# Every live Target, so emission can enforce the exactly-one rule.
_targets: list["Target"] = []


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


def _validate_namespace(namespace: str | None) -> str | None:
    """A target namespace is a bare dotted path (``"sat1"``, ``"fleet.sat1"``):
    non-empty, no leading/trailing dot, and every segment non-empty. It prefixes
    every component name the target registers, so a malformed one would corrupt
    every :class:`ComponentId`; reject it here rather than emit it."""
    if namespace is None:
        return None
    if not isinstance(namespace, str):
        raise TypeError(f"namespace must be a str, not {type(namespace).__name__}")
    if not namespace or namespace.startswith(".") or namespace.endswith("."):
        raise ValueError(f"namespace {namespace!r} must be a non-empty dotted path")
    if any(seg == "" for seg in namespace.split(".")):
        raise ValueError(f"namespace {namespace!r} has an empty segment")
    return namespace


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


def _handle_name(handle: Any) -> str:
    """The instance name of a routing endpoint. Endpoints are handles at
    runtime (``add``/``slot`` returns a :class:`SystemHandle`, ``coordinator``
    is one), but a generated entry's handle is *typed* as its spec class, so
    the name is read dynamically."""
    return handle.name


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


# ---------------------------------------------------------------------------
# Typed core for generated packs (`metor-fsw stubgen`)
#
# These are the symbols the generated `packs/<id>.py` modules import. At
# runtime they are thin: `Frame`/`Msg` are empty markers, `InPort`/`OutPort`
# are erased generics that never get instantiated (a handle's `__getattr__`
# returns a `PortRef`), and `System` is a `Spec` that also carries the
# artifact it came from so `Target.add` can auto-register it. All the typing
# lives in the generated annotations, which pyright reads; the recorder's
# behavior is exactly Phase 1's.
# ---------------------------------------------------------------------------

# The frame a port carries, the type variable that makes a cross-system frame
# mismatch a pyright error at `connect`.
F = TypeVar("F")

# The spec type `Target.add` echoes back, so a generated entry's handle keeps
# the entry's typed port attributes.
H = TypeVar("H", bound="Spec")


class Frame:
    """Base of a generated per-frame marker class (checker-only)."""


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
    ABI version and, for a published pack, its distribution name/version."""

    id: str
    crate: str
    lib: str
    manifest_hash: str | None = None
    prebuilt: str | None = None
    abi_version: int | None = None
    dist: str | None = None
    dist_version: str | None = None


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

    def port(self, name: str) -> Any:
        """The explicit, untyped spelling of ``handle.<name>``, for
        programmatic generation."""
        return PortRef(self.name, name)

    def __getattr__(self, name: str) -> Any:
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
class Component:
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
    target: Component
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


def TcpServer(addr: str, name: str | None = None) -> Spec:  # noqa: N802
    """The built-in link server state (``LinkParams``): the FSW listens on
    ``addr``; ground tools connect to it for the downlink stream and command
    ingest alike. Declare it with :meth:`Target.state`.

    ``name`` is the human node name advertised over mDNS for discovery; when
    omitted the FSW falls back to the OS hostname."""
    return static_system("TcpServer", **_drop_none({"addr": addr, "name": name}))


def _attached(spec: Spec, state: StateHandle) -> Spec:
    """Attach ``spec`` to the pack-shared ``state`` and return it."""
    if not isinstance(state, StateHandle):
        raise TypeError(
            f"expected a state handle from m.state(...), not {type(state).__name__}"
        )
    spec.attach = state.name
    return spec


def Uplink(state: StateHandle, msgs: list[str] | None = None) -> Spec:  # noqa: N802
    """The built-in command uplink (``UplinkParams``), draining the ``state``
    link server (the handle :meth:`Target.state` returned for a
    :func:`TcpServer`). Add it before its consumers and a command is consumed
    the same cycle it arrives."""
    return _attached(static_system("Uplink", **_drop_none({"msgs": msgs})), state)


def Downlink(  # noqa: N802
    state: StateHandle,
    instances: list[str] | None = None,
    frames: list[str] | None = None,
) -> Spec:
    """The built-in telemetry downlink (``DownlinkParams``), streaming over the
    ``state`` link server (the handle :meth:`Target.state` returned for a
    :func:`TcpServer`); omitting both subset lists taps everything."""
    return _attached(
        static_system("Downlink", **_drop_none({"instances": instances, "frames": frames})),
        state,
    )


# ---------------------------------------------------------------------------
# Panel presets
# ---------------------------------------------------------------------------


def component_id(name: str) -> int:
    """The numeric id of a fully-qualified component name — masked FNV-1a-64,
    mirroring the Rust ``ComponentId::new``."""
    h = 0xCBF29CE484222325
    for b in name.encode():
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h & ~(1 << 63)


def _qualify(name: str, namespace: str | None) -> str:
    return f"{namespace}.{name}" if namespace else name


# The panel's categorical trace palette, cycled when a trace has no explicit
# color. Colors are "#rrggbbaa" strings (the layout document's encoding).
_TRACE_PALETTE = [
    "#fab387ff",  # peach
    "#89b4faff",  # blue
    "#a6e3a1ff",  # green
    "#f38ba8ff",  # red
    "#cba6f7ff",  # mauve
    "#94e2d5ff",  # teal
    "#f9e2afff",  # yellow
    "#f5c2e7ff",  # pink
]


@dataclass(frozen=True)
class PaneState:
    """One pane item: a panel serialization key plus its config dict, embedded
    into the layout as a JSON blob.

    The escape hatch for panel kinds without a typed helper below — any pane
    the panel can serialize can be authored this way. Component references
    inside a raw ``state`` must be fully qualified (namespace included): the
    recorder cannot rewrite an opaque dict."""

    kind: str
    state: dict[str, Any]

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return {"kind": self.kind, "state": json.dumps(self.state)}


@dataclass(frozen=True)
class Trace:
    """One plot trace over ``component`` (namespace-relative), element
    ``element`` of its shape. ``color`` is ``"#rrggbbaa"``; unset cycles the
    panel palette."""

    component: str
    element: int = 0
    label: str | None = None
    color: str | None = None


@dataclass(frozen=True)
class TimeSeriesPlot:
    """A time-series plot pane. ``x_range`` uses the panel's time-range
    grammar (e.g. ``"LAST 30m"``); empty follows the layout's global range."""

    traces: list[Trace]
    label: str = ""
    x_range: str = ""

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("time_series_plot", self._state(namespace))._item(namespace)

    def _state(self, namespace: str | None) -> dict[str, Any]:
        traces = [
            _drop_none(
                {
                    "component_id": component_id(_qualify(t.component, namespace)),
                    "element_index": t.element,
                    "label": t.label or t.component,
                    "color": t.color or _TRACE_PALETTE[i % len(_TRACE_PALETTE)],
                }
            )
            for i, t in enumerate(self.traces)
        ]
        return {"label": self.label, "x_range": self.x_range, "traces": traces}

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        # The dashboard names this kind `plot`, not `time_series_plot`.
        return "plot", self._state(namespace), (400.0, 250.0)


@dataclass(frozen=True)
class Text:
    """A single component's latest value rendered as text."""

    component: str

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("component_text", self._state(namespace))._item(namespace)

    def _state(self, namespace: str | None) -> dict[str, Any]:
        return {"component": _qualify(self.component, namespace)}

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        return "text", self._state(namespace), (160.0, 60.0)


@dataclass(frozen=True)
class TrafficLight:
    """One component as a colored on/off square."""

    component: str
    color: str | None = None

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("traffic_light", self._state(namespace))._item(namespace)

    def _state(self, namespace: str | None) -> dict[str, Any]:
        return _drop_none(
            {"component": _qualify(self.component, namespace), "color": self.color}
        )

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        return "traffic_light", self._state(namespace), (120.0, 120.0)


@dataclass(frozen=True)
class TrafficLightGrid:
    """Every component matching a glob ``pattern`` as a traffic-light grid."""

    pattern: str
    color: str | None = None

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("traffic_light_grid", self._state(namespace))._item(namespace)

    def _state(self, namespace: str | None) -> dict[str, Any]:
        return _drop_none(
            {"pattern": _qualify(self.pattern, namespace), "color": self.color}
        )

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        return "traffic_light_grid", self._state(namespace), (360.0, 200.0)


def Logs() -> PaneState:  # noqa: N802 - pane constructors read as types
    """The streaming log viewer."""
    return PaneState("logs", {})


def AlarmList(history: bool = False) -> PaneState:  # noqa: N802
    """The alarm list with acknowledge controls."""
    return PaneState("alarm", {"show_history": history})


def SequenceList(history: bool = False) -> PaneState:  # noqa: N802
    """The per-channel sequence control list."""
    return PaneState("sequence", {"show_history": history})


def ComponentTable() -> PaneState:  # noqa: N802
    """Every component in the db as a flat table."""
    return PaneState("component_table", {})


def DataTable() -> PaneState:  # noqa: N802
    """One row per component, grouped by namespace, with live values."""
    return PaneState("data_table", {})


# ---------------------------------------------------------------------------
# Dashboard widgets and connectors
# ---------------------------------------------------------------------------
#
# A dashboard is one pane whose contents are free-placed at pixel rects rather
# than split, plus the connectors that turn those boxes into a diagram. Widget
# kinds are a separate namespace from pane kinds -- the same view can appear on
# both surfaces under different names -- so each widget below reports its own
# `_widget(namespace) -> (kind, state, default_size)`.


def _caller_dir() -> str:
    """Directory of the first stack frame outside this package.

    Asset paths in a target file are written relative to that file, not to
    whatever directory the host happened to be launched from.
    """
    frame: Any = sys._getframe(1)
    pkg_dir = os.path.dirname(os.path.abspath(__file__))
    while frame is not None:
        file = os.path.abspath(frame.f_code.co_filename)
        if os.path.dirname(file) != pkg_dir:
            return os.path.dirname(file)
        frame = frame.f_back
    return os.getcwd()


@dataclass(frozen=True)
class Meter:
    """A bar meter over one element. A scale spanning zero fills outward from
    zero, so a signed quantity reads correctly in both directions.

    Warn/critical ticks are not configured here: the panel reads them from the
    alarm definitions for the same element."""

    component: str
    element: int = 0
    min: float = 0.0
    max: float = 1.0
    unit: str | None = None
    label: str | None = None
    orientation: str = "vertical"
    color: str | None = None

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        state = _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "element": self.element,
                "min": float(self.min),
                "max": float(self.max),
                "unit": self.unit,
                "label": self.label,
                "orientation": _enum(self.orientation, ("vertical", "horizontal")),
                "color": self.color,
            }
        )
        size = (90.0, 200.0) if self.orientation.lower() == "vertical" else (220.0, 60.0)
        return "meter", state, size


@dataclass(frozen=True)
class Gauge:
    """A dial over one element. ``sweep`` is the total arc in degrees,
    symmetric about vertical; ``style`` is ``"arc"`` or ``"needle"``."""

    component: str
    element: int = 0
    min: float = 0.0
    max: float = 1.0
    unit: str | None = None
    label: str | None = None
    sweep: float = 240.0
    style: str = "arc"
    color: str | None = None

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        state = _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "element": self.element,
                "min": float(self.min),
                "max": float(self.max),
                "unit": self.unit,
                "label": self.label,
                "sweep_degrees": float(self.sweep),
                "style": _enum(self.style, ("arc", "needle")),
                "color": self.color,
            }
        )
        return "gauge", state, (160.0, 140.0)


@dataclass(frozen=True)
class State:
    """One row of a :class:`StateChip` table: the code, what it means, and
    optionally the colour it shows in."""

    value: float
    label: str
    color: str | None = None

    def _json(self) -> dict[str, Any]:
        return _drop_none(
            {"value": float(self.value), "label": self.label, "color": self.color}
        )


@dataclass(frozen=True)
class StateChip:
    """A numeric element shown as the state it means. ``unknown`` is displayed
    for a code the table does not list; empty shows the raw number."""

    component: str
    states: list[State]
    element: int = 0
    label: str | None = None
    unknown: str = ""

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        state = _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "element": self.element,
                "label": self.label,
                "states": [s._json() for s in self.states],
                "unknown_label": self.unknown,
            }
        )
        return "state_chip", state, (150.0, 60.0)


@dataclass(frozen=True)
class VectorMarker:
    """A body-frame 3-vector plotted on an :class:`Attitude` ball."""

    component: str
    label: str = ""
    color: str | None = None

    def _json(self, namespace: str | None) -> dict[str, Any]:
        return _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "label": self.label,
                "color": self.color,
            }
        )


@dataclass(frozen=True)
class Attitude:
    """An attitude ball over a four-element quaternion (``[x, y, z, w]``),
    with optional body-frame direction markers."""

    component: str
    vectors: list[VectorMarker] | None = None
    element_offset: int = 0
    label: str | None = None

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        state = _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "element_offset": self.element_offset,
                "label": self.label,
                "vectors": [v._json(namespace) for v in (self.vectors or [])],
            }
        )
        return "attitude", state, (220.0, 260.0)


@dataclass(frozen=True)
class SequenceControl:
    """Start/stop controls for one sequence channel. ``channel`` is the slot
    instance name, which is the address a command carries -- it is not
    namespace-qualified."""

    channel: str
    compact: bool = False

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        return (
            "sequence_control",
            {"channel": self.channel, "compact": self.compact},
            (260.0, 110.0),
        )


@dataclass(frozen=True)
class Image:
    """A static image, inlined into the preset at record time.

    ``path`` is resolved relative to the target file. The bytes travel with
    the preset because the panel may be running on a machine that has never
    seen the target's filesystem."""

    path: str
    _base_dir: str | None = None

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        import base64

        base = self._base_dir or _caller_dir()
        full = self.path if os.path.isabs(self.path) else os.path.join(base, self.path)
        with open(full, "rb") as f:
            data = base64.b64encode(f.read()).decode("ascii")
        return "image", {"path": self.path, "data": data}, (300.0, 200.0)


@dataclass(frozen=True, eq=False)
class Place:
    """One widget at a pixel rect on a :class:`Dashboard`.

    The object is also the handle a :class:`Connector` attaches to, mirroring
    how ``m.route`` refers to what ``m.add`` returned. Identity is the key, so
    two placements with equal fields stay distinct."""

    widget: Any
    x: float
    y: float
    w: float | None = None
    h: float | None = None

    def _rect(self, default: tuple[float, float]) -> dict[str, float]:
        return {
            "x": float(self.x),
            "y": float(self.y),
            "w": float(self.w if self.w is not None else default[0]),
            "h": float(self.h if self.h is not None else default[1]),
        }


@dataclass(frozen=True)
class At:
    """A free connector anchor at a canvas point."""

    x: float
    y: float


@dataclass(frozen=True)
class Edge:
    """A named side of a placed widget, when the automatic facing side is not
    what you want. ``t`` runs 0..1 along that side."""

    place: Place
    side: str = "bottom"
    t: float = 0.5


@dataclass(frozen=True)
class Bind:
    """Telemetry that colours a connector: on above ``threshold`` in
    magnitude, dimmed below it. This is what makes a pipe show flow."""

    component: str
    element: int = 0
    threshold: float = 0.0
    on_color: str | None = None

    def _json(self, namespace: str | None) -> dict[str, Any]:
        return _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "element": self.element,
                "threshold": float(self.threshold),
                "on_color": self.on_color,
            }
        )


@dataclass(frozen=True)
class Connector:
    """A line through two or more anchors: a :class:`Place`, an :class:`Edge`,
    or an :class:`At`.

    ``on_top`` picks which side of the widgets the line paints on -- leave it
    off for a schematic run, which should disappear into the box it enters,
    and set it for a callout leader that has to cross one."""

    points: list[Any]
    shape: str = "orthogonal"
    dashed: bool = False
    arrow: str = "none"
    color: str | None = None
    width: float = 1.5
    label: str = ""
    on_top: bool = False
    bind: Bind | None = None


_SIDES = ("top", "right", "bottom", "left")


def _enum(value: str, allowed: tuple[str, ...]) -> str:
    """A Rust unit enum variant from a lowercase spelling."""
    key = value.strip().lower()
    if key not in allowed:
        raise ValueError(f"expected one of {allowed}, got {value!r}")
    return key.capitalize() if key != "none" else "None"


def _center(rect: dict[str, float]) -> tuple[float, float]:
    return rect["x"] + rect["w"] / 2.0, rect["y"] + rect["h"] / 2.0


def _facing_side(rect: dict[str, float], toward: tuple[float, float]) -> tuple[str, float]:
    """The side of ``rect`` that faces ``toward``, and where along it to
    attach.

    A bare :class:`Place` used as a connector endpoint picks its side this
    way, so ``Connector([a, b])`` produces the line a person would have drawn
    without naming edges. ``t`` projects the target's position onto the side
    so the run stays as straight as the geometry allows.
    """
    cx, cy = _center(rect)
    dx, dy = toward[0] - cx, toward[1] - cy
    if abs(dx) >= abs(dy):
        side = "right" if dx >= 0 else "left"
        t = 0.5 if rect["h"] <= 0 else (toward[1] - rect["y"]) / rect["h"]
    else:
        side = "bottom" if dy >= 0 else "top"
        t = 0.5 if rect["w"] <= 0 else (toward[0] - rect["x"]) / rect["w"]
    return side, min(max(t, 0.0), 1.0)


@dataclass(frozen=True)
class Dashboard:
    """A pane of free-placed widgets plus the connectors between them.

    Widget ids are assigned in placement order; connectors refer to
    :class:`Place` handles rather than ids, so reordering the list cannot
    silently repoint a line."""

    widgets: list[Place]
    connectors: list[Connector] | None = None
    title: str = "Dashboard"

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("dashboard", self._state(namespace))._item(namespace)

    def _widget(self, namespace: str | None) -> tuple[str, dict[str, Any], tuple[float, float]]:
        raise TypeError("a Dashboard cannot be placed inside another Dashboard")

    def _state(self, namespace: str | None) -> dict[str, Any]:
        entries: list[dict[str, Any]] = []
        rects: dict[int, dict[str, float]] = {}
        ids: dict[int, int] = {}

        for index, place in enumerate(self.widgets, start=1):
            if not isinstance(place, Place):
                raise TypeError(
                    f"Dashboard widgets must be Place(...), got {type(place).__name__}"
                )
            kind, state, default = place.widget._widget(namespace)
            rect = place._rect(default)
            ids[id(place)] = index
            rects[id(place)] = rect
            entries.append(
                {
                    "id": index,
                    "rect": rect,
                    "kind": kind,
                    "config": json.dumps(state),
                }
            )

        lines = [
            self._connector(c, n, ids, rects, namespace)
            for n, c in enumerate(self.connectors or [], start=1)
        ]

        return {
            "title": self.title,
            "next_id": len(entries) + 1,
            "widgets": entries,
            "connectors": lines,
            "next_connector_id": len(lines) + 1,
        }

    def _connector(
        self,
        connector: Connector,
        index: int,
        ids: dict[int, int],
        rects: dict[int, dict[str, float]],
        namespace: str | None,
    ) -> dict[str, Any]:
        if len(connector.points) < 2:
            raise ValueError("a Connector needs at least two points")

        def resolved_center(point: Any) -> tuple[float, float]:
            """Where a point sits, for choosing a neighbour's facing side."""
            if isinstance(point, At):
                return float(point.x), float(point.y)
            place = point.place if isinstance(point, Edge) else point
            rect = rects.get(id(place))
            if rect is None:
                raise ValueError("connector refers to a widget not on this dashboard")
            return _center(rect)

        anchors: list[dict[str, Any]] = []
        for i, point in enumerate(connector.points):
            if isinstance(point, At):
                anchors.append({"Free": {"x": float(point.x), "y": float(point.y)}})
                continue
            place = point.place if isinstance(point, Edge) else point
            widget_id = ids.get(id(place))
            if widget_id is None:
                raise ValueError("connector refers to a widget not on this dashboard")
            if isinstance(point, Edge):
                side, t = point.side, point.t
            else:
                # Face the neighbour: the previous point for the last anchor,
                # the next one otherwise.
                neighbour = connector.points[i - 1] if i == len(connector.points) - 1 else connector.points[i + 1]
                side, t = _facing_side(rects[id(place)], resolved_center(neighbour))
            anchors.append(
                {
                    "Widget": {
                        "id": widget_id,
                        "side": _enum(side, _SIDES),
                        "t": float(t),
                    }
                }
            )

        style = _drop_none(
            {
                "color": connector.color,
                "width": float(connector.width),
                "dashed": connector.dashed,
                "shape": _enum(connector.shape, ("straight", "orthogonal", "curved")),
                "arrow": _enum(connector.arrow, ("none", "end", "both")),
                "label": connector.label,
                "on_top": connector.on_top,
                "bind": connector.bind._json(namespace) if connector.bind else None,
            }
        )
        return {"id": index, "points": anchors, "style": style}


@dataclass(frozen=True)
class Pane:
    """A tabbed pane holding one item per tab. Splits accept pane content
    directly, so an explicit ``Pane`` is only needed for tabs or chrome
    (``hide_tab_bar``)."""

    items: list[Any]
    active: int = 0
    hide_tab_bar: bool = False

    def _node(self, namespace: str | None) -> dict[str, Any]:
        return {
            "Pane": {
                "active_index": self.active,
                "hide_tab_bar": self.hide_tab_bar,
                "items": [i._item(namespace) for i in self.items],
            }
        }


def _as_node(child: Any, namespace: str | None) -> dict[str, Any]:
    """A split child: a nested split or pane serializes itself; bare pane
    content wraps into a single-tab pane."""
    if hasattr(child, "_node"):
        return child._node(namespace)
    return Pane([child])._node(namespace)


@dataclass(frozen=True)
class _Split:
    axis: str
    children: list[Any]
    flexes: list[float] | None = None

    def _node(self, namespace: str | None) -> dict[str, Any]:
        flexes = self.flexes or [1.0] * len(self.children)
        if len(flexes) != len(self.children):
            raise ValueError(
                f"{len(self.children)} split children but {len(flexes)} flexes"
            )
        return {
            "Split": {
                "axis": self.axis,
                "flexes": [float(f) for f in flexes],
                "children": [_as_node(c, namespace) for c in self.children],
            }
        }


def HSplit(*children: Any, flexes: list[float] | None = None) -> _Split:  # noqa: N802
    """Children side by side, weighted by ``flexes`` (equal when omitted)."""
    return _Split("Horizontal", list(children), flexes)


def VSplit(*children: Any, flexes: list[float] | None = None) -> _Split:  # noqa: N802
    """Children stacked vertically, weighted by ``flexes`` (equal when omitted)."""
    return _Split("Vertical", list(children), flexes)


@dataclass(frozen=True)
class Preset:
    """One named layout a target ships as a recommended default. ``layout``
    is a split tree, a pane, or bare pane content; ``time_range`` seeds the
    layout-wide window in the panel's time-range grammar."""

    name: str
    layout: Any
    time_range: str = ""

    def to_json(self, namespace: str | None) -> dict[str, Any]:
        return {
            "name": self.name,
            "layout": {
                "global_time_range": self.time_range,
                "root": _as_node(self.layout, namespace),
            },
        }


def Presets(presets: list[Preset]) -> Spec:  # noqa: N802 - a system-type wrapper
    """The built-in preset broadcaster, its ``PresetsParams`` carrying one
    entry per preset under the ``preset`` field the Rust struct declares.

    Component references are namespace-relative, like alarm targets: recording
    qualifies them with the target's namespace, so the ids match what the
    target registers. The ``Target`` must therefore exist first — the usual
    ``m.add("presets", Presets([...]))`` order."""
    namespace = _the_target().namespace
    return static_system("Presets", preset=[p.to_json(namespace) for p in presets])


# ---------------------------------------------------------------------------
# The target
# ---------------------------------------------------------------------------

_INIT_STATES = {"empty": "Empty", "loaded": "Loaded", "running": "Running"}


class Target:
    """A target under construction. Exactly one may exist at emission time.

    ``sim_dt`` (seconds) selects a free-running simulated clock; without it the
    loop paces a wall clock at ``cycle_rate``. ``default_depth`` is the in-flight
    record depth for a buffer with no rate hint. ``namespace`` is a bare dotted
    prefix (``"sat1"``) stamped onto every component name this target registers
    and announces, so several targets sharing one db keep disjoint id spaces;
    ``None`` leaves names and ids identical to an un-namespaced target. These
    are the only knobs ``CoordinatorSpec`` carries.
    """

    def __init__(
        self,
        cycle_rate: float,
        sim_dt: float | None = None,
        default_depth: int | None = None,
        namespace: str | None = None,
    ):
        self.cycle_rate = float(cycle_rate)
        self.sim_dt = sim_dt
        self.default_depth = default_depth
        self.namespace = _validate_namespace(namespace)
        self.coordinator = SystemHandle(COORDINATOR)
        self._artifacts: list[dict[str, Any]] = []
        self._states: list[dict[str, Any]] = []
        self._systems: list[dict[str, Any]] = []
        self._slots: list[dict[str, Any]] = []
        self._edges: list[dict[str, Any]] = []
        self._scopes: list[dict[str, Any]] = []
        self._scope_stack: list[int] = []
        self._names: set[str] = set()
        _targets.append(self)

    # -- artifacts ----------------------------------------------------------

    def artifact(self, id: str, crate: str, lib: str) -> ArtifactHandle:
        """Declare a loadable pack cdylib and return its entry-callable handle."""
        self._artifacts.append(
            {
                "id": id,
                "crate_name": crate,
                "lib": lib,
                "path": None,
                "prebuilt_dir": None,
                "dist": None,
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

    def _register_spec_artifact(self, spec: Spec) -> None:
        """Auto-register a generated :class:`System`'s declaring artifact, so a
        target never has to spell ``m.artifact(...)`` for a stubbed pack."""
        decl = getattr(spec, "artifact_decl", None)
        if decl is not None:
            self._register_artifact(decl)

    def _register_artifact(self, decl: "Artifact") -> None:
        """Record an :class:`Artifact`, deduped by id. A second declaration of
        the same id must agree on crate and lib (conflicting definitions are an
        eval-time error); a later declaration may supply the manifest hash an
        earlier one lacked. A prebuilt pack built against a different FSW ABI
        than the evaluating host expects is refused here, naming the pack —
        the record-time tier of the three-layer ABI gate
        (see ``docs/packaging.md``)."""
        expected = os.environ.get("METOR_EXPECTED_ABI")
        if (
            expected is not None
            and decl.abi_version is not None
            and decl.abi_version != int(expected)
        ):
            raise ValueError(
                f"pack {decl.dist or decl.id!r} was built for FSW ABI "
                f"{decl.abi_version}, but this metor-fsw expects ABI {expected}; "
                "rebuild the pack or match the metor-fsw version"
            )
        for existing in self._artifacts:
            if existing["id"] != decl.id:
                continue
            if existing["crate_name"] != decl.crate or existing["lib"] != decl.lib:
                raise ValueError(
                    f"artifact {decl.id!r} is declared twice with different crate/lib"
                )
            if existing["manifest_hash"] is None:
                existing["manifest_hash"] = decl.manifest_hash
            return
        dist = (
            {"name": decl.dist, "version": decl.dist_version}
            if decl.dist is not None and decl.dist_version is not None
            else None
        )
        self._artifacts.append(
            {
                "id": decl.id,
                "crate_name": decl.crate,
                "lib": decl.lib,
                "path": None,
                "prebuilt_dir": decl.prebuilt,
                "dist": dist,
                "manifest_hash": decl.manifest_hash,
                "src": _source_ref(),
            }
        )

    # -- systems and slots --------------------------------------------------

    def state(self, name: str, spec: Spec) -> StateHandle:
        """Declare a pack-shared state instance (``link = m.state("link",
        TcpServer(addr="0.0.0.0:2240"))``): constructed once, before any
        system, from its own params. States live in their own namespace and
        take no edges. The returned handle is passed to a shared-state
        system's constructor (``Downlink(link)``) to attach it."""
        if any(s["name"] == name or s["ty"] == spec.ty for s in self._states):
            raise ValueError(f"state {name!r} (type {spec.ty!r}) is already declared")
        self._states.append(
            {
                "name": name,
                "ty": spec.ty,
                "params": spec._param_source(),
                "src": _source_ref(),
            }
        )
        return StateHandle(name)

    def add(self, name: str, spec: H, process: bool = False) -> H:
        """Register ``spec`` under ``name`` (scope-prefixed) and return its handle.

        The return is typed as the spec's own class so a generated entry's port
        attributes (``plant.sensors``) are checkable; at runtime it is a
        :class:`SystemHandle` whose attribute access yields :class:`PortRef`s.
        A generated :class:`System` also auto-registers its artifact."""
        full, scope = self._scoped(name)
        self._claim(full)
        self._register_spec_artifact(spec)
        self._systems.append(
            {
                "name": full,
                "ty": spec.ty,
                "artifact": spec.artifact,
                "params": spec._param_source(),
                "process": bool(process),
                "src": _source_ref(),
                "scope": scope,
                "attach": spec.attach,
            }
        )
        return cast(H, SystemHandle(full))

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
        for occupant in allow:
            self._register_spec_artifact(occupant)
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

    def connect(self, src: OutPort[F], dst: InPort[F], delayed: bool = False) -> None:
        """Record a component-frame edge from ``src`` to ``dst``. The shared
        frame parameter ``F`` makes a cross-frame connection a pyright error;
        at runtime both ends are :class:`PortRef`s (same ``instance``/``port``
        fields the annotations declare)."""
        self._edge(src, dst, "Frame", bool(delayed))

    def route(self, src: "SystemHandle | Spec", dst: "SystemHandle | Spec", msg: str) -> None:
        """Record a message edge carrying ``msg`` from ``src`` to ``dst``. A
        message edge is log-delivery pub/sub, so it has no ``delayed`` form.
        Endpoints are handles (from :meth:`add`/:meth:`slot`, or
        :attr:`coordinator`); a generated entry's handle is typed as its class
        but is a :class:`SystemHandle` at runtime, so its ``name`` is read
        dynamically."""
        self._edges.append(
            {
                "from": _handle_name(src),
                "out": msg,
                "to": _handle_name(dst),
                "in_": msg,
                "delayed": False,
                "kind": "Msg",
                "src": _source_ref(),
            }
        )

    def _edge(self, src: OutPort[F], dst: InPort[F], kind: str, delayed: bool) -> None:
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
        """The serialized ``Wiring`` this target describes."""
        return {
            "ir_version": IR_VERSION,
            "metor_config_version": __version__,
            "coordinator": {
                "cycle_rate": self.cycle_rate,
                "default_depth": self.default_depth,
                "clock": self._clock(),
                "namespace": self.namespace,
            },
            "artifacts": self._artifacts,
            "states": self._states,
            "systems": self._systems,
            "slots": self._slots,
            "edges": self._edges,
            "scopes": self._scopes,
        }


# ---------------------------------------------------------------------------
# Emission
# ---------------------------------------------------------------------------


def _the_target() -> Target:
    if len(_targets) != 1:
        raise RuntimeError(
            f"exactly one Target must exist at emission, found {len(_targets)}"
        )
    return _targets[0]


def emit(target: Target | None = None) -> None:
    """Write the target IR to ``$METOR_IR_OUT`` (stdout if unset)."""
    ir = (target or _the_target()).to_ir()
    text = json.dumps(ir, indent=2)
    out = os.environ.get("METOR_IR_OUT")
    if out:
        with open(out, "w", encoding="utf-8") as f:
            f.write(text)
    else:
        sys.stdout.write(text)


def _emit_at_exit() -> None:
    # Emit only for a well-formed single-target module; a broken module has
    # already raised, and its traceback is the error surface.
    if len(_targets) == 1:
        emit(_targets[0])


atexit.register(_emit_at_exit)
