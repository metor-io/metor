from __future__ import annotations

import numpy as np
import pytest
import synth
from starcat import fmt


@pytest.fixture(scope="module")
def tiny() -> fmt.Catalog:
    return synth.tiny()[0]


@pytest.fixture(scope="module")
def tiny_bytes(tiny) -> bytes:
    return fmt.to_bytes(tiny)


def test_round_trip(tiny, tiny_bytes, tmp_path):
    path = tmp_path / "tiny.bin"
    fmt.write(path, tiny)
    back = fmt.read(path)
    assert path.read_bytes() == tiny_bytes
    assert (back.nside, back.epoch_jyear, back.mag_limit, back.sources) == (
        tiny.nside,
        tiny.epoch_jyear,
        tiny.mag_limit,
        tiny.sources,
    )
    assert np.array_equal(back.cells, tiny.cells)
    assert np.array_equal(back.stars, tiny.stars)
    assert np.array_equal(back.hip, tiny.hip)
    assert fmt.to_bytes(back) == tiny_bytes


def test_layout_sizes(tiny, tiny_bytes):
    assert fmt.HEADER.itemsize == 80
    assert fmt.CELL.itemsize == 32
    assert fmt.STAR.itemsize == 16
    assert fmt.HIP.itemsize == 8
    expected = 80 + 32 * 3072 + 16 * len(tiny.stars) + 8 * len(tiny.hip)
    assert len(tiny_bytes) == expected
    assert tiny_bytes[:8] == b"STARCAT\0"


def test_committed_tiny_matches_generator(tiny_bytes):
    assert synth.TINY_BIN.read_bytes() == tiny_bytes


def test_build_is_deterministic(tiny_bytes):
    assert fmt.to_bytes(synth.tiny()[0]) == tiny_bytes


@pytest.mark.parametrize(
    ("mag", "mmag"),
    [(0.0, 0), (7.0004, 7000), (7.0006, 7001), (-1.5, -1500), (12.0, 12000), (-0.0004, 0)],
)
def test_to_mmag_rounds(mag, mmag):
    out, clamped = fmt.to_mmag(np.array([mag]))
    assert out.dtype == np.int16
    assert (int(out[0]), clamped) == (mmag, 0)


def test_to_mmag_clamps_overflow():
    out, clamped = fmt.to_mmag(np.array([32.767, 32.768, 40.0, -32.768, -32.769, -40.0]))
    assert out.tolist() == [32767, 32767, 32767, -32768, -32768, -32768]
    assert clamped == 4


@pytest.mark.parametrize("g", [-32768, -1500, -1, 0, 1, 12000, 32767])
@pytest.mark.parametrize("c", [-32768, -400, 0, 800, 32767])
def test_pack_unpack(g, c):
    packed = fmt.pack_mags(np.array([g], np.int16), np.array([c], np.int16))
    assert packed.dtype == np.uint32
    assert int(packed[0]) & 0xFFFF == g & 0xFFFF
    assert int(packed[0]) >> 16 == c & 0xFFFF
    back_g, back_c = fmt.unpack_mags(packed)
    assert (int(back_g[0]), int(back_c[0])) == (g, c)


def test_hip_of(tiny):
    rows = tiny.hip["row"]
    assert fmt.hip_of(tiny, int(rows[0])) == int(tiny.hip["hip"][0])
    without = np.setdiff1d(np.arange(len(tiny.stars)), rows)
    assert fmt.hip_of(tiny, int(without[0])) is None
    assert fmt.hip_of(tiny, len(tiny.stars)) is None


def test_empty_catalog_round_trips():
    cells = np.zeros(12, fmt.CELL)
    cat = fmt.Catalog(1, 2024.0, 8.0, 0, cells, np.zeros(0, fmt.STAR), np.zeros(0, fmt.HIP))
    back = fmt.from_bytes(fmt.to_bytes(cat))
    assert len(back.stars) == 0 and len(back.hip) == 0 and back.nside == 1


def mutate(data: bytes, offset: int, dtype: str, value) -> bytes:
    out = bytearray(data)
    np.frombuffer(out, dtype, count=1, offset=offset)  # bounds check
    out[offset : offset + np.dtype(dtype).itemsize] = np.array(value, dtype).tobytes()
    return bytes(out)


def field_offset(name: str) -> int:
    return fmt.HEADER.fields[name][1]


def cell_offset(cell: int, name: str) -> int:
    return fmt.HEADER.itemsize + cell * fmt.CELL.itemsize + fmt.CELL.fields[name][1]


def first_nonempty(tiny) -> int:
    return int(np.flatnonzero(tiny.cells["count"])[0])


def test_rejects_bad_magic(tiny_bytes):
    with pytest.raises(fmt.FormatError, match="magic"):
        fmt.from_bytes(b"STARCAX\0" + tiny_bytes[8:])


def test_rejects_bad_version(tiny_bytes):
    with pytest.raises(fmt.FormatError, match="version"):
        fmt.from_bytes(mutate(tiny_bytes, field_offset("version"), "<u4", 2))


def test_rejects_cell_count_not_matching_nside(tiny_bytes):
    with pytest.raises(fmt.FormatError, match="nside"):
        fmt.from_bytes(mutate(tiny_bytes, field_offset("nside"), "<u4", 8))


@pytest.mark.parametrize("length", [0, 40, 79, 80, 4000])
def test_rejects_truncated_header_or_sections(tiny_bytes, length):
    with pytest.raises(fmt.FormatError):
        fmt.from_bytes(tiny_bytes[:length])


def test_rejects_truncated_tail(tiny_bytes):
    with pytest.raises(fmt.FormatError, match="sections need"):
        fmt.from_bytes(tiny_bytes[:-1])


def test_rejects_trailing_bytes(tiny_bytes):
    with pytest.raises(fmt.FormatError, match="sections need"):
        fmt.from_bytes(tiny_bytes + b"\0")


def test_rejects_moved_section(tiny_bytes):
    stars_off = int(np.frombuffer(tiny_bytes, "<u8", 1, field_offset("stars_off"))[0])
    with pytest.raises(fmt.FormatError, match="offsets"):
        fmt.from_bytes(mutate(tiny_bytes, field_offset("stars_off"), "<u8", stars_off + 16))


def test_rejects_non_monotonic_offsets(tiny, tiny_bytes):
    cell = first_nonempty(tiny) + 1
    bad = mutate(tiny_bytes, cell_offset(cell, "offset"), "<u4", 0)
    with pytest.raises(fmt.FormatError, match=f"cell {cell} offset"):
        fmt.from_bytes(bad)


def test_rejects_wrong_count_sum(tiny_bytes):
    last = 3071
    count = int(np.frombuffer(tiny_bytes, "<u4", 1, cell_offset(last, "count"))[0])
    bad = mutate(tiny_bytes, cell_offset(last, "count"), "<u4", count + 1)
    with pytest.raises(fmt.FormatError, match="sum"):
        fmt.from_bytes(bad)


def test_rejects_unsorted_hip_rows(tiny, tiny_bytes):
    hip_off = len(tiny_bytes) - fmt.HIP.itemsize * len(tiny.hip)
    second_row = hip_off + fmt.HIP.itemsize
    bad = mutate(tiny_bytes, second_row, "<u4", int(tiny.hip["row"][0]))
    with pytest.raises(fmt.FormatError, match="HIP rows"):
        fmt.from_bytes(bad)


def test_rejects_hip_row_past_stars(tiny, tiny_bytes):
    last_row = len(tiny_bytes) - fmt.HIP.itemsize
    bad = mutate(tiny_bytes, last_row, "<u4", len(tiny.stars))
    with pytest.raises(fmt.FormatError, match="HIP rows"):
        fmt.from_bytes(bad)


def test_to_mmag_rejects_nan():
    with pytest.raises(fmt.FormatError, match="NaN"):
        fmt.to_mmag(np.array([1.0, np.nan]))


def test_rejects_zero_nside():
    header = np.zeros((), fmt.HEADER)
    header["magic"] = fmt.MAGIC
    header["version"] = fmt.VERSION
    header["cells_off"] = header["stars_off"] = header["hip_off"] = fmt.HEADER.itemsize
    with pytest.raises(fmt.FormatError, match="nside is 0"):
        fmt.from_bytes(header.tobytes())
