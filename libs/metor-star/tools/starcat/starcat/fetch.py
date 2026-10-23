"""Gaia archive queries and the raw-result cache.

Hipparcos (ESA 1997, VizieR I/239 `hip_main`) is read from the Gaia archive's
copy, `public.hipparcos`, so every query goes through one TAP service.
"""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

import numpy as np

from starcat.ensure import cache_root

GAIA_COLUMNS = (
    "source_id",
    "ra",
    "dec",
    "pmra",
    "pmdec",
    "parallax",
    "radial_velocity",
    "ref_epoch",
    "phot_g_mean_mag",
    "bp_rp",
)
HIP_COLUMNS = ("hip", "ra", "de", "pmra", "pmde", "plx", "vmag", "v_i")
XMATCH_COLUMNS = ("source_id", "original_ext_source_id", "phot_g_mean_mag")
INT_COLUMNS = {"source_id", "hip", "original_ext_source_id"}
LEVEL0_CELLS = 12
# The archive caps results at 50,000 rows unless MAXREC is set. A result this
# long is taken as truncated.
MAXREC = 2_000_000


class InputError(ValueError):
    pass


@dataclass(frozen=True)
class Query:
    columns: tuple[str, ...]
    table: str
    where: str = ""
    select_list: str = ""

    def select(self) -> str:
        fields = self.select_list or ", ".join(self.columns)
        return f"SELECT {fields} FROM {self.table}{self._where()}"

    def count(self) -> str:
        return f"SELECT COUNT(*) AS n FROM {self.table}{self._where()}"

    def _where(self) -> str:
        return f" WHERE {self.where}" if self.where else ""


def gaia_query(mag_limit: float, level0_cell: int) -> Query:
    where = (
        f"GAIA_HEALPIX_INDEX(0, source_id) = {int(level0_cell)} "
        f"AND phot_g_mean_mag <= {float(mag_limit)!r}"
    )
    return Query(GAIA_COLUMNS, "gaiadr3.gaia_source", where)


def hip_query() -> Query:
    return Query(HIP_COLUMNS, "public.hipparcos")


def xmatch_query() -> Query:
    """Crossmatch pairs with the Gaia G of each partner, kept or not."""
    table = (
        "gaiadr3.hipparcos2_best_neighbour AS b "
        "JOIN gaiadr3.gaia_source AS g ON b.source_id = g.source_id"
    )
    select = "b.source_id, b.original_ext_source_id, g.phot_g_mean_mag"
    return Query(XMATCH_COLUMNS, table, select_list=select)


def cache_dir() -> Path:
    return cache_root() / "raw"


def fetch(
    query: Query,
    path: Path,
    download: Callable[[str, Path], None] | None = None,
    count: Callable[[str], int] | None = None,
) -> dict[str, np.ndarray]:
    """Reads `path`, or first downloads `query` into it.

    A download is parsed and its row count checked against `query.count()`
    before it is moved into place. On failure it is deleted.
    """
    if path.exists():
        return read_csv(path, query.columns)
    download = download or _download
    count = count or _count
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_suffix(".partial")
    try:
        download(query.select(), partial)
        rows = read_csv(partial, query.columns)
        expected = count(query.count())
        found = len(rows[query.columns[0]])
        if found != expected:
            raise InputError(f"{path.name}: {found} rows, the archive counts {expected}")
    except BaseException:
        partial.unlink(missing_ok=True)
        raise
    partial.rename(path)
    return rows


def _download(adql: str, out: Path) -> None:
    from astroquery.gaia import Gaia
    from astroquery.utils.tap.core import TapPlus

    # Gaia's override drops `maxrec`, so this calls the TapPlus method.
    TapPlus.launch_job_async(
        Gaia, adql, output_file=str(out), output_format="csv", dump_to_file=True, maxrec=MAXREC
    )


def _count(adql: str) -> int:
    from astroquery.gaia import Gaia

    return int(Gaia.launch_job(adql).get_results()["n"][0])


def read_csv(path: Path, columns: tuple[str, ...]) -> dict[str, np.ndarray]:
    """Reads `columns` from a CSV. Empty float cells become NaN."""
    from astropy.table import Table

    try:
        table = Table.read(path, format="ascii.csv")
    except Exception as e:
        raise InputError(f"{path}: unreadable CSV: {e}") from e
    if len(table) >= MAXREC:
        raise InputError(f"{path}: {len(table)} rows reaches MAXREC, so it may be truncated")
    missing = [c for c in columns if c not in table.colnames]
    if missing:
        raise InputError(f"{path}: missing columns {missing}")
    return {c: _column(table[c], c) for c in columns}


def _column(col, name: str) -> np.ndarray:
    if name in INT_COLUMNS:
        if np.ma.is_masked(col):
            raise InputError(f"column {name} has empty cells")
        return np.asarray(col, dtype=np.int64)
    data = np.ma.asarray(col).astype(np.float64)
    return np.ma.filled(data, np.nan)


def gaia(mag_limit: float, cache: Path) -> dict[str, np.ndarray]:
    parts = [
        fetch(gaia_query(mag_limit, k), cache / f"gaia-g{mag_limit:g}-{k}.csv")
        for k in range(LEVEL0_CELLS)
    ]
    return {c: np.concatenate([p[c] for p in parts]) for c in GAIA_COLUMNS}


def hipparcos(cache: Path) -> dict[str, np.ndarray]:
    return fetch(hip_query(), cache / "hip_main.csv")


def xmatch(cache: Path) -> dict[str, np.ndarray]:
    return fetch(xmatch_query(), cache / "hip2_best_neighbour_g.csv")
