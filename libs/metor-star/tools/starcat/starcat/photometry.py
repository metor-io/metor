"""Gaia DR3 photometry from Johnson-Cousins magnitudes, and the default band.

Coefficients are from Riello et al. 2021, A&A 649, A3, Table C.2
(Johnson-Cousins relationships). Each tuple is ascending powers of the colour.
"""

from __future__ import annotations

import numpy as np

# G - V as a function of V - I_C, valid for -0.4 < V - I_C < 5.0.
G_MINUS_V_FROM_V_I = (-0.01597, -0.02809, -0.2483, 0.03656, -0.002939)
# BP - RP as a function of V - I_C, valid for -0.4 < V - I_C < 5.0.
BP_RP_FROM_V_I = (-0.03298, 1.259, -0.1279, 0.01631)
# G - V as a function of BP - RP, valid for -0.5 < BP - RP < 5.0.
G_MINUS_V_FROM_BP_RP = (-0.02704, 0.01424, -0.2156, 0.01426)
# V - G as a function of BP - RP: the default band's fixed colour term.
G_TO_V = tuple(-c for c in G_MINUS_V_FROM_BP_RP)

V_I_RANGE = (-0.4, 5.0)
DEFAULT_BP_RP = 0.8


def poly(coeffs: tuple[float, ...], x: np.ndarray) -> np.ndarray:
    """Evaluates ascending-power `coeffs` at `x`."""
    return np.polynomial.polynomial.polyval(x, coeffs)


def hip_to_gaia(v: np.ndarray, v_i: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    """(G, BP - RP) from Johnson V and V - I_C.

    V - I_C is clipped to the relation's range. A NaN V - I_C takes BP - RP =
    `DEFAULT_BP_RP` and G from the BP - RP relation.
    """
    v = np.asarray(v, dtype=np.float64)
    v_i = np.asarray(v_i, dtype=np.float64)
    known = np.isfinite(v_i)
    x = np.clip(np.where(known, v_i, 0.0), *V_I_RANGE)
    g_from_vi = v + poly(G_MINUS_V_FROM_V_I, x)
    g_from_default = v + poly(G_MINUS_V_FROM_BP_RP, DEFAULT_BP_RP)
    g = np.where(known, g_from_vi, g_from_default)
    bp_rp = np.where(known, poly(BP_RP_FROM_V_I, x), DEFAULT_BP_RP)
    return g, bp_rp


def fill_bp_rp(bp_rp: np.ndarray) -> tuple[np.ndarray, int]:
    """Replaces NaN BP - RP with `DEFAULT_BP_RP`. Returns the count replaced."""
    bp_rp = np.asarray(bp_rp, dtype=np.float64)
    missing = ~np.isfinite(bp_rp)
    return np.where(missing, DEFAULT_BP_RP, bp_rp), int(missing.sum())
