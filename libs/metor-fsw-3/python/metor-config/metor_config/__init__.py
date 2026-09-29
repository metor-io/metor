"""Describe metor-fsw-3 targets in Python.

A target file creates a `Target` and adds systems to it from pack modules.
For several targets that talk to each other, it creates one `Target` for
each and groups them in a `Deployment`. When the file finishes, the config
is written to `$METOR_CONFIG_OUT`, or to stdout, for `metor run` to read.
"""

from ._builtins import ChannelRegistry, Db, DbStatus, DlSlot, LinkStatus, Publish, Subscribe
from ._config import CONFIG_VERSION, ConfigError
from ._deployment import Deployment, emit, gateway
from ._model import (
    ChannelStatus,
    Item,
    LogEvent,
    Loop,
    OutPort,
    Pack,
    PortRef,
    Record,
    SequenceChannelEvent,
    SequenceCommand,
    SequenceRegistry,
    Source,
    Sources,
    System,
    SystemStatus,
)
from ._target import SystemHandle, Target

__all__ = [
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
    "Deployment",
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
    "emit",
]
