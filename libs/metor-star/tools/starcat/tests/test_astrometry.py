from __future__ import annotations

import numpy as np
import pytest
from starcat import astrometry

MAS_DEG = 1.0 / 3.6e6


def separation_mas(ra1, dec1, ra2, dec2) -> np.ndarray:
    a = astrometry.unit(ra1, dec1)
    b = astrometry.unit(ra2, dec2)
    chord = np.linalg.norm(a - b, axis=-1)
    return np.degrees(2.0 * np.arcsin(chord / 2.0)) * 3.6e6


def test_unit_axes():
    u = astrometry.unit(np.array([0.0, 90.0, 0.0]), np.array([0.0, 0.0, 90.0]))
    assert np.allclose(u, np.eye(3), atol=1e-15)
    assert np.allclose(np.linalg.norm(astrometry.unit([12.3], [-45.6]), axis=-1), 1.0)


def test_eight_years_matches_linear_formula():
    rng = np.random.default_rng(1)
    n = 2000
    ra = rng.uniform(0, 360, n)
    dec = rng.uniform(-80, 80, n)
    speed = rng.uniform(0, 100, n)
    angle = rng.uniform(0, 2 * np.pi, n)
    pmra, pmdec = speed * np.cos(angle), speed * np.sin(angle)
    parallax = rng.uniform(0.1, 10.0, n)
    ra2, dec2 = astrometry.propagate(ra, dec, pmra, pmdec, parallax, np.zeros(n), 2016.0, 2024.0)
    lin_dec = dec + pmdec * 8 * MAS_DEG
    lin_ra = ra + pmra * 8 * MAS_DEG / np.cos(np.radians(dec))
    assert separation_mas(ra2, dec2, lin_ra, lin_dec).max() < 1.0


def test_barnards_star_hipparcos_to_gaia():
    """HIP 87937 at J1991.25 carried to J2016.0 lands on Gaia DR3 4472832130942575872."""
    hip = {"ra": 269.45402305, "dec": 4.66828815, "pmra": -797.84, "pmdec": 10326.93, "plx": 549.01}
    gaia_ra, gaia_dec, gaia_rv = 269.44850252543836, 4.739420051112412, -110.46822
    args = (hip["pmra"], hip["pmdec"], hip["plx"], gaia_rv, 1991.25, 2016.0)
    ra, dec = astrometry.propagate(hip["ra"], hip["dec"], *args)
    assert separation_mas(ra, dec, gaia_ra, gaia_dec) < 100.0
    lin_dec = hip["dec"] + hip["pmdec"] * 24.75 * MAS_DEG
    lin_ra = hip["ra"] + hip["pmra"] * 24.75 * MAS_DEG / np.cos(np.radians(hip["dec"]))
    assert separation_mas(lin_ra, lin_dec, gaia_ra, gaia_dec) > 300.0


@pytest.mark.parametrize("missing", ["pm", "parallax", "all"])
def test_null_motion_counts_as_zero(missing):
    ra, dec = np.array([10.0, 200.0]), np.array([20.0, -60.0])
    pm = np.full(2, np.nan) if missing in ("pm", "all") else np.zeros(2)
    px = np.full(2, np.nan) if missing in ("parallax", "all") else np.full(2, 5.0)
    ra2, dec2 = astrometry.propagate(ra, dec, pm, pm, px, np.full(2, np.nan), 2016.0, 2024.0)
    assert np.all(np.isfinite(ra2)) and separation_mas(ra, dec, ra2, dec2).max() < 1e-6


def test_null_parallax_keeps_proper_motion():
    _, dec2 = astrometry.propagate([10.0], [0.0], [0.0], [100.0], [np.nan], [0.0], 2016.0, 2024.0)
    assert abs((dec2[0] - 0.0) / MAS_DEG - 800.0) < 1.0


def test_pole_star_moves_in_declination_only():
    ra2, dec2 = astrometry.propagate([0.0], [90.0], [50.0], [-100.0], [1.0], [0.0], 2016.0, 2024.0)
    assert np.all(np.isfinite([ra2, dec2]))
    assert abs((90.0 - dec2[0]) / MAS_DEG - 800.0) < 1.0


def test_ra_wraps_into_range():
    ra2, _ = astrometry.propagate([359.9999999], [0.0], [100.0], [0.0], [1.0], [0.0], 2016, 2024)
    assert 0.0 <= ra2[0] < 360.0
    assert separation_mas([359.9999999], [0.0], ra2, [0.0]) == pytest.approx(800.0, abs=1.0)


def test_mixed_epochs_per_star():
    _, dec2 = astrometry.propagate(
        [0.0, 0.0],
        [0.0, 0.0],
        [0.0, 0.0],
        [100.0, 100.0],
        [1.0, 1.0],
        [0, 0],
        [2016.0, 2020.0],
        2024.0,
    )
    assert np.allclose(dec2 / MAS_DEG, [800.0, 400.0], atol=1.0)


def test_julian_year():
    assert astrometry.julian_year_jd(2000.0) == (2451545.0, 0.0)
    assert sum(astrometry.julian_year_jd(2024.0)) == 2451545.0 + 24 * 365.25
