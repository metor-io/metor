"""Panel presets, dashboard widgets, and connectors."""

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
class Text:
    """A single component's latest value rendered as text."""

    component: str

    def _item(self, namespace: str | None) -> dict[str, Any]:
        return PaneState("component_text", self._state(namespace))._item(namespace)

    def _state(self, namespace: str | None) -> dict[str, Any]:
        return {"component": _qualify(self.component, namespace)}

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
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
                "color": self.color,
            }
        )
        size = (
            (90.0, 200.0) if self.orientation.lower() == "vertical" else (220.0, 60.0)
        )
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
                "style": _enum(self.style, ("arc", "needle")),
                "color": self.color,
            }
        )
        return "gauge", state, (160.0, 140.0)


@dataclass(frozen=True)
class StateChip:
    """A numeric element shown as the state it means. ``unknown`` is displayed
    for a code the table does not list; empty shows the raw number."""

    component: str
    states: list[State]
    element: int = 0
    label: str | None = None
    unknown: str = ""

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
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

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
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


def _facing_side(
    rect: dict[str, float], toward: tuple[float, float]
) -> tuple[str, float]:
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

    def _widget(
        self, namespace: str | None
    ) -> tuple[str, dict[str, Any], tuple[float, float]]:
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
            "widgets": entries,
            "connectors": lines,
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
                neighbour = (
                    connector.points[i - 1]
                    if i == len(connector.points) - 1
                    else connector.points[i + 1]
                )
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
