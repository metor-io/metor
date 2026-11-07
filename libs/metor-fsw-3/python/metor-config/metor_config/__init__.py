"""Describe metor-fsw-3 targets in Python.

A target file creates a `Target` and adds systems to it from pack modules.
A `composite` function adds several systems as one reusable piece.
For several targets that talk to each other, it creates one `Target` for
each and groups them in a `Deployment`. When the file finishes, the config
is written to `$METOR_CONFIG_OUT`, or to stdout, for `metor run` to read.
"""

from ._builtins import (
    ChannelRegistry,
    Connect,
    Db,
    DlSlot,
    Listen,
    Mux,
    Peer,
    Publish,
    Subscribe,
    Transport,
)
from ._cal import Cal, Calibrated
from ._composite import Composite, Scope, composite
from ._config import ConfigError
from ._deployment import Deployment, emit
from ._recorder import Recorder, recorder
from ._fields import ArrayField, Field
from ._generated import (
    CONFIG_VERSION,
    ChannelMessage,
    ChannelStatus,
    DbStatus,
    LinkStatus,
    LogEvent,
    ResourceStatus,
    SequenceCommand,
    SequenceRegistry,
    SystemStatus,
)
from ._io import (
    Can,
    Dir,
    File,
    GpioLine,
    Kind,
    ResourceHandle,
    SimGpioLine,
    SimKind,
    SimUart,
    TcpDial,
    TcpListen,
    Tty,
    Udp,
)
from ._model import (
    Item,
    Loop,
    OutGroup,
    OutPort,
    Pack,
    PortGroup,
    PortRef,
    Record,
    Source,
    Sources,
    System,
)
from ._target import SystemHandle, Target

__all__ = [
    "Can",
    "Composite",
    "Scope",
    "composite",
    "Dir",
    "File",
    "GpioLine",
    "Kind",
    "ResourceHandle",
    "ResourceStatus",
    "TcpDial",
    "TcpListen",
    "SimGpioLine",
    "SimKind",
    "SimUart",
    "Tty",
    "Udp",
    "ChannelMessage",
    "ChannelRegistry",
    "ChannelStatus",
    "DlSlot",
    "SequenceCommand",
    "SequenceRegistry",
    "SystemStatus",
    "LogEvent",
    "LinkStatus",
    "DbStatus",
    "Db",
    "CONFIG_VERSION",
    "ConfigError",
    "Connect",
    "Deployment",
    "ArrayField",
    "Cal",
    "Calibrated",
    "Field",
    "Item",
    "Listen",
    "Loop",
    "Mux",
    "OutGroup",
    "OutPort",
    "Pack",
    "Peer",
    "PortGroup",
    "PortRef",
    "Publish",
    "Record",
    "Recorder",
    "recorder",
    "Source",
    "Sources",
    "Subscribe",
    "System",
    "SystemHandle",
    "Target",
    "Transport",
    "emit",
]
