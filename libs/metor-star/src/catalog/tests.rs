use std::sync::OnceLock;
use std::vec::Vec;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::build::{MAX_FACE_N, select, separation};
use super::kvector::{self, Sorted};
use super::*;
use crate::geom::{V3, dot, normalize};
use crate::render::testkit::STARCAT_G8;
use crate::starcat::StarCat;

/// A sorted table and its k-vector, for testing `range` without a DB.
struct Table {
    values: Vec<f32>,
    k: Vec<u32>,
}

impl Sorted for Table {
    fn len(&self) -> usize {
        self.values.len()
    }
    fn value(&self, i: usize) -> f32 {
        self.values[i]
    }
    fn k(&self, i: usize) -> u32 {
        self.k[i]
    }
}

fn range(values: &[f32], lo: f32, hi: f32) -> core::ops::Range<usize> {
    let mut values = values.to_vec();
    values.sort_by(f32::total_cmp);
    let (k, line) = kvector::build(&values);
    kvector::range(&Table { values, k }, &line, lo, hi)
}

/// Rounded values repeat; a query's range holds exactly the values in it.
#[test]
fn kvector_matches_brute_force() {
    let mut rng = StdRng::seed_from_u64(1);
    for n in [1usize, 10, 1000, 100_000] {
        let mut values: Vec<f32> = (0..n)
            .map(|_| (rng.random::<f32>() * 350.0).round() / 1000.0)
            .collect();
        values.sort_by(f32::total_cmp);
        let (k, line) = kvector::build(&values);
        let table = Table { values, k };
        for _ in 0..2000 {
            let lo = rng.random::<f32>() * 0.5 - 0.05;
            let hi = lo + rng.random::<f32>() * 0.05;
            let start = table.values.partition_point(|v| *v < lo);
            let end = table.values.partition_point(|v| *v <= hi).max(start);
            assert_eq!(kvector::range(&table, &line, lo, hi), start..end);
        }
    }
    assert_eq!(range(&[0.2; 50], 0.2, 0.2), 0..50);
    assert!(range(&[0.1, 0.2], 0.5, 0.4).is_empty());
    assert!(range(&[0.1, 0.2], f32::NAN, 0.4).is_empty());
    assert_eq!(range(&[], 0.0, 1.0), 0..0);
}

fn cfg() -> BuildConfig {
    BuildConfig {
        fov_max_rad: 20f64.to_radians() * 2f64.sqrt(),
        face_n: 8,
    }
}

/// A DB of 5000 synthetic stars, built once.
fn db() -> &'static Db {
    static DB: OnceLock<Db> = OnceLock::new();
    DB.get_or_init(|| {
        let stars: Vec<CatStar> = synthetic::uniform(5000, 7)
            .into_iter()
            .zip(1..)
            .map(|(r, hip)| CatStar { r, mag: 5.0, hip })
            .collect();
        Db::build(&stars, &cfg()).expect("builds")
    })
}

#[test]
fn build_keeps_every_star_and_pair() {
    let db = db();
    let mut hips: Vec<u32> = (0..db.n_stars() as u16).filter_map(|r| db.hip(r)).collect();
    hips.sort();
    assert_eq!(hips, (1..=5000).collect::<Vec<u32>>());
    let vectors: Vec<V3> = (0..5000).map(|r| db.star(r).unwrap().vector()).collect();
    let brute = (0..vectors.len())
        .flat_map(|a| (a + 1..vectors.len()).map(move |b| (a, b)))
        .filter(|(a, b)| separation(vectors[*a], vectors[*b]) < cfg().fov_max_rad)
        .count();
    assert_eq!(db.pairs().len(), brute);
    for p in db.pairs() {
        let theta = separation(vectors[p.a as usize], vectors[p.b as usize]);
        assert!((f64::from(p.theta) - theta).abs() < 1e-7);
    }
    assert_eq!(db.star(u16::MAX), None);
    assert!(db.cell(db.n_cells()).is_empty());
    let mut rng = StdRng::seed_from_u64(2);
    for _ in 0..1000 {
        let lo = rng.random::<f32>() * 0.6 - 0.05;
        let hi = lo + rng.random::<f32>() * 0.01;
        let found = db.pair_range(lo, hi);
        let brute = db.pairs().iter().filter(|p| (lo..=hi).contains(&p.theta));
        assert_eq!(brute.count(), found.len());
    }
}

#[test]
fn cone_matches_brute_force() {
    let db = db();
    let mut rng = StdRng::seed_from_u64(3);
    let mut out = Vec::with_capacity(db.n_stars());
    for t in 0..5000 {
        let axis = if t % 50 == 0 {
            [1.0, 0.0, 0.0]
        } else {
            let v: V3 = core::array::from_fn(|_| rng.random::<f64>() - 0.5);
            normalize(v).unwrap_or([0.0, 0.0, 1.0])
        };
        let radius = rng.random::<f64>() * 30f64.to_radians();
        out.clear();
        db.cone(axis, radius, &mut out);
        out.sort();
        let cos_r = libm::cos(radius);
        let brute: Vec<u16> = (0..db.n_stars() as u16)
            .filter(|r| dot(db.star(*r).unwrap().vector(), axis) >= cos_r)
            .collect();
        assert_eq!(out, brute, "axis {axis:?}, radius {radius}");
    }
    out.clear();
    db.cone([0.0, 0.0, 1.0], core::f64::consts::PI, &mut out);
    assert_eq!(out.len(), db.n_stars());
}

/// Every point of every cell lies within `cell_radius` of its centre, which
/// is in the cell; `cell_of` is total.
#[test]
fn cells_tile_the_sphere() {
    let mut rng = StdRng::seed_from_u64(4);
    for n in [1, 3, 8, 16] {
        for _ in 0..20_000 {
            let v: V3 = core::array::from_fn(|_| rng.random::<f64>() - 0.5);
            let r = normalize(v).unwrap_or([1.0, 0.0, 0.0]);
            let c = cells::cell_of(r, n);
            assert!(separation(r, cells::cell_center(c, n)) <= cells::cell_radius(n));
        }
        for c in 0..6 * n * n {
            assert_eq!(cells::cell_of(cells::cell_center(c, n), n), c);
        }
    }
    for r in [[0.0; 3], [f64::NAN, 0.0, 1.0], [f64::INFINITY, 1.0, 0.0]] {
        assert!(cells::cell_of(r, 8) < 384);
    }
}

/// Stars a hair either side of cell edges, where rounding to f32 can move a
/// star across: each row sits in the cell its stored vector falls in.
#[test]
fn rows_sit_in_the_cell_of_their_stored_vector() {
    let mut stars = Vec::new();
    for k in 0..=8 {
        let t = f64::from(k) * 0.25 - 1.0;
        for step in -40..=40 {
            let u = t + f64::from(step) * 1e-9;
            for r in [[1.0, u, 0.3], [0.3, -1.0, u], [u, 0.7, -1.0], [1.0, u, u]] {
                let hip = stars.len() as u32 + 1;
                let r = normalize(r).unwrap();
                stars.push(CatStar { r, mag: 5.0, hip });
            }
        }
    }
    let db = Db::build(&stars, &cfg()).unwrap();
    for row in 0..db.n_stars() {
        let cell = db.cell_of(db.star(row as u16).unwrap().vector());
        assert!(db.cell(cell).contains(&row), "row {row} not in cell {cell}");
    }
    for face_n in [0, MAX_FACE_N + 1] {
        let bad = BuildConfig { face_n, ..cfg() };
        assert_eq!(Db::build(&stars, &bad).err(), Some(BuildError::FaceN));
    }
    let many = std::vec![stars[0]; 65_536];
    assert_eq!(
        Db::build(&many, &cfg()).err(),
        Some(BuildError::TooManyStars)
    );
}

fn onboard() -> &'static Db {
    static DB: OnceLock<Db> = OnceLock::new();
    DB.get_or_init(|| {
        let cat = StarCat::parse(STARCAT_G8).unwrap();
        Db::from_starcat(&cat, &Select::default()).unwrap()
    })
}

fn row_of(db: &Db, hip: u32) -> u16 {
    (0..db.n_stars() as u16)
        .find(|row| db.hip(*row) == Some(hip))
        .unwrap_or_else(|| panic!("HIP {hip} is in the DB"))
}

/// 2000 named stars, 5 or 6 per cell, Sirius in; named pairs come back from
/// a k-vector query on their angle, and one past 20° is not stored.
#[test]
fn the_onboard_db_is_uniform_and_finds_known_pairs() {
    let db = onboard();
    assert_eq!(db.n_stars(), 2000);
    assert!((60_000..70_000).contains(&db.pairs().len()));
    let per_cell: Vec<usize> = (0..db.n_cells()).map(|c| db.cell(c).len()).collect();
    assert_eq!(
        (per_cell.iter().min(), per_cell.iter().max()),
        (Some(&5), Some(&6))
    );
    let known = [
        (54061, 53910, 5.37), // Dubhe, Merak
        (26727, 25930, 2.74), // Alnitak, Mintaka
        (27989, 24436, 18.6), // Betelgeuse, Rigel
    ];
    for (x, y, deg) in known {
        let (a, b) = (row_of(db, x), row_of(db, y));
        let (a, b) = (a.min(b), a.max(b));
        let theta = separation(db.star(a).unwrap().vector(), db.star(b).unwrap().vector());
        assert!((theta.to_degrees() - deg).abs() < 0.05, "{x}-{y}");
        let t = theta as f32;
        let hit = db.pairs()[db.pair_range(t - 1e-6, t + 1e-6)]
            .iter()
            .any(|p| (p.a, p.b) == (a, b));
        assert!(hit, "{x}-{y} not found at θ {t}");
    }
    // Sirius and Betelgeuse are 27° apart.
    let (a, b) = (row_of(db, 32349), row_of(db, 27989));
    assert!(
        !db.pairs()
            .iter()
            .any(|p| (p.a, p.b) == (a.min(b), a.max(b)))
    );
}

/// HIP 2484 and 2487 are 0.4 px apart: one merged star keeps the brighter's
/// number at the flux-weighted position, between the two.
#[test]
fn a_close_pair_merges_into_its_brighter_star() {
    let cat = StarCat::parse(STARCAT_G8).unwrap();
    let all = Select {
        n_stars: usize::MAX,
        ..Select::default()
    };
    let stars = select(&cat, &all);
    let row = |hip| (0..cat.all_stars().len() as u32).find(|r| cat.hip(*r) == Some(hip));
    let star = |hip| &cat.all_stars()[row(hip).unwrap() as usize];
    let (bright, faint) = if star(2484).g() < star(2487).g() {
        (2484, 2487)
    } else {
        (2487, 2484)
    };
    assert!(!stars.iter().any(|s| s.hip == faint));
    let m = stars.iter().find(|s| s.hip == bright).unwrap();
    assert!(m.mag < f64::from(star(bright).g()));
    let (ra, rb) = (star(2484).unit(), star(2487).unit());
    assert!(dot(m.r, ra).min(dot(m.r, rb)) > dot(ra, rb));
    let brightest = stars.iter().map(|s| s.mag).fold(f64::MAX, f64::min);
    assert!(brightest < -1.0, "Sirius is in: {brightest}");
}

/// Building the onboard DB at startup. Run with `cargo test --release -p
/// metor-star onboard_build_time -- --ignored --nocapture`.
#[test]
#[ignore = "timing"]
fn onboard_build_time() {
    let cat = StarCat::parse(STARCAT_G8).unwrap();
    let start = std::time::Instant::now();
    let db = Db::from_starcat(&cat, &Select::default()).unwrap();
    std::println!("{} stars in {:?}", db.n_stars(), start.elapsed());
}

/// Every file in `data/` is named in its `NOTICE` (licensing).
#[test]
fn every_data_file_is_in_the_notice() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/data");
    let notice = std::fs::read_to_string(std::format!("{dir}/NOTICE")).unwrap();
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.starts_with('.') || name == "NOTICE" || name.starts_with("starcat-g12") {
            continue;
        }
        assert!(notice.contains(&name), "data/{name} is not in data/NOTICE");
    }
}
