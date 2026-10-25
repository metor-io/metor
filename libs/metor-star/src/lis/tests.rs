use std::sync::OnceLock;
use std::vec::Vec;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, StandardNormal};

use super::*;
use crate::catalog::{BuildConfig, CatStar, Db, synthetic};
use crate::geom::{Quat, V3, normalize};

/// 5000 uniform stars, pairs under 20°.
fn db() -> &'static Db {
    static DB: OnceLock<Db> = OnceLock::new();
    DB.get_or_init(|| {
        let stars: Vec<CatStar> = synthetic::uniform(5000, 7)
            .into_iter()
            .zip(1..)
            .map(|(r, hip)| CatStar { r, mag: 5.0, hip })
            .collect();
        let cfg = BuildConfig {
            fov_max_rad: 20f64.to_radians(),
            face_n: 8,
        };
        Db::build(&stars, &cfg).unwrap()
    })
}

fn random_q(rng: &mut StdRng) -> Quat {
    let v: [f64; 4] = core::array::from_fn(|_| StandardNormal.sample(rng));
    Quat(v).normalized().unwrap()
}

/// A random unit vector in the square field within `radius` of `+z`.
fn random_in_fov(rng: &mut StdRng, radius: f64) -> V3 {
    let (x, y) = (rng.random_range(-1.0..1.0), rng.random_range(-1.0..1.0));
    let t = libm::tan(radius) / 2f64.sqrt();
    normalize([x * t, y * t, 1.0]).unwrap()
}

/// The catalog stars in a 20° square field at `q` (star-tracker frame),
/// with their rows, in a random "brightness" order.
fn observe(db: &Db, q: Quat, rng: &mut StdRng) -> Vec<(V3, u16)> {
    let half = 10f64.to_radians();
    let mut seen: Vec<(V3, u16)> = (0..db.n_stars() as u16)
        .filter_map(|row| {
            let b = q.rotate(db.star(row)?.unit());
            let inside = b[2] > 0.0
                && (b[0] / b[2]).abs() < libm::tan(half)
                && (b[1] / b[2]).abs() < libm::tan(half);
            inside.then_some((b, row))
        })
        .collect();
    for i in (1..seen.len()).rev() {
        seen.swap(i, rng.random_range(0..=i));
    }
    seen
}

fn noisy(b: V3, sigma: f64, rng: &mut StdRng) -> V3 {
    let n: [f64; 3] = core::array::from_fn(|_| {
        let x: f64 = StandardNormal.sample(rng);
        sigma * x
    });
    normalize([b[0] + n[0], b[1] + n[1], b[2] + n[2]]).unwrap()
}

fn identify(db: &Db, obs: &[V3]) -> Step {
    let mut p = Pyramid::new(db.n_stars(), LisConfig::default());
    p.start(obs);
    p.step(db, u32::MAX)
}

/// Noisy fields of up to 20 stars, four stars alone, and fields whose two
/// brightest are false all solve with every match right.
#[test]
fn identifies_noisy_and_polluted_fields() {
    let db = db();
    let mut rng = StdRng::seed_from_u64(1);
    let (mut solved, mut four) = (0, 0);
    for trial in 0..300 {
        let q = random_q(&mut rng);
        let seen = observe(db, q, &mut rng);
        let seen = &seen[..seen.len().min(20)];
        if seen.len() < 6 {
            continue;
        }
        let obs: Vec<V3> = seen
            .iter()
            .map(|(b, _)| noisy(*b, 5e-6, &mut rng))
            .collect();
        let Step::Found(m) = identify(db, &obs) else {
            panic!("trial {trial}: {} stars, not solved", seen.len());
        };
        solved += 1;
        for (k, (_, row)) in seen.iter().enumerate() {
            assert!(
                m.rows[k] == *row || m.rows[k] == NO_ROW,
                "trial {trial}: obs {k}"
            );
        }
        assert!(m.n_match >= 4 && m.q.angle_to(q) < 1e-4, "trial {trial}");
        // Four stars pass the binomial test only when the field is sparse.
        let obs: Vec<V3> = seen[..4].iter().map(|(b, _)| *b).collect();
        if let Step::Found(m) = identify(db, &obs) {
            assert!(m.q.angle_to(q) < 1e-4);
            four += 1;
        }
        let mut obs: Vec<V3> = (0..2).map(|_| random_in_fov(&mut rng, 0.15)).collect();
        obs.extend(seen.iter().take(8).map(|(b, _)| *b));
        let Step::Found(m) = identify(db, &obs) else {
            panic!("trial {trial}: false brightest stars stop the search");
        };
        assert!(m.q.angle_to(q) < 1e-4 && m.rows[..2] == [NO_ROW; 2]);
    }
    assert!(solved > 250 && four > 0, "{solved} {four}");
}

/// Random stars run out the triplets, and a real field in a mirror fails
/// the handedness check.
#[test]
fn false_and_mirrored_fields_are_not_identified() {
    let db = db();
    let mut rng = StdRng::seed_from_u64(3);
    for _ in 0..50 {
        let obs: Vec<V3> = (0..16).map(|_| random_in_fov(&mut rng, 0.15)).collect();
        let mut p = Pyramid::new(db.n_stars(), LisConfig::default());
        p.start(&obs);
        assert_eq!(p.step(db, u32::MAX), Step::Exhausted);
        assert!(p.triplets() <= LisConfig::default().max_triplets);
        let seen = observe(db, random_q(&mut rng), &mut rng);
        let mirrored: Vec<V3> = seen
            .iter()
            .take(12)
            .map(|(b, _)| [-b[0], b[1], b[2]])
            .collect();
        assert_eq!(identify(db, &mirrored), Step::Exhausted);
    }
}

/// A search run one triplet per call finds what one call does.
#[test]
fn a_resumed_search_finds_the_same_match() {
    let db = db();
    let mut rng = StdRng::seed_from_u64(4);
    for _ in 0..30 {
        let q = random_q(&mut rng);
        let mut obs: Vec<V3> = (0..6).map(|_| random_in_fov(&mut rng, 0.15)).collect();
        obs.extend(observe(db, q, &mut rng).iter().take(10).map(|(b, _)| *b));
        let mut p = Pyramid::new(db.n_stars(), LisConfig::default());
        p.start(&obs);
        let mut calls = 0;
        let resumed = loop {
            calls += 1;
            match p.step(db, 1) {
                Step::More => continue,
                other => break other,
            }
        };
        assert_eq!(identify(db, &obs), resumed);
        if let Step::Found(m) = resumed {
            assert_eq!(m.triplets, calls);
        }
    }
}

/// The right attitude with three true stars and the rest false lacks the
/// support the binomial test asks for; with all true stars it has it.
#[test]
fn verify_needs_support() {
    let db = db();
    let mut rng = StdRng::seed_from_u64(5);
    let q = random_q(&mut rng);
    let seen = observe(db, q, &mut rng);
    let mut scratch = Vec::new();
    let mut obs: Vec<V3> = seen[..3].iter().map(|(b, _)| *b).collect();
    obs.extend((0..10).map(|_| random_in_fov(&mut rng, 0.15)));
    let v = verify(db, q, &obs, &LisConfig::default(), &mut scratch);
    assert!(!v.accepted && v.n_match == 3, "{v:?}");
    let all: Vec<V3> = seen.iter().take(12).map(|(b, _)| *b).collect();
    assert!(verify(db, q, &all, &LisConfig::default(), &mut scratch).accepted);
    assert_eq!((binomial_sf(0, 5, 0.3), binomial_sf(6, 5, 0.3)), (1.0, 0.0));
    assert!((binomial_sf(1, 3, 0.5) - 0.875).abs() < 1e-12);
    assert!(binomial_sf(4, 16, 1e-6) < 1e-20);
}
