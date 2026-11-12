"""Calibrations a `Mux` mapping applies, from a scalar ``f64`` to a scalar ``f64``."""

from __future__ import annotations

import math
from collections.abc import Sequence
from dataclasses import dataclass
from itertools import pairwise
from typing import Any, Literal

from ._config import ConfigError
from ._fields import Field

KINDS = ("K", "J", "T", "E")


@dataclass(frozen=True)
class Cal:
    """A calibration, as the `CalParam` the host reads.

    ``_param`` is the param's JSON, with the excitation of a ratiometric
    calibration kept as a `Field` until `to_json`.
    """

    _param: Any
    __hash__ = None  # type: ignore[assignment]

    @staticmethod
    def poly(coeffs: Sequence[float]) -> Cal:
        """Ascending coefficients: ``c0 + c1·x + ..``."""
        values = _finite("poly", coeffs)
        if not values:
            raise ConfigError("`Cal.poly` takes at least one coefficient")
        return Cal({"poly": values})

    @staticmethod
    def linear(*, gain: float, offset: float) -> Cal:
        """``offset + gain·x``."""
        return Cal.poly([offset, gain])

    @staticmethod
    def two_point(*, volts: tuple[float, float], eng: tuple[float, float]) -> Cal:
        """The line through ``(volts[0], eng[0])`` and ``(volts[1], eng[1])``."""
        (v0, v1), (e0, e1) = _finite("two_point", volts, 2), _finite("two_point", eng, 2)
        if v0 == v1:
            raise ConfigError(f"`Cal.two_point` takes two distinct volts, got {v0} twice")
        gain = (e1 - e0) / (v1 - v0)
        return Cal.linear(gain=gain, offset=e0 - gain * v0)

    @staticmethod
    def table(points: Sequence[tuple[float, float]]) -> Cal:
        """``(x, y)`` points, interpolated linearly and clamped at the ends."""
        pairs = sorted(tuple(_finite("table", p, 2)) for p in points)
        if not pairs:
            raise ConfigError("`Cal.table` takes at least one point")
        for (a, _), (b, _) in pairwise(pairs):
            if a == b:
                raise ConfigError(f"`Cal.table` has two points at x = {a}")
        return Cal({"table": [list(p) for p in pairs]})

    @staticmethod
    def thermocouple(kind: Literal["K", "J", "T", "E"]) -> Cal:
        """The NIST ITS-90 inverse: cold-junction-compensated volts to °C."""
        if kind not in KINDS:
            raise ConfigError(f"`Cal.thermocouple` takes one of {KINDS}, got {kind!r}")
        return Cal({"thermocouple": {"kind": kind.lower()}})

    @staticmethod
    def ratiometric(
        *,
        excitation: Field[Literal["f64"]],
        span: tuple[float, float],
        eng: tuple[float, float],
    ) -> Cal:
        """``x / excitation`` mapped from ``span`` to ``eng``.

        ``excitation`` is a scalar ``f64`` field of the source's port.
        """
        if not isinstance(excitation, Field) or excitation.signature != "f64":
            raise ConfigError(f"`Cal.ratiometric` takes an f64 excitation, got {excitation!r}")
        lo, hi = _finite("ratiometric", span, 2)
        if lo == hi:
            raise ConfigError(f"`Cal.ratiometric` takes a span of nonzero width, got {span}")
        param = {"excitation": excitation, "span": [lo, hi], "eng": _finite("ratiometric", eng, 2)}
        return Cal({"ratiometric": param})

    @property
    def excitation(self) -> Field[Any] | None:
        """The extra field a ratiometric calibration reads."""
        ratio = self._param.get("ratiometric")
        return ratio["excitation"] if ratio else None

    def to_json(self) -> Any:
        ratio = self._param.get("ratiometric")
        if not ratio:
            return self._param
        return {"ratiometric": {**ratio, "excitation": ratio["excitation"].path}}


@dataclass(frozen=True)
class Calibrated:
    """A `Mux` mapping value: ``source`` through ``cal``."""

    source: Field[Literal["f64"]]
    cal: Cal
    __hash__ = None  # type: ignore[assignment]


def _finite(name: str, values: Sequence[float], count: int | None = None) -> list[float]:
    """``values`` as floats; refuses a non-number, a NaN or inf, and a wrong count."""
    if not isinstance(values, (list, tuple)):
        raise ConfigError(f"`Cal.{name}` takes a list of numbers, got {values!r}")
    if count is not None and len(values) != count:
        raise ConfigError(f"`Cal.{name}` takes {count} numbers, got {values!r}")
    numbers = [v for v in values if isinstance(v, (int, float)) and not isinstance(v, bool)]
    out = [float(v) for v in numbers]
    if len(numbers) != len(values) or not all(math.isfinite(v) for v in out):
        raise ConfigError(f"`Cal.{name}` takes finite numbers, got {values!r}")
    return out
