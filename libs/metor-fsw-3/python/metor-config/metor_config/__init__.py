"""Describe metor-fsw-3 targets in Python.

A target file creates a `Target` and adds systems to it from pack modules.
For several targets that talk to each other, it creates one `Target` for
each and groups them in a `Deployment`. When the file finishes, the config
is written to `$METOR_CONFIG_OUT`, or to stdout, for `metor run` to read.
"""

from ._builtins import (
    ChannelRegistry,
    Connect,
    Db,
    DbStatus,
    DlSlot,
    Listen,
    Peer,
    Publish,
    Subscribe,
    Transport,
)
from ._config import ConfigError
from ._deployment import Deployment, emit, gateway
from ._generated import (
    CONFIG_VERSION,
    ChannelStatus,
    LinkStatus,
    LogEvent,
    ResourceStatus,
    SequenceChannelEvent,
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
    TcpListen,
    Tty,
    Udp,
)
from ._model import (
    Item,
    Loop,
    OutPort,
    Pack,
    PortRef,
    Record,
    Source,
    Sources,
    System,
)
from ._target import SystemHandle, Target

__all__ = [
    "Can",
    "Dir",
    "File",
    "GpioLine",
    "Kind",
    "ResourceHandle",
    "ResourceStatus",
    "TcpListen",
    "SimGpioLine",
    "SimKind",
    "SimUart",
    "Tty",
    "Udp",
    "ChannelRegistry",
    "ChannelStatus",
    "DlSlot",
    "SequenceChannelEvent",
    "SequenceCommand",
    "SequenceRegistry",
    "gateway",
    "SystemStatus",
    "LogEvent",
    "LinkStatus",
    "DbStatus",
    "Db",
    "CONFIG_VERSION",
    "ConfigError",
    "Connect",
    "Deployment",
    "Item",
    "Listen",
    "Loop",
    "OutPort",
    "Pack",
    "Peer",
    "PortRef",
    "Publish",
    "Record",
    "Source",
    "Sources",
    "Subscribe",
    "System",
    "SystemHandle",
    "Target",
    "Transport",
    "emit",
]
