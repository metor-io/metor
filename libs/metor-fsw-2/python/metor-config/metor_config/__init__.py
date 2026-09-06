"""Build target wiring, capture Python systems, and describe panel layouts.

The public imports are stable; implementation lives in focused private modules.
"""

from ._model import (
    Spec as Spec,
    static_system as static_system,
    F as F,
    H as H,
    Msg as Msg,
    OutPort as OutPort,
    InPort as InPort,
    Artifact as Artifact,
    System as System,
    PortRef as PortRef,
    StateHandle as StateHandle,
    SystemHandle as SystemHandle,
)
from ._program import (
    Frame as Frame,
    Tensor as Tensor,
    f64 as f64,
    i64 as i64,
    ExprHandle as ExprHandle,
    system as system,
    node as node,
    State as State,
)
from ._builtins import (
    Component as Component,
    band as band,
    Alarm as Alarm,
    Alarms as Alarms,
    TcpServer as TcpServer,
    Uplink as Uplink,
    Downlink as Downlink,
)
from ._dashboard import (
    component_id as component_id,
    PaneState as PaneState,
    Trace as Trace,
    TimeSeriesPlot as TimeSeriesPlot,
    Text as Text,
    TrafficLight as TrafficLight,
    TrafficLightGrid as TrafficLightGrid,
    Logs as Logs,
    AlarmList as AlarmList,
    SequenceList as SequenceList,
    ComponentTable as ComponentTable,
    DataTable as DataTable,
    Pivot as Pivot,
    FrameType as FrameType,
    Outline as Outline,
    Meter as Meter,
    Gauge as Gauge,
    StateChip as StateChip,
    VectorMarker as VectorMarker,
    Attitude as Attitude,
    Map as Map,
    SequenceControl as SequenceControl,
    Image as Image,
    Place as Place,
    At as At,
    Edge as Edge,
    Bind as Bind,
    Connector as Connector,
    Dashboard as Dashboard,
    Pane as Pane,
    HSplit as HSplit,
    VSplit as VSplit,
    Preset as Preset,
)
from ._version import (
    __version__ as __version__,
    IR_VERSION as IR_VERSION,
    PROGRAM_ARTIFACT as PROGRAM_ARTIFACT,
    COORDINATOR as COORDINATOR,
)
from ._target import (
    Target as Target,
    emit as emit,
    Presets as Presets,
)

# Capture state remains available to the recorder test suite.
from ._program import _program as _program, _frames as _frames
from ._target import _targets as _targets
