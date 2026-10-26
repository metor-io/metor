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
/// The q-method's own error (rad), up to about 1e-11 for clustered stars.
const ORACLE_TOL: f64 = 1e-10;
/// QUEST's rounding error grows as `ε / gap²`, `gap` between `K`'s two
/// largest eigenvalues.
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
    let others = (0..4).filter(|j| *j != i).map(|j| eig.eigenvalues[j]);
    let gap = eig.eigenvalues[i] - others.fold(f64::NEG_INFINITY, f64::max);
    (Quat([v[0], v[1], v[2], v[3]]), gap)
}

impl Obs {
    fn new(b: V3, r: V3, sigma: f64) -> Obs {
        Obs { b, r, sigma }
    }
}

fn q_method(obs: &[Obs]) -> Quat {
    q_method_with_gap(obs).0
}

fn gaussian<const N: usize>(rng: &mut StdRng) -> [f64; N] {
    core::array::from_fn(|_| rng.sample(StandardNormal))
}

fn random_unit(rng: &mut StdRng) -> V3 {
    normalize(gaussian(rng)).unwrap()
}

fn random_quat(rng: &mut StdRng) -> Quat {
    Quat(gaussian(rng)).normalized().unwrap()
}

fn axis_angle(axis: V3, angle: f64) -> Quat {
    Quat::from_parts(scale(axis, (angle / 2.0).sin()), (angle / 2.0).cos())
}

/// `b` turned by an isotropic tangent-plane error of `sigma` per axis.
fn perturb(rng: &mut StdRng, b: V3, sigma: f64) -> V3 {
    let t1 = normalize(cross(b, random_unit(rng))).unwrap();
    let [x, y] = gaussian(rng);
    let e = add(scale(t1, sigma * x), scale(cross(b, t1), sigma * y));
    normalize(add(b, e)).unwrap()
}

/// `n` noiseless stars uniform in the field of view about +z at `q`.
fn stars(rng: &mut StdRng, q: Quat, n: usize) -> Vec<Obs> {
    (0..n)
        .map(|_| {
            let cos_t = 1.0 - rng.random::<f64>() * (1.0 - HALF_FOV.cos());
            let sin_t = (1.0 - cos_t * cos_t).sqrt();
            let phi = rng.random::<f64>() * core::f64::consts::TAU;
            let b = [sin_t * phi.cos(), sin_t * phi.sin(), cos_t];
            Obs::new(b, q.conj().rotate(b), STAR_SIGMA)
        })
        .collect()
}

/// Three well separated vectors at `q`, well determined at any attitude.
fn wide(q: Quat) -> Vec<Obs> {
    [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.6, 0.8]]
        .into_iter()
        .map(|r| Obs::new(q.rotate(r), r, 1e-3))
        .collect()
}

fn assert_solves(obs: &[Obs], truth: Quat, tol: f64) -> Solution {
    let sol = quest(obs, None).unwrap();
    let (oracle, gap) = q_method_with_gap(obs);
    let rounding = QUEST_ROUNDING / (gap * gap);
    let (to_truth, to_oracle) = (sol.q.angle_to(truth), sol.q.angle_to(oracle));
    assert!(to_truth < tol + rounding, "{to_truth} rad from {truth:?}");
    assert!(to_oracle < ORACLE_TOL + rounding, "{to_oracle} rad off");
    sol
}

/// 2 to 20 stars at random attitudes with σ spanning up to a million in
/// weight; the sequential rotation is used exactly below `MIN_GAMMA`.
#[test]
fn noiseless_random_attitudes() {
    let mut rng = StdRng::seed_from_u64(1);
    for _ in 0..10_000 {
        let truth = random_quat(&mut rng);
        let n = rng.random_range(2..=20);
        let mut obs = stars(&mut rng, truth, n);
        for o in &mut obs {
            o.sigma = STAR_SIGMA * 10f64.powf(rng.random_range(0.0..3.0));
        }
        let sol = assert_solves(&obs, truth, 1e-12);
        let expected = (truth.w().abs() < MIN_GAMMA).then_some(());
        assert_eq!(sol.rotated.map(|_| ()), expected);
    }
}

/// Near a half turn QUEST rotates about the axis that works; a hint that
/// works is used, a wrong one falls back.
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
    let below = axis_angle([0.0, 0.0, 1.0], 160f64.to_radians());
    assert_eq!(assert_solves(&wide(below), below, 1e-12).rotated, None);
    let truth = axis_angle([0.0, 1.0, 0.0], core::f64::consts::PI);
    assert_eq!(
        quest(&wide(truth), Some(Axis::Y)).unwrap().rotated,
        Some(Axis::Y)
    );
    let sol = quest(&wide(Quat::IDENTITY), Some(Axis::X)).unwrap();
    assert_eq!(sol.rotated, None);
    assert!(sol.q.angle_to(Quat::IDENTITY) < 1e-12);
}

/// With 34 µrad noise the loss matches the q-method's and the covariance is
/// calibrated: mean NEES 3.
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
        assert!(rel < 1e-9, "loss {} vs oracle {oracle_loss}", sol.loss);
        let d = truth.conj() * sol.q;
        let err = scale(d.vec(), 2.0 * d.w().signum());
        nees += dot(err, mat_vec(inv_sym(sol.p_theta).unwrap(), err));
    }
    let mean = nees / trials as f64;
    assert!((mean - 3.0).abs() < 0.1, "mean NEES {mean}");
}

/// A star given twice weighs as one of `σ/√2`, and `λ_max = 1 − loss`.
#[test]
fn repeated_star_counts_twice() {
    let mut rng = StdRng::seed_from_u64(6);
    let truth = random_quat(&mut rng);
    let mut obs = stars(&mut rng, truth, 3);
    let mut noisy = stars(&mut rng, truth, 10);
    noisy
        .iter_mut()
        .for_each(|o| o.b = perturb(&mut rng, o.b, STAR_SIGMA));
    let lambda = lambda_max(&Profile::new(&noisy)).unwrap();
    assert!((1.0 - lambda - quest(&noisy, None).unwrap().loss).abs() < 1e-15);
    obs.push(obs[0]);
    let sol = assert_solves(&obs, truth, 1e-12);
    let mut doubled = obs[..3].to_vec();
    doubled[0].sigma = STAR_SIGMA / 2f64.sqrt();
    let p = quest(&doubled, None).unwrap().p_theta;
    for (a, b) in sol.p_theta.as_flattened().iter().zip(p.as_flattened()) {
        assert!((a - b).abs() <= 1e-12 * a.abs().max(1e-30), "{a} vs {b}");
    }
}

/// Antiparallel or nearly coincident stars leave the roll unobserved.
#[test]
fn degenerate_geometry_is_an_error() {
    let obs = wide(Quat::IDENTITY);
    let o = obs[0];
    let flipped = Obs::new(scale(o.b, -1.0), scale(o.r, -1.0), o.sigma);
    assert_eq!(quest(&[o, flipped, o], None), Err(WahbaError::Degenerate));
    // Two stars too close to fix the roll.
    let close = [[0.0, 0.0, 1.0], normalize([1e-7, 0.0, 1.0]).unwrap()];
    let close = close.map(|b| Obs::new(b, b, STAR_SIGMA));
    assert_eq!(quest(&close, None), Err(WahbaError::Degenerate));
}
