"""The config file's constants, clock encoding, and the ABI check."""

from __future__ import annotations

import os
from typing import Any

CONFIG_VERSION = 1

ABI_VERSION = 4
"""The host ABI the built-in systems are compiled against; a Rust test pins it."""

ABI_ENV = "METOR_FSW_ABI_VERSION"
OUT_ENV = "METOR_CONFIG_OUT"

NANOS_PER_SEC = 1_000_000_000


class ConfigError(Exception):
    """A target that cannot be turned into a config."""


def wall_clock(rate: float) -> dict[str, Any]:
    return {"Wall": {"rate": rate}}


def simulated_clock(dt: float) -> dict[str, Any]:
    """The simulated clock over a step of ``dt`` seconds, as serde reads a `Duration`."""
    if dt <= 0.0:
        raise ConfigError(f"sim_dt must be positive, got {dt}")
    nanos = round(dt * NANOS_PER_SEC)
    return {"Simulated": {"dt": {"secs": nanos // NANOS_PER_SEC, "nanos": nanos % NANOS_PER_SEC}}}


def check_abi(pack_id: str, abi_version: int) -> None:
    """Fail in the target file when the host's ABI differs from the pack module's."""
    host = os.environ.get(ABI_ENV)
    if host is None or int(host) == abi_version:
        return
    raise ConfigError(
        f"pack `{pack_id}` was generated for ABI {abi_version}, host ABI is {int(host)}; "
        "rebuild the pack with `metor pack dev`"
    )
