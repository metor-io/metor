"""A deterministic 200-star synthetic catalog.

Run as a script to rewrite `libs/metor-star/tests/data/tiny.bin`.
"""

from __future__ import annotations

import sys
from pathlib import Path

import numpy as np
from astropy_healpix.healpy import vec2pix
from starcat import astrometry, build, fmt

TINY_BIN = Path(__file__).resolve().parents[3] / "tests" / "data" / "tiny.bin"
TINY_MAG_LIMIT = 12.0
N_GAIA = 192
SEED = 26

# Gaia rows with fixed roles.
POLE_N, POLE_S, RA_ZERO, RA_WRAP, BRIGHTEST = 0, 1, 2, 3, 4
XMATCHED = np.arange(10, 20)
COVERED = np.arange(20, 25)
DOUBLE = np.array([25, 26])
DG_REJECTED = 27
STILL = np.arange(20, 28)
# Hipparcos numbers.
HIP_BASE = 1000
HIP_DOUBLE = 1015
HIP_PARTNER_CUT = 1016
HIP_DG_REJECTED = 1017
HIP_BELOW_CUT = 1024
N_PLAIN_FILL = 6
N_FILL = N_PLAIN_FILL + 2


def gaia_source_id(ra_deg: np.ndarray, dec_deg: np.ndarray) -> np.ndarray:
    """Source ids that encode the level-12 nested HEALPix index, as Gaia's do."""
    u = astrometry.unit(ra_deg, dec_deg)
    level12 = np.asarray(vec2pix(4096, u[:, 0], u[:, 1], u[:, 2], nest=True), np.int64)
    return (level12 << 35) + np.arange(len(ra_deg))


def gaia_rows(rng: np.random.Generator) -> dict[str, np.ndarray]:
    n = N_GAIA
    ra = rng.uniform(0.0, 360.0, n)
    dec = np.degrees(np.arcsin(rng.uniform(-1.0, 1.0, n)))
    ra[[POLE_N, POLE_S, RA_ZERO, RA_WRAP]] = [10.0, 200.0, 0.0, 359.9999999]
    dec[[POLE_N, POLE_S, RA_ZERO, RA_WRAP]] = [90.0, -90.0, 12.0, -30.0]
    g = rng.uniform(0.0, TINY_MAG_LIMIT, n)
    g[BRIGHTEST] = -1.5
    rows = {
        "source_id": gaia_source_id(ra, dec),
        "ra": ra,
        "dec": dec,
        "pmra": rng.normal(0.0, 50.0, n),
        "pmdec": rng.normal(0.0, 50.0, n),
        "parallax": rng.uniform(0.1, 50.0, n),
        "radial_velocity": np.where(rng.uniform(size=n) < 0.2, rng.normal(0, 30, n), np.nan),
        "ref_epoch": np.full(n, 2016.0),
        "phot_g_mean_mag": g,
        "bp_rp": rng.normal(0.8, 0.5, n),
    }
    for col in ("pmra", "pmdec", "parallax", "bp_rp"):
        rows[col][rng.choice(np.arange(30, n), 8, replace=False)] = np.nan
    for col in ("pmra", "pmdec"):
        rows[col][STILL] = 0.0
    rows["ra"][DOUBLE[1]] = ra[DOUBLE[0]]
    rows["dec"][DOUBLE[1]] = dec[DOUBLE[0]] + 1.0 / 3600.0
    rows["source_id"][DOUBLE[1]] = gaia_source_id(ra[DOUBLE[1:]], dec[DOUBLE[1:]])[0] + DOUBLE[1]
    g[DOUBLE] = 5.0
    g[DG_REJECTED] = 9.0
    return rows


def hip_rows(rng: np.random.Generator, gaia: dict[str, np.ndarray]) -> dict[str, np.ndarray]:
    """HIP 1000-1009 crossmatched, 1010-1014 covered by one Gaia row, 1015 by
    the double, 1016 crossmatched to a source below the cut, 1017 crossmatched
    with a 5 mag disagreement, then plain fill, 1024 with an agreeing partner
    below the cut, and one without a position."""
    single = np.concatenate([XMATCHED, COVERED])
    n_random = N_PLAIN_FILL + 4
    ra = np.concatenate(
        [gaia["ra"][single], [gaia["ra"][DOUBLE[0]]], rng.uniform(0.0, 360.0, n_random)]
    )
    mid_dec = gaia["dec"][DOUBLE].mean()
    dec = np.concatenate([gaia["dec"][single], [mid_dec], rng.uniform(-80.0, 80.0, n_random)])
    ra[2 + len(single)] = gaia["ra"][DG_REJECTED]
    dec[2 + len(single)] = gaia["dec"][DG_REJECTED]
    g = gaia["phot_g_mean_mag"]
    double_g = -2.5 * np.log10(2 * 10 ** (-0.4 * 5.0))
    vmag = np.concatenate([g[single], [double_g], rng.uniform(1, 9, n_random)]) + 0.01597
    vmag[2 + len(single)] = g[DG_REJECTED] - 5.0
    vmag[-2] = 7.0 + 0.01597  # its agreeing partner is below the cut
    ra[-1] = np.nan  # no astrometric solution
    n = len(ra)
    return {
        "hip": HIP_BASE + np.arange(n),
        "ra": ra,
        "de": dec,
        "pmra": np.zeros(n),
        "pmde": np.zeros(n),
        "plx": np.full(n, np.nan),
        "vmag": vmag,
        "v_i": np.zeros(n),
    }


def xmatch_rows(gaia: dict[str, np.ndarray]) -> dict[str, np.ndarray]:
    sid, g = gaia["source_id"], gaia["phot_g_mean_mag"]
    absent = sid.max() + np.array([1, 2])
    return {
        "source_id": np.array([*sid[XMATCHED], absent[0], sid[DG_REJECTED], absent[1]]),
        "original_ext_source_id": np.array(
            [
                *(HIP_BASE + np.arange(len(XMATCHED))),
                HIP_PARTNER_CUT,
                HIP_DG_REJECTED,
                HIP_BELOW_CUT,
            ]
        ),
        "phot_g_mean_mag": np.array([*g[XMATCHED], 20.9, g[DG_REJECTED], 7.0]),
    }


def inputs() -> tuple[dict, dict, dict]:
    rng = np.random.default_rng(SEED)
    gaia = gaia_rows(rng)
    return gaia, hip_rows(rng, gaia), xmatch_rows(gaia)


def tiny() -> tuple[fmt.Catalog, build.Log]:
    gaia, hip, xmatch = inputs()
    return build.build(gaia, hip, xmatch, TINY_MAG_LIMIT)


if __name__ == "__main__":
    cat, log = tiny()
    data = fmt.write(TINY_BIN, cat)
    print(log)
    print(f"sha256: {build.sha256(data)}", file=sys.stderr)
