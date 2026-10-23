from __future__ import annotations

import numpy as np
import pytest
from starcat import photometry


def test_hip_to_gaia_at_zero_colour():
    g, bp_rp = photometry.hip_to_gaia(np.array([5.0]), np.array([0.0]))
    assert g[0] == pytest.approx(5.0 - 0.01597)
    assert bp_rp[0] == pytest.approx(-0.03298)


def test_hip_to_gaia_red_star():
    g, bp_rp = photometry.hip_to_gaia(np.array([9.54]), np.array([2.52]))
    x = 2.52
    assert g[0] == pytest.approx(
        9.54 - 0.01597 - 0.02809 * x - 0.2483 * x**2 + 0.03656 * x**3 - 0.002939 * x**4
    )
    assert bp_rp[0] == pytest.approx(-0.03298 + 1.259 * x - 0.1279 * x**2 + 0.01631 * x**3)


def test_hip_to_gaia_clips_colour():
    g_hi, _ = photometry.hip_to_gaia(np.array([5.0, 5.0]), np.array([5.0, 9.0]))
    g_lo, _ = photometry.hip_to_gaia(np.array([5.0, 5.0]), np.array([-0.4, -3.0]))
    assert g_hi[0] == g_hi[1] and g_lo[0] == g_lo[1]


def test_hip_to_gaia_missing_colour():
    g, bp_rp = photometry.hip_to_gaia(np.array([4.0]), np.array([np.nan]))
    assert bp_rp[0] == photometry.DEFAULT_BP_RP
    expected = 4.0 + photometry.poly(photometry.G_MINUS_V_FROM_BP_RP, photometry.DEFAULT_BP_RP)
    assert g[0] == pytest.approx(expected)


def test_g_to_v_negates_g_minus_v():
    c = np.linspace(-0.5, 5.0, 12)
    v_minus_g = photometry.poly(photometry.G_TO_V, c)
    assert np.allclose(v_minus_g, -photometry.poly(photometry.G_MINUS_V_FROM_BP_RP, c))
    assert photometry.poly(photometry.G_TO_V, 0.0) == pytest.approx(0.02704)


def test_fill_bp_rp():
    out, n = photometry.fill_bp_rp(np.array([0.1, np.nan, 2.0, np.nan]))
    assert out.tolist() == [0.1, photometry.DEFAULT_BP_RP, 2.0, photometry.DEFAULT_BP_RP]
    assert n == 2


def test_fill_bp_rp_empty():
    out, n = photometry.fill_bp_rp(np.zeros(0))
    assert len(out) == 0 and n == 0
