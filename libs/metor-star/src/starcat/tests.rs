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
    let c = libm::cos(dec);
    [c * libm::cos(ra), c * libm::sin(ra), libm::sin(dec)]
}

#[test]
fn parses_tiny_and_g8() {
    let cat = tiny();
    let h = cat.header();
    assert_eq!(
        (h.nside.get(), cat.cells().len(), cat.all_stars().len()),
        (16, 3072, 200)
    );
    assert_eq!(h.sources.get(), SOURCE_GAIA_DR3 | SOURCE_HIPPARCOS);
    assert_eq!(cat.all_stars()[130].g(), -1.5);
    assert_eq!((cat.hip(124), cat.hip(200)), (Some(1000), None));
    let cat = g8();
    assert_eq!(
        (cat.all_stars().len(), cat.header().n_hip.get()),
        (63_022, 49_359)
    );
    assert!(cat.all_stars().iter().all(|s| s.g() <= 8.0));
    // Sirius and Vega, from J2000 and Hipparcos proper motions to 2024.0.
    let cases = [
        (32349, 25232, radec(101.283354, -16.724271), -1.46),
        (91262, 18437, radec(279.236452, 38.785605), 0.03),
    ];
    for (hip, row, truth, g) in cases {
        assert_eq!(cat.hip(row), Some(hip));
        let star = &cat.all_stars()[row as usize];
        let sep = angle(star.unit(), truth);
        assert!(sep < 0.5 * ARCSEC, "HIP {hip} is {}″ off", sep / ARCSEC);
        assert!((star.g() - g).abs() < 0.05, "HIP {hip} G {}", star.g());
    }
}

/// Every star within the cone lies in a cell `cells_in_cone` returns, which
/// are about as few as the cone's area allows; degenerate cones are empty.
#[test]
fn cones_lose_no_star() {
    let cat = g8();
    let mut cell_of = vec![0; cat.all_stars().len()];
    for (i, cell) in (0u32..).zip(cat.cells()) {
        cell_of[cell.rows()].fill(i);
    }
    let mut rng = StdRng::seed_from_u64(26);
    let mut cones: Vec<(V3, f64)> = (0..300)
        .map(|_| {
            let z: f64 = rng.random_range(-1.0..1.0);
            let phi: f64 = rng.random_range(0.0..core::f64::consts::TAU);
            let r = libm::sqrt(1.0 - z * z);
            let dir = [r * libm::cos(phi), r * libm::sin(phi), z];
            (dir, rng.random_range(5.0..20.0) * DEG)
        })
        .collect();
    cones.extend([([0.0, 0.0, 1.0], 5.0 * DEG), ([0.0, 0.0, -1.0], 0.0)]);
    cones.push((cat.all_stars()[25232].unit(), 0.0));
    for (dir, half) in cones {
        let mut hit = vec![false; cat.cells().len()];
        cat.cells_in_cone(dir, half)
            .for_each(|c| hit[c as usize] = true);
        for (row, star) in cat.all_stars().iter().enumerate() {
            if angle(dir, star.unit()) <= half {
                assert!(hit[cell_of[row] as usize], "row {row} missed at {dir:?}");
            }
        }
    }
    // About 0.95% of the sky; a cell is 1/3072 of it, plus a one-cell rim.
    let n = cat.cells_in_cone([0.3, -0.2, 0.9], 10.0 * DEG).count();
    assert!((23..=60).contains(&n), "{n} cells");
    assert!(cat.cells_in_cone([0.0, 0.0, -2.0], 4.0).eq(0..3072));
    assert_eq!(cat.cells_in_cone([0.0; 3], 1.0).count(), 0);
    assert_eq!(cat.cells_in_cone([0.0, 0.0, 1.0], f64::NAN).count(), 0);
}

#[test]
fn brighter_than_matches_scan() {
    let cat = g8();
    for cell in 0..3072 {
        for g in [-2.0, 3.5, 7.999, 8.0, f32::NAN] {
            let scan = cat.stars(cell).iter().filter(|s| s.g() < g).count();
            assert_eq!(cat.brighter_than(cell, g).len(), scan, "cell {cell} g {g}");
        }
    }
    assert!(cat.stars(3072).is_empty());
    assert!(cat.brighter_than(u32::MAX, 99.0).is_empty());
}

/// Every star lies inside its cell's stored radius, with the 1e-7 rad the
/// cone query relies on to spare.
#[test]
fn stars_lie_inside_their_cells() {
    for cat in [tiny(), g8()] {
        for (i, cell) in (0u32..).zip(cat.cells()) {
            for star in cat.stars(i) {
                let sep = angle(cell.centre(), star.unit());
                assert!(sep <= cell.radius() - 1e-7, "cell {i}: {sep}");
            }
        }
    }
}

/// The parse error of the tiny catalog after `f`, given its bytes and the
/// offset of its first star.
fn mutated(f: impl FnOnce(&mut [u8], usize)) -> CatError {
    let mut bytes = TINY.to_vec();
    let n_cells = Header::ref_from_prefix(&bytes).unwrap().0.n_cells.get() as usize;
    f(&mut bytes, Header::LEN + n_cells * size_of::<Cell>());
    StarCat::parse(&bytes).unwrap_err()
}

fn at<T: FromBytes + IntoBytes + KnownLayout>(b: &mut [u8], offset: usize) -> &mut T {
    T::mut_from_prefix(&mut b[offset..]).unwrap().0
}

fn header(b: &mut [u8]) -> &mut Header {
    at(b, 0)
}

#[test]
fn rejects_malformed_files() {
    assert_eq!(StarCat::parse(&TINY[..79]).err(), Some(CatError::TooShort));
    let len = TINY.len() as u64 - 1;
    let short = StarCat::parse(&TINY[..len as usize]).err();
    assert_eq!(
        short,
        Some(CatError::BadLength {
            len,
            expected: len + 1
        })
    );
    assert_eq!(mutated(|b, _| b[0] = b'X'), CatError::BadMagic);
    assert_eq!(
        mutated(|b, _| header(b).version.set(2)),
        CatError::BadVersion(2)
    );
    assert_eq!(
        mutated(|b, _| header(b).nside.set(8)),
        CatError::BadCellCount
    );
    assert_eq!(
        mutated(|b, _| header(b).stars_off.set(0)),
        CatError::BadOffsets
    );
    let cell = |i: usize| Header::LEN + i * size_of::<Cell>();
    let wide = mutated(|b, _| at::<Cell>(b, cell(7)).radius.set(3.2));
    assert_eq!(wide, CatError::BadCell { cell: 7 });
    let full = tiny()
        .cells()
        .iter()
        .position(|c| c.count.get() >= 2)
        .unwrap();
    let shifted = mutated(|b, _| {
        let c = at::<Cell>(b, cell(full));
        c.offset.set(c.offset.get() + 1);
    });
    assert_eq!(shifted, CatError::CellOffset { cell: full as u32 });
    let star = |first: usize, row: usize| first + row * size_of::<Star>();
    let long = mutated(|b, s| at::<Star>(b, star(s, 17)).dir[0].set(2.0));
    assert_eq!(long, CatError::BadStar { row: 17 });
    let first = tiny().cells()[full].offset.get() as usize;
    let unsorted = mutated(|b, s| at::<Star>(b, star(s, first)).mags.set(0x7fff));
    assert_eq!(unsorted, CatError::Unsorted { cell: full as u32 });
    let repeated = mutated(|b, _| {
        let hip = header(b).hip_off.get() as usize;
        let row = at::<HipEntry>(b, hip).row.get();
        at::<HipEntry>(b, hip + size_of::<HipEntry>()).row.set(row);
    });
    assert_eq!(repeated, CatError::HipRows);
}
