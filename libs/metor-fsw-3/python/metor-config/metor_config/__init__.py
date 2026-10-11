"""Build metor-fsw-3 targets and emit the config `metor run` reads.

A target file constructs one `Target`, or several and one `Deployment` of
them, adds the systems a pack module supplies, and ends; the config lands in
`$METOR_CONFIG_OUT` or on stdout.
"""

from ._builtins import Db, DbStatus, LinkStatus, Publish, Subscribe
from ._config import CONFIG_VERSION, ConfigError
from ._deployment import Deployment, emit, gateway
from ._model import (
    Item,
    LogEvent,
    Loop,
    OutPort,
    Pack,
    PortRef,
    Record,
    Source,
    Sources,
    System,
    SystemStatus,
)
from ._target import SystemHandle, Target

__all__ = [
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
