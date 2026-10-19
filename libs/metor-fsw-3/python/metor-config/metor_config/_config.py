"""Config file constants and clock encoding."""

from __future__ import annotations

from typing import Any

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

