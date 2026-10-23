"""Proper motion and unit vectors."""

from __future__ import annotations

import warnings

import erfa
import numpy as np

MAS = np.pi / (180.0 * 3600.0 * 1000.0)
J2000_JD = 2451545.0
JULIAN_YEAR_DAYS = 365.25


def julian_year_jd(epoch_jyear: float) -> tuple[float, float]:
    """A Julian-year epoch as a two-part TDB Julian date."""
    return J2000_JD, (epoch_jyear - 2000.0) * JULIAN_YEAR_DAYS


def propagate(
    ra_deg: np.ndarray,
    dec_deg: np.ndarray,
    pmra_mas_yr: np.ndarray,
    pmdec_mas_yr: np.ndarray,
    parallax_mas: np.ndarray,
    rv_km_s: np.ndarray,
    epoch_from: np.ndarray | float,
    epoch_to: float,
) -> tuple[np.ndarray, np.ndarray]:
    """Moves stars from `epoch_from` to `epoch_to` (Julian years) by space motion.

    `pmra_mas_yr` is μα* = μα cos δ. A NaN pm, parallax or rv counts as 0.
    """
    ra = np.radians(np.asarray(ra_deg, dtype=np.float64))
    dec = np.radians(np.asarray(dec_deg, dtype=np.float64))
    pmra = np.nan_to_num(np.asarray(pmra_mas_yr, dtype=np.float64)) * MAS
    pmdec = np.nan_to_num(np.asarray(pmdec_mas_yr, dtype=np.float64)) * MAS
    px = np.nan_to_num(np.asarray(parallax_mas, dtype=np.float64)) / 1000.0
    rv = np.nan_to_num(np.asarray(rv_km_s, dtype=np.float64))
    from_a, from_b = julian_year_jd(np.asarray(epoch_from, dtype=np.float64))
    to_a, to_b = julian_year_jd(epoch_to)
    pmr = ra_rate(pmra, dec)
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", erfa.ErfaWarning)
        out = erfa.pmsafe(ra, dec, pmr, pmdec, px, rv, from_a, from_b, to_a, to_b)
    return np.degrees(out[0]) % 360.0, np.degrees(out[1])


def ra_rate(pmra_star: np.ndarray, dec: np.ndarray) -> np.ndarray:
    """dα/dt from μα* = μα cos δ. It is 0 exactly on a pole, where α is undefined."""
    cos_dec = np.cos(dec)
    on_pole = np.abs(cos_dec) < 1e-15
    return np.divide(pmra_star, np.where(on_pole, 1.0, cos_dec)) * ~on_pole


def unit(ra_deg: np.ndarray, dec_deg: np.ndarray) -> np.ndarray:
    """ICRS unit vectors, shape (n, 3)."""
    ra = np.radians(np.asarray(ra_deg, dtype=np.float64))
    dec = np.radians(np.asarray(dec_deg, dtype=np.float64))
    cos_dec = np.cos(dec)
    return np.stack([cos_dec * np.cos(ra), cos_dec * np.sin(ra), np.sin(dec)], axis=-1)
