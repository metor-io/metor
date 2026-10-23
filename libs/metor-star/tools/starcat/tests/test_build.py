from __future__ import annotations

import numpy as np
import pytest
import synth
from starcat import build, fetch, fmt, sky


@pytest.fixture(scope="module")
def tiny():
    return synth.tiny()


def hip_by_row(cat: fmt.Catalog) -> np.ndarray:
    out = np.full(len(cat.stars), build.NO_HIP, np.int64)
    out[cat.hip["row"]] = cat.hip["hip"]
    return out


def g_mmag(cat: fmt.Catalog) -> np.ndarray:
    return fmt.unpack_mags(cat.stars["mags"])[0].astype(np.int64)


def test_counts(tiny):
    cat, log = tiny
    assert len(cat.stars) == 200
    assert log.counts["gaia"] == synth.N_GAIA
    assert log.counts["hipparcos_fill"] == synth.N_FILL
    assert log.counts["hip_crossmatched"] == len(synth.XMATCHED)
    assert log.counts["hip_crossmatch_dg_rejected"] == 2
    assert log.counts["hip_partner_below_cut"] == 1
    assert log.counts["hip_covered"] == len(synth.COVERED) + 1
    assert log.counts["hip_covered_no_free_component"] == 0
    assert log.counts["hip_no_position"] == 1
    assert log.counts["bp_rp_defaulted"] == 8
    assert cat.sources == fmt.SOURCE_GAIA_DR3 | fmt.SOURCE_HIPPARCOS
    assert cat.epoch_jyear == build.EPOCH_JYEAR


def test_cell_offsets_are_contiguous(tiny):
    cat = tiny[0]
    assert len(cat.cells) == sky.n_cells()
    assert cat.cells["count"].sum() == len(cat.stars)
    assert np.array_equal(
        cat.cells["offset"][1:], cat.cells["offset"][:-1] + cat.cells["count"][:-1]
    )
    assert np.count_nonzero(cat.cells["count"] == 0) > 0


def test_each_star_is_in_its_cell(tiny):
    cat = tiny[0]
    cell_of_row = np.repeat(np.arange(len(cat.cells)), cat.cells["count"])
    assert np.array_equal(sky.cell(cat.stars["dir"].astype(np.float64)), cell_of_row)


def test_each_star_is_within_its_cell_radius(tiny):
    cat = tiny[0]
    cell_of_row = np.repeat(np.arange(len(cat.cells)), cat.cells["count"])
    centre = cat.cells["centre"][cell_of_row].astype(np.float64)
    star = cat.stars["dir"].astype(np.float64)
    cos = np.einsum("ij,ij->i", centre, star) / np.linalg.norm(star, axis=1)
    angle = np.arccos(np.clip(cos / np.linalg.norm(centre, axis=1), -1, 1))
    assert np.all(angle <= cat.cells["radius"][cell_of_row])


def test_stars_sorted_by_g_within_cell(tiny):
    cat = tiny[0]
    g = g_mmag(cat)
    for offset, count in zip(cat.cells["offset"], cat.cells["count"], strict=True):
        assert np.all(np.diff(g[offset : offset + count]) >= 0)


def test_ties_break_by_source_id_then_hip():
    gaia, hip, _ = synth.inputs()
    for col in gaia:
        gaia[col] = gaia[col][:1].repeat(3)
    gaia["source_id"] = gaia["source_id"][0] + np.array([5, 1, 3])
    hip = {k: v[:3].copy() for k, v in hip.items()}
    hip["hip"] = np.array([7, 8, 9])
    hip["ra"][:] = np.nan
    hip["vmag"][:] = gaia["phot_g_mean_mag"][0] + 0.01597
    xmatch = {
        "source_id": gaia["source_id"],
        "original_ext_source_id": np.array([7, 8, 9]),
        "phot_g_mean_mag": gaia["phot_g_mean_mag"],
    }
    cat, _ = build.build(gaia, hip, xmatch, 12.0)
    assert hip_by_row(cat).tolist() == [8, 9, 7]


def rows_of_hip(cat: fmt.Catalog, hip: int) -> np.ndarray:
    return cat.hip["row"][cat.hip["hip"] == hip]


def test_hip_table_holds_every_usable_bright_hip_once(tiny):
    cat, log = tiny
    _, hip, _ = synth.inputs()
    bright = np.isfinite(hip["ra"]) & (hip["hip"] != synth.HIP_BELOW_CUT)
    have = cat.hip["hip"].tolist()
    assert len(have) == len(set(have)) == log.counts["hip"]
    assert sorted(have) == sorted(hip["hip"][bright].tolist())
    assert np.all(np.diff(cat.hip["row"].astype(np.int64)) > 0)


def test_hip_with_partner_below_cut_is_filled(tiny):
    cat = tiny[0]
    _, hip, _ = synth.inputs()
    (row,) = rows_of_hip(cat, synth.HIP_PARTNER_CUT)
    v = hip["vmag"][hip["hip"] == synth.HIP_PARTNER_CUT][0]
    assert g_mmag(cat)[row] == round((v - 0.01597) * 1000)


def test_crossmatch_with_wrong_magnitude_is_filled_at_hipparcos_g(tiny):
    cat = tiny[0]
    gaia, _, _ = synth.inputs()
    (row,) = rows_of_hip(cat, synth.HIP_DG_REJECTED)
    hip_g = gaia["phot_g_mean_mag"][synth.DG_REJECTED] - 5.0 - 0.01597
    assert g_mmag(cat)[row] == round(hip_g * 1000)
    assert np.count_nonzero(g_mmag(cat) == 9000) == 1


def test_double_gives_hip_to_one_component_and_is_not_filled(tiny):
    cat = tiny[0]
    (row,) = rows_of_hip(cat, synth.HIP_DOUBLE)
    assert g_mmag(cat)[row] == 5000
    assert np.count_nonzero(g_mmag(cat) == 5000) == 2


def test_crossmatched_star_carries_gaia_magnitude(tiny):
    cat = tiny[0]
    gaia, _, _ = synth.inputs()
    (row,) = rows_of_hip(cat, synth.HIP_BASE)
    gaia_g = gaia["phot_g_mean_mag"][synth.XMATCHED[0]]
    assert g_mmag(cat)[row] == round(gaia_g * 1000)


def stars(g: list[float], hip: list[int] | None = None, dec_arcsec: list[float] | None = None):
    n = len(g)
    zeros = np.zeros(n)
    return build.Stars(
        source_id=np.arange(1, n + 1, dtype=np.int64),
        hip=np.array(hip if hip else [build.NO_HIP] * n, np.uint32),
        ra=zeros + 10.0,
        dec=np.array(dec_arcsec or [0.0] * n) / 3600.0,
        pmra=zeros,
        pmdec=zeros,
        parallax=zeros,
        rv=zeros,
        epoch=zeros + 2016.0,
        g=np.array(g, float),
        bp_rp=zeros,
    )


def xm(source: list[int], hip: list[int], g: list[float]) -> dict[str, np.ndarray]:
    return {
        "source_id": np.array(source, np.int64),
        "original_ext_source_id": np.array(hip, np.int64),
        "phot_g_mean_mag": np.array(g, float),
    }


def test_crossmatch_gates_on_magnitude_and_presence():
    gaia = stars([5.0, 6.0, 7.0])
    hip = stars([5.2, 9.0, 7.0, 3.0], hip=[30, 31, 32, 33])
    pairs = xm([1, 2, 99, 98], [30, 31, 32, 33], [5.0, 6.0, 8.0, 20.0])
    g_row, h_row, below_cut, rejected = build.crossmatch(gaia, hip, pairs)
    assert g_row.tolist() == [0] and h_row.tolist() == [0]
    assert below_cut.tolist() == [2]
    assert rejected == 2


def test_crossmatch_one_pair_per_source_and_per_hip():
    gaia = stars([5.0, 5.0])
    hip = stars([5.0, 5.0], hip=[30, 20])
    g_row, h_row, _, _ = build.crossmatch(gaia, hip, xm([1, 1, 2], [30, 20, 20], [5.0] * 3))
    assert sorted(zip(g_row.tolist(), hip.hip[h_row].tolist(), strict=True)) == [(0, 20)]


def test_crossmatch_ignores_unknown_hip():
    g_row, _, below_cut, rejected = build.crossmatch(
        stars([5.0]), stars([5.0], hip=[1]), xm([1], [2], [5.0])
    )
    assert len(g_row) == len(below_cut) == rejected == 0


def test_crossmatch_empty():
    g_row, h_row, below_cut, rejected = build.crossmatch(
        stars([5.0]), stars([5.0], hip=[1]), xm([], [], [])
    )
    assert len(g_row) == len(h_row) == len(below_cut) == rejected == 0


def test_cover_sums_flux_of_components():
    gaia = stars([5.0, 5.0, 12.0], dec_arcsec=[-1.0, 1.0, 0.5])
    components = build.cover(stars([4.25, 2.0, 5.0], dec_arcsec=[0, 0, 10.0]), gaia)
    assert components[0].tolist() == [0, 1, 2]
    assert len(components[1]) == 0  # 2 mag brighter than the pair
    assert len(components[2]) == 0  # nothing within the radius


def test_cover_ignores_stars_past_radius():
    gaia = stars([5.0], dec_arcsec=[build.COVER_RADIUS_ARCSEC + 0.5])
    assert len(build.cover(stars([5.0]), gaia)[0]) == 0


def test_give_hip_brightest_free_component_lowest_hip_first():
    gaia_hip = np.array([build.NO_HIP, 77, build.NO_HIP], np.uint32)
    components = [np.array([1, 0]), np.array([0]), np.array([1]), np.array([], int)]
    lost = build.give_hip(gaia_hip, components, np.array([12, 11, 13, 14]))
    assert gaia_hip.tolist() == [11, 77, build.NO_HIP]
    assert lost == 2


def test_fill_respects_mag_limit():
    gaia, hip, xmatch = synth.inputs()
    _, log = build.build(gaia, hip, xmatch, 5.0)
    fill_v = np.concatenate([hip["vmag"][16:18], hip["vmag"][18 : 18 + synth.N_PLAIN_FILL]])
    expected = np.count_nonzero(fill_v - 0.01597 <= 5.0)
    assert 0 < expected < synth.N_FILL
    assert log.counts["hipparcos_fill"] == expected


def test_empty_input_builds_an_empty_catalog():
    gaia, hip, xmatch = synth.inputs()
    empty = [{k: v[:0] for k, v in d.items()} for d in (gaia, hip, xmatch)]
    cat, _ = build.build(*empty, 8.0)
    assert len(cat.stars) == 0 and len(cat.hip) == 0
    assert cat.sources == fmt.SOURCE_GAIA_DR3
    assert np.all(cat.cells["count"] == 0)
    back = fmt.from_bytes(fmt.to_bytes(cat))
    assert len(back.cells) == sky.n_cells()


def test_g_overflow_is_clamped_and_counted():
    gaia, hip, xmatch = synth.inputs()
    gaia["phot_g_mean_mag"][0] = 40.0
    cat, log = build.build(gaia, hip, xmatch, 50.0)
    assert log.counts["g_clamped"] == 1
    assert g_mmag(cat).max() == 32767


def test_read_csv_parses_nulls(tmp_path):
    path = tmp_path / "gaia.csv"
    header = ",".join(fetch.GAIA_COLUMNS)
    path.write_text(f"{header}\n123,1.5,2.5,,,,,2016.0,7.25,\n456,3,4,1,2,3,4,2016.0,8,0.5\n")
    rows = fetch.read_csv(path, fetch.GAIA_COLUMNS)
    assert rows["source_id"].tolist() == [123, 456]
    assert np.isnan(rows["pmra"][0]) and np.isnan(rows["bp_rp"][0])
    assert rows["bp_rp"][1] == 0.5


def test_read_csv_header_only_is_empty(tmp_path):
    path = tmp_path / "gaia.csv"
    path.write_text(",".join(fetch.GAIA_COLUMNS) + "\n")
    rows = fetch.read_csv(path, fetch.GAIA_COLUMNS)
    assert all(len(v) == 0 for v in rows.values())


def test_read_csv_rejects_missing_column(tmp_path):
    path = tmp_path / "gaia.csv"
    path.write_text("source_id,ra\n1,2\n")
    with pytest.raises(fetch.InputError, match="missing columns"):
        fetch.read_csv(path, fetch.GAIA_COLUMNS)


@pytest.mark.parametrize("text", ["", "<VOTABLE>\n<oops", "a,b\n1,2,3,4\n"])
def test_read_csv_rejects_malformed(tmp_path, text):
    path = tmp_path / "bad.csv"
    path.write_text(text)
    with pytest.raises(fetch.InputError):
        fetch.read_csv(path, ("a", "b"))


def test_read_csv_rejects_null_id(tmp_path):
    path = tmp_path / "x.csv"
    path.write_text("source_id,original_ext_source_id,phot_g_mean_mag\n1,,5\n2,3,6\n")
    with pytest.raises(fetch.InputError, match="empty cells"):
        fetch.read_csv(path, fetch.XMATCH_COLUMNS)


def test_gaia_query_partitions_by_level0_cell():
    query = fetch.gaia_query(8, 11)
    q = query.select()
    assert query.count() == (
        "SELECT COUNT(*) AS n FROM gaiadr3.gaia_source WHERE "
        "GAIA_HEALPIX_INDEX(0, source_id) = 11 AND phot_g_mean_mag <= 8.0"
    )
    assert "GAIA_HEALPIX_INDEX(0, source_id) = 11" in q
    assert "phot_g_mean_mag <= 8.0" in q
    assert all(c in q for c in fetch.GAIA_COLUMNS)


def fake_download(text: str):
    def download(adql: str, out) -> None:
        out.write_text(text)

    return download


XM_CSV = "source_id,original_ext_source_id,phot_g_mean_mag\n1,10,5\n2,20,6\n3,30,7\n"


def test_fetch_caches_a_complete_download(tmp_path):
    path = tmp_path / "xm.csv"
    rows = fetch.fetch(fetch.xmatch_query(), path, fake_download(XM_CSV), lambda q: 3)
    assert rows["original_ext_source_id"].tolist() == [10, 20, 30]
    assert path.read_text() == XM_CSV
    assert not list(tmp_path.glob("*.partial"))


@pytest.mark.parametrize(
    "text", [XM_CSV.rsplit("3,30", 1)[0], XM_CSV[:-6], "", "source_id\n1\n2\n3\n"]
)
def test_fetch_rejects_truncated_download_and_caches_nothing(tmp_path, text):
    path = tmp_path / "xm.csv"
    with pytest.raises(fetch.InputError):
        fetch.fetch(fetch.xmatch_query(), path, fake_download(text), lambda q: 3)
    assert list(tmp_path.iterdir()) == []


def test_fetch_deletes_partial_when_download_fails(tmp_path):
    def broken(adql: str, out) -> None:
        out.write_text("source_id,orig")
        raise ConnectionError("dropped")

    with pytest.raises(ConnectionError):
        fetch.fetch(fetch.xmatch_query(), tmp_path / "xm.csv", broken, lambda q: 3)
    assert list(tmp_path.iterdir()) == []


def test_fetch_reads_cache_without_download(tmp_path):
    path = tmp_path / "xm.csv"
    path.write_text(XM_CSV)

    def forbidden(*_):
        raise AssertionError("downloaded")

    rows = fetch.fetch(fetch.xmatch_query(), path, forbidden, forbidden)
    assert len(rows["source_id"]) == 3


def test_cover_takes_a_coincident_star_despite_magnitude():
    gaia = stars([7.0, 4.0, 9.0], dec_arcsec=[0.1, 2.0, 0.2])
    components = build.cover(stars([3.5, 2.0], hip=[1, 2]), gaia)
    assert components[0].tolist()[0] == 0  # 3.5 mag apart, 0.1 arcsec
    assert components[1].tolist() == []  # 7 mag apart is not the same object


def test_cover_coincident_rule_stops_at_radius():
    gaia = stars([7.0], dec_arcsec=[build.SAME_RADIUS_ARCSEC + 0.1])
    assert build.cover(stars([3.5]), gaia)[0].tolist() == []
