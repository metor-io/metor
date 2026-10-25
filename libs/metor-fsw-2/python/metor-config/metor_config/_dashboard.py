"""Panel presets and dashboard widgets."""

from __future__ import annotations

import json
import os
import sys
from dataclasses import dataclass
from typing import Any
from ._model import _drop_none
from ._program import State


def component_id(name: str) -> int:
    """The numeric id of a fully-qualified component name — masked FNV-1a-64,
    mirroring the Rust ``ComponentId::new``."""
    h = 0xCBF29CE484222325
    for b in name.encode():
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h & ~(1 << 63)


def _qualify(name: str, namespace: str | None) -> str:
    return f"{namespace}.{name}" if namespace else name


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

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        # The dashboard names this kind `plot`, not `time_series_plot`.
        return "plot", self._state(namespace), (400.0, 250.0)


@dataclass(frozen=True)
class Monitor:
    """A component's latest value with its name and an optional sparkline.
    ``align`` is ``"left"``, ``"center"`` or ``"right"``; ``value_size`` is
    the value's text size in pixels, ``None`` for the automatic size."""

    component: str
    unit: str | None = None
    sparkline: bool | None = None
    align: str = "left"
    value_size: float | None = None

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("monitor", self._state(namespace))._item(namespace)

    def _state(self, namespace: str | None) -> dict[str, Any]:
        return _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "unit": self.unit,
                "show_sparkline": self.sparkline,
                "align": _enum(self.align, ("left", "center", "right")),
                "value_size": None
                if self.value_size is None
                else float(self.value_size),
            }
        )

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        return "monitor", self._state(namespace), (300.0, 160.0)


# The panel folded its text readout into the monitor; `Text` keeps older
# target files building.
Text = Monitor


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

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
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

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
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


_OUTLINE_COLUMNS = ("name", "unit", "type", "sparkline", "value")


_OUTLINE_SORTS = ("ascending", "descending")


def _qualify_all(paths: list[str] | None, namespace: str | None) -> list[str] | None:
    return None if paths is None else [_qualify(p, namespace) for p in paths]


@dataclass(frozen=True)
class Pivot:
    """A branch the outline shows as instances × fields. ``fields`` lead the
    columns in that order (unlisted ones follow in natural order), ``hidden``
    fields are left out, and ``rows`` lead the instances by their segment
    under the branch. Field names are leaf paths relative to an instance."""

    path: str
    fields: list[str] | None = None
    hidden: list[str] | None = None
    rows: list[str] | None = None

    def _state(self, namespace: str | None) -> dict[str, Any]:
        return _drop_none(
            {
                "path": _qualify(self.path, namespace),
                "fields": self.fields,
                "hidden": self.hidden,
                "rows": self.rows,
            }
        )


@dataclass(frozen=True)
class FrameType:
    """A shape the outline pivots across the whole namespace: every subtree
    whose leaf paths are exactly ``fields`` lands in one grid labelled
    ``label``. ``order``, ``hidden`` and ``rows`` arrange it like a
    :class:`Pivot`; ``rows`` are full instance paths."""

    label: str
    fields: list[str]
    order: list[str] | None = None
    hidden: list[str] | None = None
    rows: list[str] | None = None

    def _state(self, namespace: str | None) -> dict[str, Any]:
        return _drop_none(
            {
                "label": self.label,
                "fields": list(self.fields),
                "order": self.order,
                "hidden": self.hidden,
                "rows": _qualify_all(self.rows, namespace),
            }
        )


@dataclass(frozen=True)
class Outline:
    """The component outline: the namespace as a collapsible tree-table.

    ``root`` lists only that branch's children; ``columns`` are the visible
    columns in display order, from ``name``, ``unit``, ``type``,
    ``sparkline`` and ``value``; ``sort`` is ``"ascending"`` or
    ``"descending"``. ``expanded`` and ``collapsed`` name branches to open
    or fold away from the default (top level open, the rest folded).
    Paths are namespace-relative. Unset fields keep the panel's defaults."""

    root: str | None = None
    columns: list[str] | None = None
    sort: str | None = None
    filter: str | None = None
    filter_bar: bool | None = None
    expanded: list[str] | None = None
    collapsed: list[str] | None = None
    pivots: list[Pivot] | None = None
    types: list[FrameType] | None = None
    focus: str | None = None

    def __post_init__(self) -> None:
        for column in self.columns or ():
            if column not in _OUTLINE_COLUMNS:
                raise ValueError(f"unknown outline column {column!r}")
        if self.sort is not None and self.sort not in _OUTLINE_SORTS:
            raise ValueError(f"unknown outline sort {self.sort!r}")

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("component_outline", self._state(namespace))._item(namespace)

    def _state(self, namespace: str | None) -> dict[str, Any]:
        return _drop_none(
            {
                "root": _qualify(self.root, namespace) if self.root else None,
                "columns": self.columns,
                "sort": self.sort,
                "filter": self.filter,
                "filter_bar": self.filter_bar,
                "expanded": _qualify_all(self.expanded, namespace),
                "collapsed": _qualify_all(self.collapsed, namespace),
                "pivots": None
                if self.pivots is None
                else [p._state(namespace) for p in self.pivots],
                "types": None
                if self.types is None
                else [t._state(namespace) for t in self.types],
                "focus": self.focus,
            }
        )

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        return "table", self._state(namespace), (400.0, 300.0)


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

    ``shape`` is ``"bar"``, ``"tank"``, ``"cylinder"``, ``"sphere"`` or
    ``"battery"``; the fill always rises linearly along the meter's axis.

    Warn/critical ticks are not configured here: the panel reads them from the
    alarm definitions for the same element."""

    component: str
    element: int = 0
    min: float = 0.0
    max: float = 1.0
    unit: str | None = None
    label: str | None = None
    orientation: str = "vertical"
    shape: str = "bar"
    color: str | None = None

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        state = _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "element": self.element,
                "min": float(self.min),
                "max": float(self.max),
                "unit": self.unit,
                "label": self.label,
                "orientation": _enum(self.orientation, ("vertical", "horizontal")),
                "shape": _enum(
                    self.shape, ("bar", "tank", "cylinder", "sphere", "battery")
                ),
                "color": self.color,
            }
        )
        size = (
            (90.0, 200.0) if self.orientation.lower() == "vertical" else (220.0, 60.0)
        )
        return "meter", state, size


@dataclass(frozen=True)
class Gauge:
    """A dial over one element. ``sweep`` is the total arc in degrees and
    ``rotation`` turns its middle clockwise from straight up (``0`` is
    symmetric about vertical, ``-135`` faces lower-left). ``divisions`` major
    notches split the scale, with ``minor_ticks`` between each pair; ``0``
    divisions draws none. ``style`` is ``"arc"`` or ``"needle"``."""

    component: str
    element: int = 0
    min: float = 0.0
    max: float = 1.0
    unit: str | None = None
    label: str | None = None
    sweep: float = 240.0
    rotation: float = 0.0
    divisions: int = 5
    minor_ticks: int = 1
    style: str = "arc"
    color: str | None = None

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        state = _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "element": self.element,
                "min": float(self.min),
                "max": float(self.max),
                "unit": self.unit,
                "label": self.label,
                "sweep_degrees": float(self.sweep),
                "rotation_degrees": float(self.rotation),
                "divisions": int(self.divisions),
                "minor_ticks": int(self.minor_ticks),
                "style": _enum(self.style, ("arc", "needle")),
                "color": self.color,
            }
        )
        return "gauge", state, (160.0, 140.0)


@dataclass(frozen=True)
class StateChip:
    """A numeric or bool element shown as the state it means. ``unknown`` is
    displayed for a code the table does not list; empty shows the raw number.

    ``style="toggle"`` draws every state as a segment of a switch, and
    clicking one commands that state's value (when edits are unlocked and the
    panel is live). A toggle over a bool with no ``states`` offers OFF/ON.
    ``text_size`` sizes the chip or switch, padding included; 0 is automatic."""

    component: str
    states: list[State] | None = None
    element: int = 0
    label: str | None = None
    unknown: str = ""
    style: str = "chip"
    text_size: float = 0.0

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        state = _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "element": self.element,
                "label": self.label,
                "states": [s._json() for s in (self.states or [])],
                "unknown_label": self.unknown,
                "style": _variant_or_none(self.style, ("chip", "toggle")),
                "text_size": float(self.text_size) if self.text_size else None,
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

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
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
class Map:
    """A slippy map (OpenStreetMap tiles) plotting a lat/lon component --
    ``component`` holds latitude and longitude in degrees at ``lat_element``
    / ``lon_element`` (a ``[lat, lon, alt]`` triple by default)."""

    component: str
    lat_element: int = 0
    lon_element: int = 1
    zoom: float | None = None
    time_range: str | None = None

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("map", self._state(namespace))._item(namespace)

    def _state(self, namespace: str | None) -> dict[str, Any]:
        return _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "lat_element": self.lat_element,
                "lon_element": self.lon_element,
                "zoom": self.zoom,
                "time_range": self.time_range,
            }
        )

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        return "map", self._state(namespace), (400.0, 300.0)


@dataclass(frozen=True)
class SequenceControl:
    """Start/stop controls for one sequence channel. ``channel`` is the slot
    instance name, which is the address a command carries -- it is not
    namespace-qualified."""

    channel: str
    compact: bool = False

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        return (
            "sequence_control",
            {"channel": self.channel, "compact": self.compact},
            (340.0, 48.0),
        )


@dataclass(frozen=True)
class Image:
    """A static image, inlined into the preset at record time.

    ``path`` is resolved relative to the target file. The bytes travel with
    the preset because the panel may be running on a machine that has never
    seen the target's filesystem."""

    path: str
    _base_dir: str | None = None

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        import base64

        base = self._base_dir or _caller_dir()
        full = self.path if os.path.isabs(self.path) else os.path.join(base, self.path)
        with open(full, "rb") as f:
            data = base64.b64encode(f.read()).decode("ascii")
        return "image", {"path": self.path, "data": data}, (300.0, 200.0)


def _inline_image(path: str | None, base: str) -> dict[str, str] | None:
    """An image as the panel's ``{path, data}`` reference, its bytes inlined
    at record time (see :class:`Image`); ``None`` leaves the slot unset."""
    import base64

    if not path:
        return None
    full = path if os.path.isabs(path) else os.path.join(base, path)
    with open(full, "rb") as f:
        return {"path": path, "data": base64.b64encode(f.read()).decode("ascii")}


@dataclass(frozen=True)
class ImageState:
    """One row of a :class:`StateImage` table: the code and the image it
    shows. ``label`` is drawn instead when the image cannot be loaded."""

    value: float
    path: str
    label: str = ""


@dataclass(frozen=True)
class StateImage:
    """A numeric or bool element shown as the image its state names, with
    images inlined at record time like :class:`Image`. ``unknown`` is shown
    for a code the table does not list, ``no_data`` before any sample, and
    ``error`` when the binding is broken; an unset one falls back to text."""

    component: str
    states: list[ImageState]
    element: int = 0
    label: str | None = None
    unknown: str | None = None
    no_data: str | None = None
    error: str | None = None
    _base_dir: str | None = None

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        base = self._base_dir or _caller_dir()
        states = []
        for s in self.states:
            image = _inline_image(s.path, base) or {}
            states.append({"value": float(s.value), "label": s.label, **image})
        state = _drop_none(
            {
                "component": _qualify(self.component, namespace),
                "element": self.element,
                "label": self.label,
                "states": states,
                "unknown": _inline_image(self.unknown, base),
                "no_data": _inline_image(self.no_data, base),
                "error": _inline_image(self.error, base),
            }
        )
        return "state_image", state, (160.0, 120.0)


@dataclass(frozen=True)
class Label:
    """Static text in a value-cell box, for captions on a schematic.
    ``color`` tints the box (and its edge, with ``border``); colours are
    ``"#rrggbbaa"`` and unset ones follow the panel theme. ``align`` is
    ``"left"``, ``"center"`` or ``"right"``. ``background=False`` drops the
    box fill, leaving bare text."""

    text: str
    text_color: str | None = None
    color: str | None = None
    text_size: float = 12.0
    align: str = "center"
    bold: bool = False
    uppercase: bool = False
    background: bool = True
    border: bool = False

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("label", self._state())._item(namespace)

    def _state(self) -> dict[str, Any]:
        return _drop_none(
            {
                "text": self.text,
                "text_color": self.text_color,
                "color": self.color,
                "text_size": float(self.text_size),
                "align": _enum(self.align, ("left", "center", "right")),
                "bold": self.bold,
                "uppercase": self.uppercase,
                "background": self.background,
                "border": self.border,
            }
        )

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        return "label", self._state(), (120.0, 40.0)


@dataclass(frozen=True, eq=False)
class Place:
    """One widget at a pixel rect on a :class:`Dashboard`."""

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


def _enum(value: str, allowed: tuple[str, ...]) -> str:
    """A Rust unit enum variant from a lowercase spelling."""
    key = value.strip().lower()
    if key not in allowed:
        raise ValueError(f"expected one of {allowed}, got {value!r}")
    return key.capitalize() if key != "none" else "None"


def _variant_or_none(value: str, allowed: tuple[str, ...]) -> str | None:
    """Like :func:`_enum`, but the first (default) variant is left out of
    the blob so a preset that never chose one stays byte-identical."""
    variant = _enum(value, allowed)
    return None if variant == _enum(allowed[0], allowed) else variant


@dataclass(frozen=True)
class Dashboard:
    """A pane of free-placed widgets. Widget ids are assigned in placement
    order."""

    widgets: list[Place]
    title: str = "Dashboard"

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("dashboard", self._state(namespace))._item(namespace)

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
        raise TypeError("a Dashboard cannot be placed inside another Dashboard")

    def _state(self, namespace: str | None) -> dict[str, Any]:
        entries: list[dict[str, Any]] = []

        for index, place in enumerate(self.widgets, start=1):
            if not isinstance(place, Place):
                raise TypeError(
                    f"Dashboard widgets must be Place(...), got {type(place).__name__}"
                )
            kind, state, default = place.widget._widget(namespace)
            entries.append(
                {
                    "id": index,
                    "rect": place._rect(default),
                    "kind": kind,
                    "config": json.dumps(state),
                }
            )

        return {
            "title": self.title,
            "widgets": entries,
        }


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
