"""Queries the Gaia archive. Run with `pytest -m network`.

Raw results go to `$STARCAT_RAW`, or the default cache.
"""

from __future__ import annotations

import os
from pathlib import Path

import numpy as np
import pytest
from starcat import build, fetch, fmt, photometry
from starcat.ensure import load

pytestmark = pytest.mark.network


@pytest.fixture(scope="module")
def cache() -> Path:
    return Path(os.environ.get("STARCAT_RAW") or fetch.cache_dir())


def test_g6_count(cache):
    gaia = fetch.gaia(6.0, cache)
    assert len(gaia["source_id"]) == 6764
    _, log = build.build(gaia, fetch.hipparcos(cache), fetch.xmatch(cache), 6.0)
    assert log.counts["gaia"] == 6764
    assert 0 < log.counts["hipparcos_fill"] < 400


def test_g8_holds_each_bright_hip_once(cache):
    """Every Hipparcos star with a position and V <= 6.5 is in the G <= 8 file once."""
    hip = fetch.hipparcos(cache)
    cat, _ = build.build(fetch.gaia(8.0, cache), hip, fetch.xmatch(cache), 8.0)
    bright = (hip["vmag"] <= 6.5) & np.isfinite(hip["ra"])
    have, count = np.unique(cat.hip["hip"], return_counts=True)
    assert np.all(count == 1)
    missing = np.setdiff1d(hip["hip"][bright], have)
    assert missing.tolist() == []


def test_hip_to_gaia_rms(cache):
    """A MAD-based RMS: doubles and variables put about 1.5% of pairs over 0.5 mag off."""
    gaia = fetch.gaia(9.0, cache)
    hip = fetch.hipparcos(cache)
    xm = fetch.xmatch(cache)
    g_by_source = dict(zip(gaia["source_id"].tolist(), gaia["phot_g_mean_mag"], strict=True))
    hip_row = {h: i for i, h in enumerate(hip["hip"].tolist())}
    pairs = [
        (g_by_source[s], hip_row[h])
        for s, h in zip(
            xm["source_id"].tolist(), xm["original_ext_source_id"].tolist(), strict=True
        )
        if s in g_by_source and h in hip_row
    ]
    g = np.array([p[0] for p in pairs])
    rows = np.array([p[1] for p in pairs])
    est, _ = photometry.hip_to_gaia(hip["vmag"][rows], hip["v_i"][rows])
    band = (g > 6) & (g < 9) & np.isfinite(est)
    d = est[band] - g[band]
    robust_rms = 1.4826 * np.median(np.abs(d - np.median(d)))
    assert band.sum() > 10_000
    assert robust_rms < 0.05
    assert abs(np.median(d)) < 0.02


def test_g8_is_reproducible(cache):
    cat, _ = build.build(fetch.gaia(8.0, cache), fetch.hipparcos(cache), fetch.xmatch(cache), 8.0)
    data = fmt.to_bytes(cat)
    entry = load()["g8"]
    assert build.sha256(data) == entry["sha256"]
    assert len(data) == entry["bytes"]
