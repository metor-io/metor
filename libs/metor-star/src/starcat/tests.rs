use std::vec;
use std::vec::Vec;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::*;

static TINY: &[u8] = include_bytes!("../../tests/data/tiny.bin");
static G8: &[u8] = include_bytes!("../../data/starcat-g8.bin");

const DEG: f64 = core::f64::consts::PI / 180.0;
const ARCSEC: f64 = DEG / 3600.0;

fn tiny() -> StarCat<'static> {
    StarCat::parse(TINY).unwrap()
}

fn g8() -> StarCat<'static> {
    StarCat::parse(G8).unwrap()
}

fn radec(ra_deg: f64, dec_deg: f64) -> V3 {
    let (ra, dec) = (ra_deg * DEG, dec_deg * DEG);
    [
        libm::cos(dec) * libm::cos(ra),
        libm::cos(dec) * libm::sin(ra),
        libm::sin(dec),
    ]
}

/// The cell of every star row.
fn cell_of_rows(cat: &StarCat) -> Vec<u32> {
    let mut out = vec![0; cat.all_stars().len()];
    for (i, cell) in (0u32..).zip(cat.cells()) {
        out[cell.rows()].fill(i);
    }
    out
}

/// Every star within `half` of `dir` lies in a cell `cells_in_cone` returns.
fn assert_cone_complete(cat: &StarCat, cell_of: &[u32], dir: V3, half: f64) {
    let mut hit = vec![false; cat.cells().len()];
    for c in cat.cells_in_cone(dir, half) {
        hit[c as usize] = true;
    }
    let dir = normalize(dir).unwrap();
    let near = libm::cos((half + 1e-3).min(core::f64::consts::PI));
    for (row, star) in cat.all_stars().iter().enumerate() {
        let u = star.unit();
        if dot(dir, u) >= near && angle(dir, u) <= half {
            assert!(
                hit[cell_of[row] as usize],
                "row {row} missed at {dir:?}, {half}"
            );
        }
    }
}

fn random_unit(rng: &mut StdRng) -> V3 {
    let z: f64 = rng.random_range(-1.0..1.0);
    let phi: f64 = rng.random_range(0.0..core::f64::consts::TAU);
    let r = libm::sqrt(1.0 - z * z);
    [r * libm::cos(phi), r * libm::sin(phi), z]
}

fn mutated(f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut bytes = TINY.to_vec();
    f(&mut bytes);
    bytes
}

fn header_mut(bytes: &mut [u8]) -> &mut Header {
    Header::mut_from_prefix(bytes).unwrap().0
}

fn cell_mut(bytes: &mut [u8], cell: usize) -> &mut Cell {
    let at = Header::LEN + cell * size_of::<Cell>();
    Cell::mut_from_prefix(&mut bytes[at..]).unwrap().0
}

fn star_mut(bytes: &mut [u8], row: usize) -> &mut Star {
    let n_cells = header_mut(bytes).n_cells.get() as usize;
    let at = Header::LEN + n_cells * size_of::<Cell>() + row * size_of::<Star>();
    Star::mut_from_prefix(&mut bytes[at..]).unwrap().0
}

fn hip_mut(bytes: &mut [u8], i: usize) -> &mut HipEntry {
    let at = header_mut(bytes).hip_off.get() as usize + i * size_of::<HipEntry>();
    HipEntry::mut_from_prefix(&mut bytes[at..]).unwrap().0
}

/// The first cell holding at least `n` stars.
fn cell_with(cat: &StarCat, n: u32) -> usize {
    cat.cells().iter().position(|c| c.count.get() >= n).unwrap()
}

fn parse_err(bytes: &[u8]) -> CatError {
    StarCat::parse(bytes).unwrap_err()
}

#[test]
fn header_is_80_bytes() {
    assert_eq!(Header::LEN, 80);
    assert_eq!(size_of::<Cell>(), 32);
    assert_eq!(size_of::<Star>(), 16);
    assert_eq!(size_of::<HipEntry>(), 8);
}

#[test]
fn parses_tiny() {
    let cat = tiny();
    let h = cat.header();
    assert_eq!(h.nside.get(), 16);
    assert_eq!(cat.cells().len(), 3072);
    assert_eq!(cat.all_stars().len(), 200);
    assert_eq!(h.n_hip.get(), 24);
    assert_eq!(h.epoch_jyear.get(), 2024.0);
    assert_eq!(h.mag_limit.get(), 12.0);
    assert_eq!(h.sources.get(), SOURCE_GAIA_DR3 | SOURCE_HIPPARCOS);
    let total: usize = (0..3072).map(|c| cat.stars(c).len()).sum();
    assert_eq!(total, 200);
}

#[test]
fn tiny_known_rows() {
    let cat = tiny();
    let brightest = &cat.all_stars()[130];
    assert_eq!(brightest.g_mmag(), -1500);
    assert_eq!(brightest.g(), -1.5);
    let min = cat.all_stars().iter().map(Star::g_mmag).min();
    assert_eq!(min, Some(-1500));
    assert_eq!(cat.hip(124), Some(1000));
    assert_eq!(cat.hip(89), Some(1005));
    let poles = cat
        .all_stars()
        .iter()
        .filter(|s| s.unit()[2].abs() > 1.0 - 1e-9);
    assert_eq!(poles.count(), 2);
}

#[test]
fn parses_g8() {
    let cat = g8();
    let h = cat.header();
    assert_eq!(cat.all_stars().len(), 63_022);
    assert_eq!(h.n_hip.get(), 49_359);
    assert_eq!(h.mag_limit.get(), 8.0);
    assert!(cat.all_stars().iter().all(|s| s.g() <= 8.0));
}

/// Sirius and Vega, from their J2000 positions and Hipparcos proper motions
/// propagated linearly to 2024.0.
#[test]
fn g8_known_stars() {
    let cat = g8();
    let cases = [
        (32349, 25232, radec(101.283354, -16.724271), -1.46),
        (91262, 18437, radec(279.236452, 38.785605), 0.03),
    ];
    for (hip, row, truth, v) in cases {
        assert_eq!(cat.hip(row), Some(hip));
        let star = &cat.all_stars()[row as usize];
        let sep = angle(star.unit(), truth);
        assert!(sep < 0.5 * ARCSEC, "HIP {hip} is {}″ off", sep / ARCSEC);
        assert!((star.g() - v).abs() < 0.05, "HIP {hip} G {}", star.g());
    }
    let brightest = cat.all_stars().iter().map(Star::g_mmag).min();
    assert_eq!(brightest, Some(cat.all_stars()[25232].g_mmag()));
}

#[test]
fn hip_absent_rows() {
    let cat = tiny();
    let hip_rows: Vec<u32> = cat.hip.iter().map(|e| e.row.get()).collect();
    let bare = (0..200).find(|r| !hip_rows.contains(r)).unwrap();
    assert_eq!(cat.hip(bare), None);
    assert_eq!(cat.hip(200), None);
    assert_eq!(cat.hip(u32::MAX), None);
}

#[test]
fn cones_lose_no_star() {
    let cat = g8();
    let cell_of = cell_of_rows(&cat);
    let mut rng = StdRng::seed_from_u64(26);
    for _ in 0..500 {
        let half = rng.random_range(5.0..20.0) * DEG;
        assert_cone_complete(&cat, &cell_of, random_unit(&mut rng), half);
    }
}

#[test]
fn cones_are_tight() {
    let cat = g8();
    let n = cat.cells_in_cone([0.3, -0.2, 0.9], 10.0 * DEG).count();
    // About 0.95% of the sky; a cell is 1/3072 of it, plus a one-cell rim.
    assert!((23..=60).contains(&n), "{n} cells");
}

#[test]
fn brighter_than_matches_scan() {
    let cat = g8();
    for cell in 0..3072 {
        for g in [-2.0, 0.014, 3.5, 6.0, 7.999, 8.0, 9.0, f32::NAN] {
            let scan = cat.stars(cell).iter().filter(|s| s.g() < g).count();
            assert_eq!(cat.brighter_than(cell, g).len(), scan, "cell {cell} g {g}");
        }
    }
}

#[test]
fn cone_of_zero_holds_its_star() {
    let cat = g8();
    let cell_of = cell_of_rows(&cat);
    for row in [0, 18437, 25232, 63_021] {
        let dir = cat.all_stars()[row].unit();
        let cells: Vec<u32> = cat.cells_in_cone(dir, 0.0).collect();
        assert!(cells.contains(&cell_of[row]), "row {row}: {cells:?}");
        assert!(cells.len() <= 8);
    }
}

#[test]
fn cone_of_pi_is_every_cell() {
    let cat = tiny();
    assert!(
        cat.cells_in_cone([1.0, 0.0, 0.0], core::f64::consts::PI)
            .eq(0..3072)
    );
    assert!(cat.cells_in_cone([0.0, 0.0, -2.0], 4.0).eq(0..3072));
}

#[test]
fn cones_on_poles() {
    for cat in [tiny(), g8()] {
        let cell_of = cell_of_rows(&cat);
        for z in [1.0, -1.0] {
            assert_cone_complete(&cat, &cell_of, [0.0, 0.0, z], 5.0 * DEG);
            assert_cone_complete(&cat, &cell_of, [0.0, 0.0, z], 0.0);
        }
    }
}

#[test]
fn bad_cones_are_empty() {
    let cat = tiny();
    assert_eq!(cat.cells_in_cone([0.0; 3], 1.0).count(), 0);
    assert_eq!(cat.cells_in_cone([f64::NAN, 0.0, 1.0], 1.0).count(), 0);
    assert_eq!(cat.cells_in_cone([0.0, 0.0, 1.0], -1e-3).count(), 0);
    assert_eq!(cat.cells_in_cone([0.0, 0.0, 1.0], f64::NAN).count(), 0);
}

#[test]
fn empty_and_unknown_cells() {
    let cat = tiny();
    let empty = cat.cells().iter().position(|c| c.count.get() == 0).unwrap() as u32;
    assert!(cat.stars(empty).is_empty());
    assert!(cat.brighter_than(empty, 99.0).is_empty());
    assert!(cat.stars(3072).is_empty());
    assert!(cat.brighter_than(u32::MAX, 99.0).is_empty());
}

#[test]
fn rejects_short_files() {
    assert_eq!(parse_err(&[]), CatError::TooShort);
    assert_eq!(parse_err(&TINY[..79]), CatError::TooShort);
    let len = TINY.len() as u64;
    let short = parse_err(&TINY[..TINY.len() - 1]);
    assert_eq!(
        short,
        CatError::BadLength {
            len: len - 1,
            expected: len
        }
    );
    let long = mutated(|b| b.push(0));
    assert_eq!(
        parse_err(&long),
        CatError::BadLength {
            len: len + 1,
            expected: len
        }
    );
}

/// The HIP section shorter than `n_hip` says.
#[test]
fn rejects_short_hip_section() {
    let bytes = mutated(|b| {
        let h = header_mut(b);
        h.n_hip.set(h.n_hip.get() + 1);
    });
    assert!(matches!(parse_err(&bytes), CatError::BadLength { .. }));
}

#[test]
fn rejects_bad_header() {
    let magic = mutated(|b| b[0] = b'X');
    assert_eq!(parse_err(&magic), CatError::BadMagic);
    let version = mutated(|b| header_mut(b).version.set(2));
    assert_eq!(parse_err(&version), CatError::BadVersion(2));
    let nside = mutated(|b| header_mut(b).nside.set(0));
    assert_eq!(parse_err(&nside), CatError::BadCellCount);
    let n_cells = mutated(|b| header_mut(b).nside.set(8));
    assert_eq!(parse_err(&n_cells), CatError::BadCellCount);
    let huge = mutated(|b| header_mut(b).nside.set(u32::MAX));
    assert_eq!(parse_err(&huge), CatError::BadCellCount);
    let offset = mutated(|b| header_mut(b).stars_off.set(0));
    assert_eq!(parse_err(&offset), CatError::BadOffsets);
}

#[test]
fn rejects_bad_cells() {
    let cat = tiny();
    let wide = mutated(|b| cell_mut(b, 7).radius.set(3.2));
    assert_eq!(parse_err(&wide), CatError::BadCell { cell: 7 });
    let long = mutated(|b| cell_mut(b, 7).centre[1].set(2.0));
    assert_eq!(parse_err(&long), CatError::BadCell { cell: 7 });
    let full = cell_with(&cat, 2);
    let radius = mutated(|b| cell_mut(b, 7).radius.set(f32::NAN));
    assert_eq!(parse_err(&radius), CatError::BadCell { cell: 7 });
    let negative = mutated(|b| cell_mut(b, 7).radius.set(-0.1));
    assert_eq!(parse_err(&negative), CatError::BadCell { cell: 7 });
    let shifted = mutated(|b| {
        let c = cell_mut(b, full);
        c.offset.set(c.offset.get() + 1);
    });
    assert_eq!(
        parse_err(&shifted),
        CatError::CellOffset { cell: full as u32 }
    );
    let sum = mutated(|b| {
        let c = cell_mut(b, 3071);
        c.count.set(c.count.get() + 1);
    });
    assert_eq!(
        parse_err(&sum),
        CatError::CountSum {
            sum: 201,
            n_stars: 200
        }
    );
}

#[test]
fn rejects_bad_stars() {
    let cat = tiny();
    let short = mutated(|b| star_mut(b, 17).dir[0].set(2.0));
    assert_eq!(parse_err(&short), CatError::BadStar { row: 17 });
    let nan = mutated(|b| star_mut(b, 17).dir[2].set(f32::NAN));
    assert_eq!(parse_err(&nan), CatError::BadStar { row: 17 });
    let full = cell_with(&cat, 2);
    let first = cat.cells()[full].offset.get() as usize;
    let unsorted = mutated(|b| star_mut(b, first).mags.set(0x7fff));
    assert_eq!(
        parse_err(&unsorted),
        CatError::Unsorted { cell: full as u32 }
    );
}

#[test]
fn rejects_bad_hip() {
    let repeated = mutated(|b| {
        let row = hip_mut(b, 0).row.get();
        hip_mut(b, 1).row.set(row);
    });
    assert_eq!(parse_err(&repeated), CatError::HipRows);
    let past_end = mutated(|b| hip_mut(b, 23).row.set(200));
    assert_eq!(parse_err(&past_end), CatError::HipRows);
    let sentinel = mutated(|b| hip_mut(b, 5).hip.set(u32::MAX));
    assert_eq!(parse_err(&sentinel), CatError::HipValue);
}

#[test]
fn check_offsets_cases() {
    assert_eq!(
        check_offsets([(0, 2), (2, 0), (2, 3)].into_iter(), 5),
        Ok(())
    );
    assert_eq!(check_offsets(core::iter::empty(), 0), Ok(()));
    let gap = check_offsets([(0, 2), (3, 0)].into_iter(), 3);
    assert_eq!(gap, Err(CatError::CellOffset { cell: 1 }));
    let sum = check_offsets([(0, u32::MAX), (u32::MAX, 1)].into_iter(), u32::MAX);
    assert_eq!(
        sum,
        Err(CatError::CountSum {
            sum: 1 << 32,
            n_stars: u32::MAX
        })
    );
}

/// Every star lies inside its cell's stored radius, with the 1e-7 rad the
/// cone query relies on to spare.
#[test]
fn stars_lie_inside_their_cells() {
    for cat in [tiny(), g8()] {
        for (i, cell) in cat.cells().iter().enumerate() {
            for star in cat.stars(i as u32) {
                let sep = angle(cell.centre(), star.unit());
                assert!(
                    sep <= cell.radius() - 1e-7,
                    "cell {i}: {sep} > {}",
                    cell.radius()
                );
            }
        }
    }
}
