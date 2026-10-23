use std::vec::Vec;

use nalgebra::{Matrix4, SymmetricEigen};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::StandardNormal;

use super::*;
use crate::geom::normalize;

const STAR_SIGMA: f64 = 34e-6;
/// Half-angle (rad) of the 20° field of view.
const HALF_FOV: f64 = 10.0 * core::f64::consts::PI / 180.0;
/// The q-method's own error (rad). An eigensolver's error grows as the
/// inverse of the gap between `K`'s two largest eigenvalues, and reaches
/// about 1e-11 for clustered stars.
const ORACLE_TOL: f64 = 1e-10;

/// QUEST's error from rounding grows as `ε / gap²`, where `gap` is the
/// distance between `K`'s two largest eigenvalues. Clustered stars give a
/// small gap; the error is then about the boresight.
const QUEST_ROUNDING: f64 = 1e-15;

/// Davenport's q-method: the eigenvector of `K` for its largest eigenvalue,
/// and the gap to the next eigenvalue.
fn q_method_with_gap(obs: &[Obs]) -> (Quat, f64) {
    let p = Profile::new(obs);
    let mut k = Matrix4::zeros();
    for i in 0..3 {
        for j in 0..3 {
            k[(i, j)] = p.s[i][j] - if i == j { p.sigma } else { 0.0 };
        }
        k[(i, 3)] = p.z[i];
        k[(3, i)] = p.z[i];
    }
    k[(3, 3)] = p.sigma;
    let eig = SymmetricEigen::new(k);
    let i = eig.eigenvalues.imax();
    let v = eig.eigenvectors.column(i);
    let lambda = eig.eigenvalues[i];
    let next = (0..4)
        .filter(|j| *j != i)
        .map(|j| eig.eigenvalues[j])
        .fold(f64::NEG_INFINITY, f64::max);
    (Quat([v[0], v[1], v[2], v[3]]), lambda - next)
}

fn q_method(obs: &[Obs]) -> Quat {
    q_method_with_gap(obs).0
}

fn gaussian(rng: &mut StdRng) -> f64 {
    rng.sample(StandardNormal)
}

fn random_unit(rng: &mut StdRng) -> V3 {
    loop {
        if let Some(v) = normalize([gaussian(rng), gaussian(rng), gaussian(rng)]) {
            return v;
        }
    }
}

fn random_quat(rng: &mut StdRng) -> Quat {
    loop {
        let q = Quat([gaussian(rng), gaussian(rng), gaussian(rng), gaussian(rng)]);
        if let Some(q) = q.normalized() {
            return q;
        }
    }
}

fn axis_angle(axis: V3, angle: f64) -> Quat {
    Quat::from_parts(scale(axis, (angle / 2.0).sin()), (angle / 2.0).cos())
}

/// A body vector uniform in the field of view about +z.
fn in_fov(rng: &mut StdRng) -> V3 {
    let cos_t = 1.0 - rng.random::<f64>() * (1.0 - HALF_FOV.cos());
    let sin_t = (1.0 - cos_t * cos_t).sqrt();
    let phi = rng.random::<f64>() * 2.0 * core::f64::consts::PI;
    [sin_t * phi.cos(), sin_t * phi.sin(), cos_t]
}

/// `b` turned by an isotropic tangent-plane error of `sigma` per axis.
fn perturb(rng: &mut StdRng, b: V3, sigma: f64) -> V3 {
    let t1 = normalize(cross(b, random_unit(rng))).unwrap();
    let t2 = cross(b, t1);
    let e = add(
        scale(t1, sigma * gaussian(rng)),
        scale(t2, sigma * gaussian(rng)),
    );
    normalize(add(b, e)).unwrap()
}

/// `n` noiseless stars in the field of view for attitude `q`.
fn stars(rng: &mut StdRng, q: Quat, n: usize) -> Vec<Obs> {
    (0..n)
        .map(|_| {
            let b = in_fov(rng);
            Obs {
                b,
                r: q.conj().rotate(b),
                sigma: STAR_SIGMA,
            }
        })
        .collect()
}

/// Two well separated vectors and a third for attitude `q`, so the solution is
/// well determined at any attitude.
fn wide(q: Quat) -> Vec<Obs> {
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.6, 0.8]]
        .into_iter()
        .map(|r| Obs {
            b: q.rotate(r),
            r,
            sigma: 1e-3,
        })
        .collect()
}

fn assert_solves(obs: &[Obs], truth: Quat, tol: f64) -> Solution {
    let sol = quest(obs, None).unwrap();
    let (oracle, gap) = q_method_with_gap(obs);
    let to_truth = sol.q.angle_to(truth);
    let to_oracle = sol.q.angle_to(oracle);
    let rounding = QUEST_ROUNDING / (gap * gap);
    let tol = tol + rounding;
    assert!(
        to_truth < tol,
        "QUEST is {to_truth} rad from truth {truth:?}"
    );
    assert!(
        to_oracle < ORACLE_TOL + rounding,
        "QUEST is {to_oracle} rad from the oracle"
    );
    sol
}

#[test]
fn oracle_matches_truth() {
    let truth = Quat([0.1, -0.5, 0.3, 0.8]).normalized().unwrap();
    assert!(q_method(&wide(truth)).angle_to(truth) < 1e-12);
}

#[test]
fn noiseless_random_attitudes() {
    let mut rng = StdRng::seed_from_u64(1);
    for _ in 0..10_000 {
        let truth = random_quat(&mut rng);
        let n = rng.random_range(3..=20);
        let sol = assert_solves(&stars(&mut rng, truth, n), truth, 1e-12);
        let expected = (truth.w().abs() < MIN_GAMMA).then_some(());
        assert_eq!(sol.rotated.map(|_| ()), expected);
    }
}

#[test]
fn near_half_turn_uses_sequential_rotations() {
    let mut rng = StdRng::seed_from_u64(2);
    let mut axes: Vec<V3> = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]].into();
    axes.extend((0..100).map(|_| random_unit(&mut rng)));
    for (i, axis) in axes.into_iter().enumerate() {
        for angle in [180f64, 180.0 - 1e-6f64.to_degrees(), 179.9] {
            let truth = axis_angle(axis, angle.to_radians());
            let sol = assert_solves(&wide(truth), truth, 1e-12);
            assert!(sol.rotated.is_some(), "axis {axis:?} at {angle}°");
            if i < 3 {
                assert_eq!(sol.rotated, Some(Axis::ALL[i]));
            }
        }
    }
}

#[test]
fn turns_below_the_switch_are_solved_unrotated() {
    let truth = axis_angle([0.0, 0.0, 1.0], 160f64.to_radians());
    let sol = assert_solves(&wide(truth), truth, 1e-12);
    assert_eq!(sol.rotated, None);
}

#[test]
fn hint_is_used_when_it_works() {
    let truth = axis_angle([0.0, 1.0, 0.0], core::f64::consts::PI);
    let sol = quest(&wide(truth), Some(Axis::Y)).unwrap();
    assert_eq!(sol.rotated, Some(Axis::Y));
    assert!(sol.q.angle_to(truth) < 1e-12);
}

#[test]
fn wrong_hint_falls_back() {
    let truth = Quat::IDENTITY;
    let sol = quest(&wide(truth), Some(Axis::X)).unwrap();
    assert_eq!(sol.rotated, None);
    assert!(sol.q.angle_to(truth) < 1e-12);
}

#[test]
fn noisy_loss_matches_oracle_and_nees_is_three() {
    let mut rng = StdRng::seed_from_u64(3);
    let trials = 10_000;
    let mut nees = 0.0;
    for _ in 0..trials {
        let truth = random_quat(&mut rng);
        let n = rng.random_range(3..=20);
        let mut obs = stars(&mut rng, truth, n);
        for o in &mut obs {
            o.b = perturb(&mut rng, o.b, STAR_SIGMA);
        }
        let sol = quest(&obs, None).unwrap();
        let oracle_loss = wahba_loss(&obs, q_method(&obs));
        let rel = (sol.loss - oracle_loss).abs() / oracle_loss;
        // Residuals of 34 µrad carry rounding of about 1e-16, so the loss
        // agrees to about 1e-11 relative.
        assert!(rel < 1e-9, "loss {} vs oracle {oracle_loss}", sol.loss);

        let d = truth.conj() * sol.q;
        let err = scale(d.vec(), 2.0 * d.w().signum());
        nees += dot(err, mat_vec(inv_sym(sol.p_theta).unwrap(), err));
    }
    let mean = nees / trials as f64;
    assert!((mean - 3.0).abs() < 0.1, "mean NEES {mean}");
}

#[test]
fn two_stars_are_enough() {
    let mut rng = StdRng::seed_from_u64(4);
    for _ in 0..100 {
        let truth = random_quat(&mut rng);
        assert_solves(&stars(&mut rng, truth, 2), truth, 1e-12);
    }
}

#[test]
fn weights_spanning_a_million() {
    let mut rng = StdRng::seed_from_u64(5);
    for _ in 0..100 {
        let truth = random_quat(&mut rng);
        let mut obs = stars(&mut rng, truth, 8);
        for (i, o) in obs.iter_mut().enumerate() {
            o.sigma = STAR_SIGMA * 10f64.powf(3.0 * i as f64 / 7.0);
        }
        assert_solves(&obs, truth, 1e-12);
    }
}

#[test]
fn repeated_star_counts_twice() {
    let mut rng = StdRng::seed_from_u64(6);
    let truth = random_quat(&mut rng);
    let mut obs = stars(&mut rng, truth, 3);
    obs.push(obs[0]);
    let sol = assert_solves(&obs, truth, 1e-12);
    let mut doubled = obs[..3].to_vec();
    doubled[0].sigma = STAR_SIGMA / 2f64.sqrt();
    let p = quest(&doubled, None).unwrap().p_theta;
    for (a, b) in sol.p_theta.as_flattened().iter().zip(p.as_flattened()) {
        assert!((a - b).abs() <= 1e-12 * a.abs().max(1e-30), "{a} vs {b}");
    }
}

#[test]
fn too_few_vectors() {
    let obs = wide(Quat::IDENTITY);
    assert_eq!(quest(&[], None), Err(WahbaError::TooFew));
    assert_eq!(quest(&obs[..1], None), Err(WahbaError::TooFew));
}

#[test]
fn parallel_vectors_are_degenerate() {
    let o = wide(Quat::IDENTITY)[0];
    let flipped = Obs {
        b: scale(o.b, -1.0),
        r: scale(o.r, -1.0),
        ..o
    };
    assert_eq!(quest(&[o, o], None), Err(WahbaError::Degenerate));
    assert_eq!(quest(&[o, flipped, o], None), Err(WahbaError::Degenerate));
}

#[test]
fn invalid_input_is_an_error() {
    let obs = wide(Quat::IDENTITY);
    for bad in [
        Obs {
            b: [f64::NAN, 0.0, 1.0],
            ..obs[0]
        },
        Obs {
            r: [0.0, f64::INFINITY, 0.0],
            ..obs[0]
        },
        Obs {
            sigma: f64::NAN,
            ..obs[0]
        },
        Obs {
            sigma: 0.0,
            ..obs[0]
        },
    ] {
        assert_eq!(
            quest(&[bad, obs[1], obs[2]], None),
            Err(WahbaError::Invalid)
        );
    }
}

#[test]
fn lambda_max_is_one_minus_loss() {
    let mut rng = StdRng::seed_from_u64(7);
    let truth = random_quat(&mut rng);
    let mut obs = stars(&mut rng, truth, 10);
    for o in &mut obs {
        o.b = perturb(&mut rng, o.b, STAR_SIGMA);
    }
    let lambda = lambda_max(&Profile::new(&obs)).unwrap();
    let loss = quest(&obs, None).unwrap().loss;
    assert!(
        (1.0 - lambda - loss).abs() < 1e-15,
        "λ {lambda}, loss {loss}"
    );
}

#[test]
fn lambda_max_fails_on_non_finite_profile() {
    let mut p = Profile::new(&wide(Quat::IDENTITY));
    p.sigma = f64::NAN;
    assert_eq!(lambda_max(&p), None);
}

#[test]
fn stars_too_close_to_fix_roll_are_degenerate() {
    let b = [0.0, 0.0, 1.0];
    let close = normalize([1e-7, 0.0, 1.0]).unwrap();
    let truth = axis_angle([0.0, 1.0, 0.0], 0.3);
    let obs = [b, close].map(|b| Obs {
        b,
        r: truth.conj().rotate(b),
        sigma: STAR_SIGMA,
    });
    assert_eq!(quest(&obs, None), Err(WahbaError::Degenerate));
}

#[test]
fn double_root_is_degenerate() {
    let mut rng = StdRng::seed_from_u64(8);
    let obs = stars(&mut rng, Quat::IDENTITY, 2);
    let mut p = Profile::new(&obs);
    p.s = [[0.0; 3]; 3];
    p.z = [0.0; 3];
    p.sigma = 0.0;
    assert_eq!(solve(&p, 0.0, None), None);
}
