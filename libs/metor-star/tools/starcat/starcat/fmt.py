"""Reads and writes `starcat.bin`. All fields are little-endian.

Layout:

- Header, 80 B (`HEADER`).
- Cells, 32 B each: centre f32x3, radius f32, offset u32, count u32, 8 B pad.
- Stars, 16 B each: direction f32x3, then G and BP - RP as i16 millimag
  packed into one u32 (G in the low half).
- HIP, 8 B each: (row u32, hip u32) pairs, sorted by row, one per star that
  has a Hipparcos number. About 118k of 3.1M stars have one at G <= 12, so
  this is 0.9 MB where one u32 per star would be 12 MB.
"""

from __future__ import annotations

from dataclasses import dataclass

import numpy as np

MAGIC = b"STARCAT\0"
VERSION = 1
SOURCE_GAIA_DR3 = 1 << 0
SOURCE_HIPPARCOS = 1 << 1

HEADER = np.dtype(
    [
        ("magic", "S8"),
        ("version", "<u4"),
        ("nside", "<u4"),
        ("n_cells", "<u4"),
        ("n_stars", "<u4"),
        ("epoch_jyear", "<f8"),
        ("mag_limit", "<f4"),
        ("sources", "<u4"),
        ("cells_off", "<u8"),
        ("stars_off", "<u8"),
        ("hip_off", "<u8"),
        ("n_hip", "<u4"),
        ("_pad", "<u4", (3,)),
    ]
)
CELL = np.dtype(
    [
        ("centre", "<f4", (3,)),
        ("radius", "<f4"),
        ("offset", "<u4"),
        ("count", "<u4"),
        ("_pad", "<u4", (2,)),
    ]
)
STAR = np.dtype([("dir", "<f4", (3,)), ("mags", "<u4")])
HIP = np.dtype([("row", "<u4"), ("hip", "<u4")])

MMAG_MIN = -32768
MMAG_MAX = 32767


class FormatError(ValueError):
    pass


@dataclass
class Catalog:
    nside: int
    epoch_jyear: float
    mag_limit: float
    sources: int
    cells: np.ndarray  # CELL
    stars: np.ndarray  # STAR
    hip: np.ndarray  # HIP


def to_mmag(mag: np.ndarray) -> tuple[np.ndarray, int]:
    """Rounds magnitudes to i16 millimag, clamping to the i16 range.

    Returns the values and how many were clamped. NaN is a `FormatError`.
    """
    mag = np.asarray(mag, dtype=np.float64)
    if np.isnan(mag).any():
        raise FormatError(f"{int(np.isnan(mag).sum())} magnitudes are NaN")
    mmag = np.rint(mag * 1000.0)
    clamped = int(np.count_nonzero((mmag < MMAG_MIN) | (mmag > MMAG_MAX)))
    return np.clip(mmag, MMAG_MIN, MMAG_MAX).astype(np.int16), clamped


def pack_mags(g_mmag: np.ndarray, bp_rp_mmag: np.ndarray) -> np.ndarray:
    g = np.asarray(g_mmag, dtype=np.int16).view(np.uint16).astype(np.uint32)
    c = np.asarray(bp_rp_mmag, dtype=np.int16).view(np.uint16).astype(np.uint32)
    return g | (c << np.uint32(16))


def unpack_mags(mags: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
    mags = np.asarray(mags, dtype=np.uint32)
    g = (mags & np.uint32(0xFFFF)).astype(np.uint16).view(np.int16)
    c = (mags >> np.uint32(16)).astype(np.uint16).view(np.int16)
    return g, c


def to_bytes(cat: Catalog) -> bytes:
    header = np.zeros((), HEADER)
    header["magic"] = MAGIC
    header["version"] = VERSION
    header["nside"] = cat.nside
    header["n_cells"] = len(cat.cells)
    header["n_stars"] = len(cat.stars)
    header["epoch_jyear"] = cat.epoch_jyear
    header["mag_limit"] = cat.mag_limit
    header["sources"] = cat.sources
    header["cells_off"] = HEADER.itemsize
    header["stars_off"] = HEADER.itemsize + cat.cells.nbytes
    header["hip_off"] = header["stars_off"] + cat.stars.nbytes
    header["n_hip"] = len(cat.hip)
    parts = (header, cat.cells.astype(CELL), cat.stars.astype(STAR), cat.hip.astype(HIP))
    return b"".join(p.tobytes() for p in parts)


def write(path, cat: Catalog) -> bytes:
    data = to_bytes(cat)
    with open(path, "wb") as f:
        f.write(data)
    return data


def read(path) -> Catalog:
    with open(path, "rb") as f:
        return from_bytes(f.read())


def from_bytes(data: bytes) -> Catalog:
    header = _header(data)
    n_cells = int(header["n_cells"])
    n_stars = int(header["n_stars"])
    n_hip = int(header["n_hip"])
    cells = _section(data, header["cells_off"], CELL, n_cells)
    stars = _section(data, header["stars_off"], STAR, n_stars)
    hip = _section(data, header["hip_off"], HIP, n_hip)
    _check_cells(cells, n_stars)
    _check_hip(hip, n_stars)
    return Catalog(
        nside=int(header["nside"]),
        epoch_jyear=float(header["epoch_jyear"]),
        mag_limit=float(header["mag_limit"]),
        sources=int(header["sources"]),
        cells=cells,
        stars=stars,
        hip=hip,
    )


def _header(data: bytes) -> np.ndarray:
    if len(data) < HEADER.itemsize:
        raise FormatError(f"file is {len(data)} B, shorter than the {HEADER.itemsize} B header")
    if bytes(data[:8]) != MAGIC:
        raise FormatError(f"bad magic {bytes(data[:8])!r}")
    header = np.frombuffer(data, HEADER, count=1)[0]
    if header["version"] != VERSION:
        raise FormatError(f"unsupported version {header['version']}")
    if header["nside"] == 0:
        raise FormatError("nside is 0")
    if header["n_cells"] != 12 * int(header["nside"]) ** 2:
        raise FormatError(f"n_cells {header['n_cells']} is not 12 * nside^2")
    _check_layout(header, len(data))
    return header


def _check_layout(header: np.ndarray, size: int) -> None:
    cells_off = HEADER.itemsize
    stars_off = cells_off + CELL.itemsize * int(header["n_cells"])
    hip_off = stars_off + STAR.itemsize * int(header["n_stars"])
    end = hip_off + HIP.itemsize * int(header["n_hip"])
    found = (header["cells_off"], header["stars_off"], header["hip_off"])
    if found != (cells_off, stars_off, hip_off):
        raise FormatError(f"section offsets {found} != {(cells_off, stars_off, hip_off)}")
    if size != end:
        raise FormatError(f"file is {size} B, sections need {end} B")


def _section(data: bytes, offset, dtype: np.dtype, count: int) -> np.ndarray:
    return np.frombuffer(data, dtype, count=count, offset=int(offset)).copy()


def _check_cells(cells: np.ndarray, n_stars: int) -> None:
    offset = cells["offset"].astype(np.int64)
    count = cells["count"].astype(np.int64)
    expected = np.cumsum(count) - count
    if not np.array_equal(offset, expected):
        bad = int(np.flatnonzero(offset != expected)[0])
        raise FormatError(f"cell {bad} offset {offset[bad]} != {expected[bad]}")
    if count.sum() != n_stars:
        raise FormatError(f"cell counts sum to {count.sum()}, not n_stars {n_stars}")


def _check_hip(hip: np.ndarray, n_stars: int) -> None:
    rows = hip["row"].astype(np.int64)
    if len(rows) and (np.any(np.diff(rows) <= 0) or rows[-1] >= n_stars):
        raise FormatError("HIP rows are not strictly increasing star rows")
    if np.any(hip["hip"] == np.iinfo(np.uint32).max):
        raise FormatError("HIP table holds u32::MAX")


def hip_of(cat: Catalog, row: int) -> int | None:
    """The Hipparcos number of star `row`, or None."""
    i = int(np.searchsorted(cat.hip["row"], row))
    if i < len(cat.hip) and cat.hip["row"][i] == row:
        return int(cat.hip["hip"][i])
    return None
