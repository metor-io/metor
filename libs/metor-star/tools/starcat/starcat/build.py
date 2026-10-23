"""Builds `starcat.bin` from Gaia DR3 with a Hipparcos bright-end fill.

A Gaia row takes its HIP number from `hipparcos2_best_neighbour` when the two
G agree within `XMATCH_MAX_DG`. A HIP whose agreeing partner is fainter than
the cut is left out. Any other Hipparcos row is covered by the Gaia
rows near it when their summed flux matches (`cover`), and gives its number to
the brightest free one. The rest, down to the magnitude limit, are the fill.
"""

from __future__ import annotations

import argparse
import hashlib
from dataclasses import dataclass, field, fields
from pathlib import Path

import numpy as np

from starcat import astrometry, fetch, fmt, photometry, sky

NO_HIP = 0xFFFFFFFF
EPOCH_JYEAR = 2024.0
HIPPARCOS_EPOCH = 1991.25
MATCH_EPOCH = 2016.0
XMATCH_MAX_DG = 1.5
COVER_RADIUS_ARCSEC = 3.0
COVER_RADIUS = np.radians(COVER_RADIUS_ARCSEC / 3600.0)
COVER_MAX_DG = 1.5
SAME_RADIUS_ARCSEC = 0.5
SAME_RADIUS = np.radians(SAME_RADIUS_ARCSEC / 3600.0)
SAME_MAX_DG = 4.0


@dataclass
class Stars:
    """Per-star columns before propagation. `source_id` is 0 for Hipparcos rows."""

    source_id: np.ndarray
    hip: np.ndarray
    ra: np.ndarray
    dec: np.ndarray
    pmra: np.ndarray
    pmdec: np.ndarray
    parallax: np.ndarray
    rv: np.ndarray
    epoch: np.ndarray
    g: np.ndarray
    bp_rp: np.ndarray


@dataclass
class Log:
    counts: dict[str, int] = field(default_factory=dict)

    def __str__(self) -> str:
        return "\n".join(f"{k}: {v}" for k, v in self.counts.items())


def gaia_stars(gaia: dict[str, np.ndarray]) -> Stars:
    n = len(gaia["source_id"])
    return Stars(
        source_id=gaia["source_id"].astype(np.int64),
        hip=np.full(n, NO_HIP, np.uint32),
        ra=gaia["ra"],
        dec=gaia["dec"],
        pmra=gaia["pmra"],
        pmdec=gaia["pmdec"],
        parallax=gaia["parallax"],
        rv=gaia["radial_velocity"],
        epoch=gaia["ref_epoch"],
        g=gaia["phot_g_mean_mag"],
        bp_rp=gaia["bp_rp"],
    )


def hipparcos_stars(hip: dict[str, np.ndarray]) -> Stars:
    """Every Hipparcos row, with G and BP - RP from V and V - I."""
    n = len(hip["hip"])
    g, bp_rp = photometry.hip_to_gaia(hip["vmag"], hip["v_i"])
    return Stars(
        source_id=np.zeros(n, np.int64),
        hip=hip["hip"].astype(np.uint32),
        ra=hip["ra"],
        dec=hip["de"],
        pmra=hip["pmra"],
        pmdec=hip["pmde"],
        parallax=hip["plx"],
        rv=np.full(n, np.nan),
        epoch=np.full(n, HIPPARCOS_EPOCH),
        g=g,
        bp_rp=bp_rp,
    )


def has_position(stars: Stars) -> np.ndarray:
    return np.isfinite(stars.ra) & np.isfinite(stars.dec) & np.isfinite(stars.g)


def index_of(keys: np.ndarray, values: np.ndarray) -> np.ndarray:
    """The index in `keys` of each of `values`, or -1. `keys` must be unique."""
    order = np.argsort(keys, kind="stable")
    if len(keys) == 0:
        return np.full(len(values), -1, np.int64)
    i = np.searchsorted(keys[order], values).clip(max=len(keys) - 1)
    return np.where(keys[order][i] == values, order[i], -1)


def crossmatch(
    gaia: Stars, hip: Stars, xmatch: dict[str, np.ndarray]
) -> tuple[np.ndarray, np.ndarray, np.ndarray, int]:
    """Crossmatch pairs whose G agree within `XMATCH_MAX_DG`.

    Returns the kept pairs as (Gaia row, Hipparcos row), the Hipparcos rows
    whose agreeing partner is below the cut, and how many pairs disagree. A
    Gaia row in several pairs keeps the lowest HIP, and a HIP the lowest row.
    """
    h_row = index_of(hip.hip.astype(np.int64), xmatch["original_ext_source_id"].astype(np.int64))
    g_row = index_of(gaia.source_id, xmatch["source_id"].astype(np.int64))
    known = h_row >= 0
    agree = known & (np.abs(xmatch["phot_g_mean_mag"] - hip.g[h_row]) <= XMATCH_MAX_DG)
    below_cut = np.unique(h_row[agree & (g_row < 0)])
    kept = agree & (g_row >= 0)
    g_row, h_row = _first_per(g_row[kept], h_row[kept], hip.hip[h_row[kept]])
    h_row, g_row = _first_per(h_row, g_row, g_row)
    return g_row, h_row, below_cut, int((known & ~agree).sum())


def _first_per(key: np.ndarray, other: np.ndarray, rank: np.ndarray):
    """One (key, other) pair per key: the one with the lowest `rank`."""
    order = np.lexsort((rank, key))
    first = np.unique(key[order], return_index=True)[1]
    return key[order][first], other[order][first]


def cover(hip: Stars, gaia: Stars) -> list[np.ndarray]:
    """Per Hipparcos row, the Gaia rows covering it, or none. The row to carry
    its HIP comes first.

    Gaia rows within `COVER_RADIUS_ARCSEC` at `MATCH_EPOCH` cover a Hipparcos
    row when their summed flux is within `COVER_MAX_DG` of its G; the brightest
    comes first. A Gaia row within `SAME_RADIUS_ARCSEC` and `SAME_MAX_DG` is the
    same object, and comes first.
    """
    i, j, sep = sky.pairs_within(
        _unit_at(hip, MATCH_EPOCH), _unit_at(gaia, MATCH_EPOCH), COVER_RADIUS
    )
    dg = np.abs(gaia.g[j] - hip.g[i])
    same = (sep <= SAME_RADIUS) & (dg <= SAME_MAX_DG)
    flux = np.bincount(i, 10.0 ** (-0.4 * gaia.g[j]), minlength=len(hip.g))
    with np.errstate(divide="ignore"):
        summed = -2.5 * np.log10(flux)
    covered = np.abs(summed - hip.g) <= COVER_MAX_DG
    covered[i[same]] = True
    order = np.lexsort((gaia.g[j], ~same, i))
    i, j = i[order], j[order]
    starts = np.searchsorted(i, np.arange(len(hip.g) + 1))
    return [j[starts[k] : starts[k + 1]] if covered[k] else j[:0] for k in range(len(hip.g))]


def give_hip(gaia_hip: np.ndarray, components: list[np.ndarray], hip: np.ndarray) -> int:
    """Gives each HIP to its brightest component without one, lowest HIP first.

    Updates `gaia_hip` in place. Returns how many covered HIPs found no free component.
    """
    lost = 0
    for k in np.argsort(hip, kind="stable"):
        free = components[k][gaia_hip[components[k]] == NO_HIP]
        if len(free):
            gaia_hip[free[0]] = hip[k]
        elif len(components[k]):
            lost += 1
    return lost


def take(stars: Stars, mask: np.ndarray) -> Stars:
    return Stars(**{f.name: getattr(stars, f.name)[mask] for f in fields(Stars)})


def merge(a: Stars, b: Stars) -> Stars:
    return Stars(
        **{f.name: np.concatenate([getattr(a, f.name), getattr(b, f.name)]) for f in fields(Stars)}
    )


def _unit_at(stars: Stars, epoch: float) -> np.ndarray:
    ra, dec = astrometry.propagate(
        stars.ra, stars.dec, stars.pmra, stars.pmdec, stars.parallax, stars.rv, stars.epoch, epoch
    )
    return astrometry.unit(ra, dec)


def assemble(
    stars: Stars, epoch: float, mag_limit: float, sources: int, nside: int = sky.NSIDE
) -> tuple[fmt.Catalog, Log]:
    """Propagates, cells, sorts by (cell, G, source_id, hip) and packs `stars`."""
    log = Log()
    unit = _unit_at(stars, epoch)
    cell = sky.cell(unit, nside)
    bp_rp, log.counts["bp_rp_defaulted"] = photometry.fill_bp_rp(stars.bp_rp)
    g_mmag, log.counts["g_clamped"] = fmt.to_mmag(stars.g)
    bp_rp_mmag, log.counts["bp_rp_clamped"] = fmt.to_mmag(bp_rp)
    order = np.lexsort((stars.hip, stars.source_id, g_mmag, cell))
    cat = fmt.Catalog(
        nside=nside,
        epoch_jyear=epoch,
        mag_limit=mag_limit,
        sources=sources,
        cells=cell_table(cell, nside),
        stars=star_table(unit[order], g_mmag[order], bp_rp_mmag[order]),
        hip=hip_table(stars.hip[order]),
    )
    log.counts["stars"] = len(cat.stars)
    log.counts["hip"] = len(cat.hip)
    return cat, log


def cell_table(cell: np.ndarray, nside: int) -> np.ndarray:
    n = sky.n_cells(nside)
    centre, radius = sky.cell_bounds(nside)
    count = np.bincount(cell, minlength=n)
    table = np.zeros(n, fmt.CELL)
    table["centre"] = centre
    table["radius"] = radius
    table["offset"] = np.concatenate([[0], np.cumsum(count)[:-1]])
    table["count"] = count
    return table


def star_table(unit: np.ndarray, g_mmag: np.ndarray, bp_rp_mmag: np.ndarray) -> np.ndarray:
    table = np.zeros(len(unit), fmt.STAR)
    table["dir"] = unit
    table["mags"] = fmt.pack_mags(g_mmag, bp_rp_mmag)
    return table


def hip_table(hip_by_row: np.ndarray) -> np.ndarray:
    rows = np.flatnonzero(hip_by_row != NO_HIP)
    table = np.zeros(len(rows), fmt.HIP)
    table["row"] = rows
    table["hip"] = hip_by_row[rows]
    return table


def build(
    gaia: dict[str, np.ndarray],
    hip: dict[str, np.ndarray],
    xmatch: dict[str, np.ndarray],
    mag_limit: float,
    epoch: float = EPOCH_JYEAR,
) -> tuple[fmt.Catalog, Log]:
    g = gaia_stars(gaia)
    h = hipparcos_stars(hip)
    g_row, h_row, below_cut, rejected = crossmatch(g, h, xmatch)
    g.hip[g_row] = h.hip[h_row]
    rest = np.ones(len(h.g), bool)
    rest[h_row] = False
    rest[below_cut] = False
    cand = take(h, rest & has_position(h))
    components = cover(cand, g)
    lost = give_hip(g.hip, components, cand.hip)
    covered = np.array([len(c) > 0 for c in components], bool)
    fill = take(cand, ~covered & (cand.g <= mag_limit))
    sources = fmt.SOURCE_GAIA_DR3 | (fmt.SOURCE_HIPPARCOS if len(fill.hip) else 0)
    cat, log = assemble(merge(g, fill), epoch, mag_limit, sources)
    log.counts = {
        "gaia": len(g.g),
        "hip_crossmatched": len(g_row),
        "hip_crossmatch_dg_rejected": rejected,
        "hip_partner_below_cut": len(below_cut),
        "hip_covered": int(covered.sum()) - lost,
        "hip_covered_no_free_component": lost,
        "hip_no_position": int((rest & ~has_position(h)).sum()),
        "hipparcos_fill": len(fill.g),
        **log.counts,
    }
    return cat, log


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description="Build starcat.bin from the Gaia archive.")
    parser.add_argument("--mag-limit", type=float, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--cache", type=Path, default=None, help="raw query cache directory")
    parser.add_argument("--epoch", type=float, default=EPOCH_JYEAR)
    args = parser.parse_args(argv)
    cache = args.cache or fetch.cache_dir()
    cat, log = build(
        fetch.gaia(args.mag_limit, cache),
        fetch.hipparcos(cache),
        fetch.xmatch(cache),
        args.mag_limit,
        args.epoch,
    )
    data = fmt.write(args.out, cat)
    print(log)
    print(f"bytes: {len(data)}")
    print(f"sha256: {sha256(data)}")


if __name__ == "__main__":
    main()
