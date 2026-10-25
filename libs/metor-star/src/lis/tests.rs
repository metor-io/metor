use std::sync::OnceLock;
use std::vec::Vec;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, StandardNormal};

use super::*;
use crate::catalog::{BuildConfig, CatStar, Db, synthetic};
use crate::geom::{Quat, V3, dot, normalize};

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

#[test]
fn identifies_noisy_fields() {
    let db = db();
    let mut rng = StdRng::seed_from_u64(1);
    let mut solved = 0;
    for trial in 0..300 {
        let q = random_q(&mut rng);
        let seen = observe(db, q, &mut rng);
        let seen = &seen[..seen.len().min(20)];
        if seen.len() < 4 {
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
        assert!(m.n_match >= 4);
        assert!(m.q.angle_to(q) < 1e-4, "trial {trial}: {}", m.q.angle_to(q));
    }
    assert!(solved > 250);
}

#[test]
fn identifies_with_exactly_four_and_with_false_stars() {
    let db = db();
    let mut rng = StdRng::seed_from_u64(2);
    let mut four = 0;
    for _ in 0..100 {
        let q = random_q(&mut rng);
        let seen = observe(db, q, &mut rng);
        if seen.len() < 6 {
            continue;
        }
        // Exactly four true stars.
        let obs: Vec<V3> = seen[..4].iter().map(|(b, _)| *b).collect();
        if let Step::Found(m) = identify(db, &obs) {
            assert!(m.q.angle_to(q) < 1e-4);
            four += 1;
        }
        // The two brightest are false, then eight true stars.
        let mut obs: Vec<V3> = (0..2).map(|_| random_in_fov(&mut rng, 0.15)).collect();
        obs.extend(seen.iter().take(8).map(|(b, _)| *b));
        let Step::Found(m) = identify(db, &obs) else {
            panic!("false brightest stars stop the search");
        };
        assert!(m.q.angle_to(q) < 1e-4);
        assert!(m.rows[..2].iter().all(|r| *r == NO_ROW));
    }
    // Four stars pass the binomial test only when the field is sparse.
    assert!(four > 0);
}

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
        // A real field seen in a mirror: the handedness check rejects it.
        let q = random_q(&mut rng);
        let mirrored: Vec<V3> = observe(db, q, &mut rng)
            .iter()
            .take(12)
            .map(|(b, _)| [-b[0], b[1], b[2]])
            .collect();
        assert_eq!(identify(db, &mirrored), Step::Exhausted);
    }
}

#[test]
fn a_resumed_search_finds_the_same_match() {
    let db = db();
    let mut rng = StdRng::seed_from_u64(4);
    for _ in 0..30 {
        let q = random_q(&mut rng);
        let mut obs: Vec<V3> = (0..6).map(|_| random_in_fov(&mut rng, 0.15)).collect();
        obs.extend(observe(db, q, &mut rng).iter().take(10).map(|(b, _)| *b));
        let whole = identify(db, &obs);
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
        assert_eq!(whole, resumed);
        if let Step::Found(m) = resumed {
            assert_eq!(m.triplets, calls);
        }
    }
}

#[test]
fn verify_rejects_a_triangle_without_support() {
    let db = db();
    let mut rng = StdRng::seed_from_u64(5);
    let q = random_q(&mut rng);
    let seen = observe(db, q, &mut rng);
    let mut scratch = Vec::new();
    // The right attitude with three true stars and the rest false.
    let mut obs: Vec<V3> = seen[..3].iter().map(|(b, _)| *b).collect();
    obs.extend((0..10).map(|_| random_in_fov(&mut rng, 0.15)));
    let v = verify(db, q, &obs, &LisConfig::default(), &mut scratch);
    assert!(!v.accepted && v.n_match == 3, "{v:?}");
    let all: Vec<V3> = seen.iter().take(12).map(|(b, _)| *b).collect();
    let v = verify(db, q, &all, &LisConfig::default(), &mut scratch);
    assert!(v.accepted, "{v:?}");
}

#[test]
fn binomial_sf_is_a_tail() {
    assert_eq!(binomial_sf(0, 5, 0.3), 1.0);
    assert_eq!(binomial_sf(6, 5, 0.3), 0.0);
    assert!((binomial_sf(1, 3, 0.5) - 0.875).abs() < 1e-12);
    assert!((binomial_sf(3, 3, 0.5) - 0.125).abs() < 1e-12);
    assert!(binomial_sf(4, 16, 1e-6) < 1e-20);
    let _ = dot;
}
