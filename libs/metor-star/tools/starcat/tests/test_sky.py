from __future__ import annotations

import numpy as np
import pytest
from starcat import astrometry, sky

# (source_id, ra, dec) of Gaia DR3 stars at the reference epoch.
GAIA_ROWS = [
    (2053223001491770752, 293.23305443089845, 39.662706936868396),
    (2149251357756648832, 275.4519106778335, 55.12983689067979),
    (1822484194266196224, 302.80394831030446, 19.364946890622193),
    (2162917943694399488, 313.9958906089479, 44.37395346787891),
    (1966852377399382912, 325.4965508902599, 42.569140823447604),
    (2006631986545912576, 339.71370064106645, 56.449527919314264),
    (141903112416398208, 45.281227682213405, 36.187322648253364),
    (142695654141786624, 46.88353502474499, 39.08658066904224),
    (4036505651808325120, 269.42046184333543, -39.1019044061955),
    (4037358426191922688, 269.7320952132936, -36.85831292946056),
    (6535867174178188032, 350.93937709374035, -43.12437183642993),
    (6538840975174712832, 355.40354381270606, -37.433489084241685),
    (4472832130942575872, 269.44850252543836, 4.739420051112412),
    (5853498713190525696, 217.39232147200883, -62.67607511676666),
]


def random_units(n: int, seed: int = 3) -> np.ndarray:
    rng = np.random.default_rng(seed)
    v = rng.normal(size=(n, 3))
    return v / np.linalg.norm(v, axis=1, keepdims=True)


def test_cell_matches_gaia_source_id():
    source_id, ra, dec = (np.array(c) for c in zip(*GAIA_ROWS, strict=True))
    assert np.array_equal(sky.cell(astrometry.unit(ra, dec)), sky.gaia_cell(source_id))


def test_cell_range():
    cells = sky.cell(random_units(50_000))
    assert cells.min() == 0 and cells.max() == sky.n_cells() - 1


@pytest.mark.parametrize(
    ("ra", "dec"), [(0, 90), (123, 90), (0, -90), (0, 0), (360, 0), (359.9999999, 0)]
)
def test_cell_at_poles_and_ra_seam(ra, dec):
    c = sky.cell(astrometry.unit([ra], [dec]))[0]
    assert 0 <= c < sky.n_cells()


def test_ra_seam_is_continuous():
    a = sky.cell(astrometry.unit([0.0], [10.0]))
    b = sky.cell(astrometry.unit([360.0], [10.0]))
    assert a == b


def test_every_point_is_within_its_cell_radius():
    centre, radius = sky.cell_bounds()
    units = np.vstack([random_units(200_000), astrometry.unit([0, 0, 45, 0], [90, -90, 0, 0])])
    c = sky.cell(units)
    angle = np.arccos(np.clip(np.einsum("ij,ij->i", centre[c], units), -1, 1))
    assert np.all(angle <= radius[c])
    assert np.all(angle.max() > radius.min() * 0.8)


def test_cell_bounds_shape():
    centre, radius = sky.cell_bounds()
    assert centre.shape == (3072, 3) and radius.shape == (3072,)
    assert np.allclose(np.linalg.norm(centre, axis=1), 1.0)
    assert np.all((radius > 0.04) & (radius < 0.07))


def test_pairs_within_matches_brute_force():
    a = random_units(300, seed=4)
    b = np.vstack([random_units(2000, seed=5), a[:100] + 1e-6])
    b /= np.linalg.norm(b, axis=1, keepdims=True)
    radius = np.radians(10.0 / 3600.0)
    i, j, _ = sky.pairs_within(a, b, radius)
    sep = np.arccos(np.clip(a @ b.T, -1, 1))
    bi, bj = np.nonzero(sep <= radius)
    assert sorted(zip(i, j, strict=True)) == sorted(zip(bi, bj, strict=True))
    assert len(bi) >= 100


def test_pairs_within_across_cell_edge():
    edge = astrometry.unit([45.0], [0.0])
    a = astrometry.unit([45.0 - 0.5 / 3600], [0.0])
    b = np.vstack([astrometry.unit([45.0 + 0.5 / 3600], [0.0]), edge])
    _, j, sep = sky.pairs_within(a, b, np.radians(2 / 3600))
    assert sorted(j.tolist()) == [0, 1]
    assert np.degrees(sep.max()) * 3600 == pytest.approx(1.0, abs=1e-6)


def test_pairs_within_empty():
    i, j, sep = sky.pairs_within(np.zeros((0, 3)), random_units(5), 1e-5)
    assert len(i) == len(j) == len(sep) == 0
