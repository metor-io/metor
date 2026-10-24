use std::sync::OnceLock;
use std::vec::Vec;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use zerocopy::little_endian::U32;
use zerocopy::{FromBytes, IntoBytes};

use super::build::{MAX_FACE_N, build, separation};
use super::format::{CELLS, HEADER_LEN, Header, KVECTOR, MAX_STARS, PAIRS, STARS, crc_of, within};
use super::kvector::{self, Line, Sorted};
use super::*;
use crate::geom::{V3, normalize};

/// A sorted table and its k-vector, for testing `range` without a DB.
struct Table {
    values: Vec<f32>,
    k: Vec<u32>,
    line: Line,
}

impl Table {
    fn new(mut values: Vec<f32>) -> Table {
        values.sort_by(f32::total_cmp);
        let (k, line) = kvector::build(&values);
        Table { values, k, line }
    }

    fn brute(&self, lo: f32, hi: f32) -> core::ops::Range<usize> {
        let start = self
            .values
            .iter()
            .position(|v| *v >= lo)
            .unwrap_or(self.values.len());
        let end = self
            .values
            .iter()
            .rposition(|v| *v <= hi)
            .map_or(start, |i| i + 1);
        start..end.max(start)
    }
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

#[test]
fn kvector_matches_brute_force() {
    let mut rng = StdRng::seed_from_u64(1);
    for n in [10usize, 100, 1000, 100_000] {
        // Rounded to 1e-3 so that many values repeat.
        let values = (0..n)
            .map(|_| (rng.random::<f32>() * 0.35 * 1000.0).round() / 1000.0)
            .collect();
        let table = Table::new(values);
        for _ in 0..10_000 {
            let a = rng.random::<f32>() * 0.5 - 0.05;
            let b = a + rng.random::<f32>() * 0.05;
            let range = kvector::range(&table, &table.line, a, b);
            assert_eq!(range, table.brute(a, b), "n {n}, [{a}, {b}]");
        }
    }
}

#[test]
fn kvector_on_one_value_and_repeats() {
    let one = Table::new(std::vec![0.1]);
    assert_eq!(kvector::range(&one, &one.line, 0.1, 0.1), 0..1);
    assert_eq!(kvector::range(&one, &one.line, 0.0, 0.05), 0..0);
    assert_eq!(kvector::range(&one, &one.line, 0.2, 0.3), 1..1);
    let same = Table::new(std::vec![0.2; 50]);
    assert_eq!(kvector::range(&same, &same.line, 0.2, 0.2), 0..50);
    assert_eq!(kvector::range(&same, &same.line, -1.0, 1.0), 0..50);
}

#[test]
fn kvector_degenerate_queries_are_empty_or_whole() {
    let table = Table::new((0..100).map(|i| i as f32 * 0.01).collect());
    let n = table.values.len();
    let range = |lo, hi| kvector::range(&table, &table.line, lo, hi);
    assert!(range(0.5, 0.4).is_empty());
    assert!(range(f32::NAN, 0.4).is_empty());
    assert!(range(0.1, f32::NAN).is_empty());
    assert_eq!(range(f32::NEG_INFINITY, f32::INFINITY), 0..n);
    assert_eq!(range(-5.0, -1.0), 0..0);
    assert_eq!(range(5.0, 6.0), n..n);
    let empty = Table::new(Vec::new());
    assert_eq!(kvector::range(&empty, &empty.line, 0.0, 1.0), 0..0);
}

fn cfg() -> BuildConfig {
    BuildConfig {
        fov_max_rad: 20f64.to_radians() * 2f64.sqrt(),
        mag_limit: 6.5,
        epoch_jy: 2024.0,
        face_n: 8,
    }
}

fn catalog() -> Vec<CatStar> {
    synthetic::uniform(5000, 7)
        .into_iter()
        .zip(1..)
        .map(|(r, hip)| CatStar {
            r,
            mag: 2.0 + (hip % 450) as f64 / 100.0,
            hip,
        })
        .collect()
}

/// A DB of 5000 synthetic stars, built once.
fn db_bytes() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    BYTES.get_or_init(|| build(&catalog(), &cfg()).expect("builds"))
}

#[test]
fn round_trip_keeps_every_star_and_pair() {
    let db = Db::parse(db_bytes()).expect("parses");
    let stars = catalog();
    assert_eq!(db.n_stars(), stars.len());
    let mut hips: Vec<u32> = (0..db.n_stars() as u16).filter_map(|r| db.hip(r)).collect();
    hips.sort();
    assert_eq!(hips, (1..=5000).collect::<Vec<u32>>());
    for row in 0..db.n_stars() as u16 {
        let star = db.star(row).unwrap();
        assert_eq!(
            db.cell_of(star.vector()) as usize,
            (0..db.header.n_cells.get())
                .find(|c| db.cell(*c).contains(&(row as usize)))
                .unwrap() as usize
        );
    }
    let vectors: Vec<V3> = (0..db.n_stars() as u16)
        .map(|r| db.star(r).unwrap().vector())
        .collect();
    let max = cfg().fov_max_rad;
    let brute = (0..vectors.len())
        .flat_map(|a| (a + 1..vectors.len()).map(move |b| (a, b)))
        .filter(|(a, b)| separation(vectors[*a], vectors[*b]) < max)
        .count();
    assert_eq!(db.pairs().len(), brute);
    for p in db.pairs() {
        let theta = separation(vectors[p.a.get() as usize], vectors[p.b.get() as usize]);
        assert!((f64::from(p.theta.get()) - theta).abs() < 1e-7);
    }
    assert_eq!(db.hip(5000), None);
    assert_eq!(db.star(u16::MAX), None);
}

#[test]
fn pair_range_matches_a_scan() {
    let db = Db::parse(db_bytes()).unwrap();
    let mut rng = StdRng::seed_from_u64(2);
    for _ in 0..1000 {
        let lo = rng.random::<f32>() * 0.6 - 0.05;
        let hi = lo + rng.random::<f32>() * 0.01;
        let range = db.pair_range(lo, hi);
        let brute = db
            .pairs()
            .iter()
            .enumerate()
            .filter(|(_, p)| (lo..=hi).contains(&p.theta.get()))
            .map(|(i, _)| i);
        assert!(brute.clone().all(|i| range.contains(&i)));
        assert_eq!(brute.count(), range.len());
    }
}

fn brute_cone(db: &Db, axis: V3, radius: f64) -> Vec<u16> {
    let cos_r = libm::cos(radius);
    (0..db.n_stars() as u16)
        .filter(|r| within(db.star(*r).unwrap(), axis, cos_r))
        .collect()
}

#[test]
fn cone_matches_brute_force() {
    let db = Db::parse(db_bytes()).unwrap();
    let mut rng = StdRng::seed_from_u64(3);
    let s2 = core::f64::consts::FRAC_1_SQRT_2;
    let s3 = 1.0 / 3f64.sqrt();
    let special: [V3; 7] = [
        [1.0, 0.0, 0.0],
        [0.0, 0.0, -1.0],
        [s2, s2, 0.0],
        [0.0, -s2, s2],
        [s3, s3, s3],
        [-s3, s3, -s3],
        [s2, 0.0, -s2],
    ];
    let mut out = Vec::with_capacity(db.n_stars());
    for t in 0..10_000 {
        let axis = match special.get(t % 50) {
            Some(a) => *a,
            None => normalize([
                rng.random::<f64>() - 0.5,
                rng.random::<f64>() - 0.5,
                rng.random::<f64>() - 0.5,
            ])
            .unwrap_or([0.0, 0.0, 1.0]),
        };
        let radius = rng.random::<f64>() * 30f64.to_radians();
        out.clear();
        db.cone(axis, radius, &mut out);
        out.sort();
        assert_eq!(
            out,
            brute_cone(&db, axis, radius),
            "axis {axis:?}, radius {radius}"
        );
    }
}

#[test]
fn cone_covers_the_sky_and_nothing() {
    let db = Db::parse(db_bytes()).unwrap();
    let mut out = Vec::new();
    db.cone([0.0, 0.0, 1.0], core::f64::consts::PI, &mut out);
    assert_eq!(out.len(), db.n_stars());
    out.clear();
    db.cone([0.0, 0.0, 1.0], 0.0, &mut out);
    assert!(out.len() <= 1);
}

/// Every point of every cell lies within `cell_radius` of its centre.
#[test]
fn cell_radius_bounds_every_cell() {
    for n in [1, 2, 3, 8, 16] {
        let radius = cells::cell_radius(n);
        let mut rng = StdRng::seed_from_u64(u64::from(n));
        for _ in 0..20_000 {
            let r = normalize([
                rng.random::<f64>() - 0.5,
                rng.random::<f64>() - 0.5,
                rng.random::<f64>() - 0.5,
            ])
            .unwrap_or([1.0, 0.0, 0.0]);
            let c = cells::cell_of(r, n);
            assert!(c < 6 * n * n);
            assert!(
                separation(r, cells::cell_center(c, n)) <= radius,
                "n {n}, {r:?}"
            );
        }
    }
}

#[test]
fn cell_center_is_in_its_cell() {
    for n in [1, 4, 8] {
        for c in 0..6 * n * n {
            assert_eq!(cells::cell_of(cells::cell_center(c, n), n), c);
        }
    }
}

#[test]
fn cell_of_is_total() {
    for r in [
        [0.0; 3],
        [f64::NAN, 0.0, 1.0],
        [f64::INFINITY, 1.0, 0.0],
        [1e-300, 0.0, 0.0],
    ] {
        assert!(cells::cell_of(r, 8) < 384);
    }
}

/// A copy of the DB with `edit` applied to its header and the CRC fixed.
fn edited(edit: impl FnOnce(&mut Header, &mut [u8])) -> Vec<u8> {
    let mut bytes = db_bytes().to_vec();
    let (mut header, _) = Header::read_from_prefix(&bytes).unwrap();
    edit(&mut header, &mut bytes[HEADER_LEN..]);
    bytes[..HEADER_LEN].copy_from_slice(header.as_bytes());
    let crc = crc_of(&bytes).unwrap();
    header.crc32 = U32::new(crc);
    bytes[..HEADER_LEN].copy_from_slice(header.as_bytes());
    bytes
}

#[test]
fn a_flipped_byte_fails_the_crc() {
    let mut bytes = db_bytes().to_vec();
    let i = bytes.len() / 2;
    bytes[i] ^= 0x10;
    assert!(matches!(Db::parse(&bytes), Err(DbError::Crc { .. })));
}

#[test]
fn malformed_dbs_give_their_own_errors() {
    let bytes = db_bytes();
    assert_eq!(Db::parse(&bytes[..10]).err(), Some(DbError::Truncated));
    assert_eq!(
        Db::parse(&bytes[..bytes.len() - 4]).err(),
        Some(DbError::Truncated)
    );

    let mut magic = bytes.to_vec();
    magic[0] = b'X';
    assert_eq!(Db::parse(&magic).err(), Some(DbError::BadMagic));

    let version = edited(|h, _| h.version = U32::new(2));
    assert_eq!(Db::parse(&version).err(), Some(DbError::Version(2)));

    let misaligned =
        edited(|h, _| h.sections[STARS].offset = U32::new(h.sections[STARS].offset.get() + 4));
    assert_eq!(Db::parse(&misaligned).err(), Some(DbError::Misaligned));

    let count = edited(|h, _| h.n_stars = U32::new(h.n_stars.get() - 1));
    assert_eq!(Db::parse(&count).err(), Some(DbError::CountMismatch));

    let too_many = edited(|h, _| h.n_stars = U32::new(MAX_STARS + 1));
    assert_eq!(Db::parse(&too_many).err(), Some(DbError::TooManyStars));

    let cells = edited(|h, body| {
        let at = h.sections[CELLS].offset.get() as usize - HEADER_LEN + 4 * 10;
        body[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    });
    assert_eq!(Db::parse(&cells).err(), Some(DbError::Cells));

    let kvec = edited(|h, body| {
        let at = h.sections[KVECTOR].offset.get() as usize - HEADER_LEN + 4 * 3;
        body[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    });
    assert_eq!(Db::parse(&kvec).err(), Some(DbError::KVector));
}

#[test]
fn too_many_stars_do_not_build() {
    let star = CatStar {
        r: [0.0, 0.0, 1.0],
        mag: 5.0,
        hip: 1,
    };
    let stars = std::vec![star; MAX_STARS as usize + 1];
    assert_eq!(build(&stars, &cfg()).err(), Some(BuildError::TooManyStars));
}

#[test]
fn an_empty_catalog_builds_and_parses() {
    let bytes = build(&[], &cfg()).unwrap();
    let db = Db::parse(&bytes).unwrap();
    assert_eq!(db.n_stars(), 0);
    assert!(db.pairs().is_empty());
    assert!(db.pair_range(0.0, 1.0).is_empty());
    let mut out = Vec::<u16>::new();
    db.cone([1.0, 0.0, 0.0], 1.0, &mut out);
    assert!(out.is_empty());
}

/// Sizes and query times for the synthetic 5000-star DB. Run with
/// `cargo test --release -p metor-star db_costs -- --ignored --nocapture`.
#[test]
#[ignore]
fn db_costs() {
    let db = Db::parse(db_bytes()).unwrap();
    std::println!(
        "{} stars, {} pairs, {} cells: {} B",
        db.n_stars(),
        db.pairs().len(),
        db.header.n_cells.get(),
        db_bytes().len()
    );
    let mut rng = StdRng::seed_from_u64(4);
    let axes: Vec<V3> = (0..10_000)
        .map(|_| {
            normalize([
                rng.random::<f64>() - 0.5,
                rng.random::<f64>() - 0.5,
                rng.random::<f64>() - 0.5,
            ])
            .unwrap()
        })
        .collect();
    let mut out = Vec::with_capacity(db.n_stars());
    let start = std::time::Instant::now();
    let mut hits = 0;
    for axis in &axes {
        out.clear();
        db.cone(*axis, 14f64.to_radians(), &mut out);
        hits += out.len();
    }
    let cone = start.elapsed() / axes.len() as u32;
    let start = std::time::Instant::now();
    let mut found = 0;
    for i in 0..100_000 {
        let lo = (i % 1000) as f32 * 4e-4;
        found += db.pair_range(lo, lo + 1e-4).len();
    }
    let range = start.elapsed() / 100_000;
    std::println!(
        "cone (14°): {cone:?}/query, {} stars on average; pair range (±1e-4 rad): {range:?}/query, {} pairs on average",
        hits / axes.len(),
        found / 100_000
    );
}

/// Stars a hair either side of cell edges, where rounding to f32 can move a
/// star across the edge: each row must sit in the cell its stored vector
/// falls in, or a cone query could miss it.
#[test]
fn rows_sit_in_the_cell_of_their_stored_vector() {
    let mut stars = Vec::new();
    for k in 0..=8 {
        let t = f64::from(k) * 0.25 - 1.0;
        for step in -40..=40 {
            let u = t + f64::from(step) * 1e-9;
            for r in [[1.0, u, 0.3], [0.3, -1.0, u], [u, 0.7, -1.0], [1.0, u, u]] {
                let hip = stars.len() as u32 + 1;
                stars.push(CatStar {
                    r: normalize(r).unwrap(),
                    mag: 5.0,
                    hip,
                });
            }
        }
    }
    let bytes = build(&stars, &cfg()).unwrap();
    let db = Db::parse(&bytes).unwrap();
    for row in 0..db.n_stars() {
        let star = db.star(row as u16).unwrap();
        let cell = db.cell_of(star.vector());
        assert!(db.cell(cell).contains(&row), "row {row} not in cell {cell}");
    }
}

#[test]
fn a_bad_face_n_does_not_build() {
    for face_n in [0, MAX_FACE_N + 1, u32::MAX] {
        let cfg = BuildConfig { face_n, ..cfg() };
        assert_eq!(build(&catalog(), &cfg).err(), Some(BuildError::FaceN));
    }
}

#[test]
fn a_bad_line_or_theta_is_rejected() {
    for km in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        let bad = edited(|h, _| h.km = zerocopy::little_endian::F64::new(km));
        assert_eq!(Db::parse(&bad).err(), Some(DbError::KVector), "km {km}");
    }
    let bad = edited(|h, _| h.kq = zerocopy::little_endian::F64::new(f64::NAN));
    assert_eq!(Db::parse(&bad).err(), Some(DbError::KVector));
    let nan = edited(|h, body| {
        let at = h.sections[PAIRS].offset.get() as usize - HEADER_LEN;
        body[at..at + 4].copy_from_slice(&f32::NAN.to_le_bytes());
    });
    assert_eq!(Db::parse(&nan).err(), Some(DbError::Pairs));
}

#[test]
fn cells_past_the_end_are_empty() {
    let db = Db::parse(db_bytes()).unwrap();
    let n_cells = db.header.n_cells.get();
    for c in [n_cells, n_cells + 1, u32::MAX] {
        assert!(db.cell(c).is_empty());
    }
}
