"""The HEALPix cell index: nside 16, nested ordering."""

from __future__ import annotations

import astropy.units as u
import numpy as np
from astropy_healpix import boundaries_lonlat, neighbours
from astropy_healpix.healpy import pix2vec, vec2pix

NSIDE = 16
RADIUS_MARGIN_RAD = 1e-6
BOUNDARY_STEP = 8
# About 3.4 arcmin cells, far wider than any match radius.
MATCH_NSIDE = 1024
# Gaia source_id >> GAIA_SHIFT is the nside-16 nested index (level 4).
GAIA_SHIFT = 35 + 2 * (12 - 4)


def n_cells(nside: int = NSIDE) -> int:
    return 12 * nside * nside


def cell(unit: np.ndarray, nside: int = NSIDE) -> np.ndarray:
    """Nested cell index of each unit vector in `unit`, shape (n, 3)."""
    unit = np.asarray(unit, dtype=np.float64).reshape(-1, 3)
    return np.asarray(vec2pix(nside, unit[:, 0], unit[:, 1], unit[:, 2], nest=True), np.int64)


def gaia_cell(source_id: np.ndarray) -> np.ndarray:
    """The nside-16 nested cell encoded in a Gaia source_id."""
    return (np.asarray(source_id, dtype=np.uint64) >> np.uint64(GAIA_SHIFT)).astype(np.int64)


def cell_bounds(nside: int = NSIDE) -> tuple[np.ndarray, np.ndarray]:
    """Each cell's centre, shape (n, 3), and the largest angle from it to its
    boundary plus `RADIUS_MARGIN_RAD`."""
    index = np.arange(n_cells(nside))
    centre = np.stack(pix2vec(nside, index, nest=True), axis=-1)
    lon, lat = boundaries_lonlat(index, BOUNDARY_STEP, nside, order="nested")
    lon = lon.to_value(u.rad)
    lat = lat.to_value(u.rad)
    edge = np.stack([np.cos(lat) * np.cos(lon), np.cos(lat) * np.sin(lon), np.sin(lat)], axis=-1)
    cos_angle = np.clip(np.einsum("ck,cbk->cb", centre, edge), -1.0, 1.0)
    radius = np.arccos(cos_angle).max(axis=1) + RADIUS_MARGIN_RAD
    return centre, radius


def pairs_within(
    a: np.ndarray, b: np.ndarray, radius_rad: float, nside: int = MATCH_NSIDE
) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
    """Every (i, j, separation) with `a[i]` and `b[j]` at most `radius_rad` apart.

    `radius_rad` must be smaller than a cell at `nside`.
    """
    if len(a) == 0 or len(b) == 0:
        return np.zeros(0, np.int64), np.zeros(0, np.int64), np.zeros(0)
    b_cell = cell(b, nside)
    order = np.argsort(b_cell, kind="stable")
    b_sorted = b_cell[order]
    near = _cell_and_neighbours(cell(a, nside), nside)
    lo = np.searchsorted(b_sorted, near, side="left")
    hi = np.where(near >= 0, np.searchsorted(b_sorted, near, side="right"), lo)
    counts = (hi - lo).ravel()
    i = np.repeat(np.tile(np.arange(len(near[0])), len(near)), counts)
    within = np.arange(counts.sum()) - np.repeat(np.cumsum(counts) - counts, counts)
    j = order[np.repeat(lo.ravel(), counts) + within]
    sep = 2.0 * np.arcsin(np.linalg.norm(a[i] - b[j], axis=-1).clip(max=2.0) / 2.0)
    keep = sep <= radius_rad
    return i[keep], j[keep], sep[keep]


def _cell_and_neighbours(index: np.ndarray, nside: int) -> np.ndarray:
    """Shape (9, n): each cell, then its neighbours, with -1 where a cell has only 7."""
    with np.errstate(invalid="ignore"):
        ring = neighbours(index, nside, order="nested")
    return np.vstack([index[None, :], ring.reshape(8, -1)])
