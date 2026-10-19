"""Describe metor-fsw-3 targets in Python.

A target file creates a `Target` and adds systems to it from pack modules.
`Target.to_config()` returns the target's config.
"""

from ._builtins import ChannelRegistry, DlSlot, LinkStatus, Publish, Subscribe
from ._config import ConfigError
from ._generated import (
    ChannelStatus,
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
    "SystemStatus",
    "LogEvent",
    "LinkStatus",
    "ConfigError",
    "Item",
    "Loop",
    "OutPort",
    "Pack",
    "PortRef",
    "Publish",
    "Record",
    "Source",
    "Sources",
    "Subscribe",
    "System",
    "SystemHandle",
    "Target",
]
