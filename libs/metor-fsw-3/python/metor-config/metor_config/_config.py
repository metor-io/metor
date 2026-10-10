"""Config file constants, clock encoding, and the pack ABI check."""

from __future__ import annotations

import os
from typing import Any

CONFIG_VERSION = 2

ABI_VERSION = 9
"""The host ABI version the built-in systems are compiled against.

A Rust test keeps this in sync with the host.
"""

ABI_ENV = "METOR_FSW_ABI_VERSION"
OUT_ENV = "METOR_CONFIG_OUT"

NANOS_PER_SEC = 1_000_000_000


class ConfigError(Exception):
    """Raised when a target cannot be turned into a valid config."""


def wall_clock(rate: float) -> dict[str, Any]:
    return {"Wall": {"rate": rate}}


def simulated_clock(dt: float, epoch: int | None = None) -> dict[str, Any]:
    """Encodes a simulated clock that advances ``dt`` seconds per cycle,
    starting at ``epoch`` microseconds since the Unix epoch when set.

    The step is encoded the way serde reads a Rust `Duration`.
    """
    if dt <= 0.0:
        raise ConfigError(f"sim_dt must be positive, got {dt}")
    nanos = round(dt * NANOS_PER_SEC)
    clock: dict[str, Any] = {
        "dt": {"secs": nanos // NANOS_PER_SEC, "nanos": nanos % NANOS_PER_SEC}
    }
    if epoch is not None:
        clock["epoch"] = epoch
    return {"Simulated": clock}


def check_abi(pack_id: str, abi_version: int) -> None:
    """Raises `ConfigError` if the host's ABI differs from the pack module's.

    This catches a stale pack module while the target file runs, instead of
    later when the host loads the pack.
    """
    host = os.environ.get(ABI_ENV)
    if host is None or int(host) == abi_version:
        return
    raise ConfigError(
        f"pack `{pack_id}` was generated for ABI {abi_version}, host ABI is {int(host)}; "
        "rebuild the pack with `metor pack dev`"
    )
