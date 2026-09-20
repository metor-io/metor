"""Build a metor-fsw-3 target and emit the config `metor run` reads.

A target file constructs one `Target`, adds the systems a pack module supplies,
and ends; the config lands in `$METOR_CONFIG_OUT` or on stdout.
"""

from ._builtins import Publish, Subscribe
from ._config import CONFIG_VERSION, ConfigError
from ._model import Item, Loop, OutPort, Pack, PortRef, Record, Source, Sources, System
from ._target import SystemHandle, Target, emit

__all__ = [
    "CONFIG_VERSION",
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
    "emit",
]
