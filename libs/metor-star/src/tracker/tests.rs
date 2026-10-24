use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, StandardNormal};

use super::*;
use crate::geom::normalize;

fn random_q(rng: &mut StdRng) -> Quat {
    let v: [f64; 4] = core::array::from_fn(|_| StandardNormal.sample(rng));
    Quat(v).normalized().unwrap()
}

/// A star fixed in ECI, seen from a frame turning at `ω`, moves by
/// `−ω × b` (to first order): `predict` turns the frame the same way.
#[test]
fn predict_turns_the_frame_by_the_rate() {
    let mut rng = StdRng::seed_from_u64(1);
    for _ in 0..100 {
        let q = random_q(&mut rng);
        let omega: V3 = core::array::from_fn(|_| rng.random_range(-0.1..0.1));
        let r = normalize(core::array::from_fn(|_| rng.random_range(-1.0..1.0))).unwrap();
        let dt = 1e-3;
        let (b0, b1) = (q.rotate(r), predict(q, omega, dt).rotate(r));
        let expected = crate::geom::add(b0, scale(crate::geom::cross(omega, b0), -dt));
        let err = norm(crate::geom::add(b1, scale(expected, -1.0)));
        assert!(err < 1e-7, "{err}");
        // Many small steps compose to one large one.
        let mut q_steps = q;
        for _ in 0..100 {
            q_steps = predict(q_steps, omega, 0.01);
        }
        assert!(q_steps.angle_to(predict(q, omega, 1.0)) < 1e-12);
    }
    let q = random_q(&mut rng);
    assert_eq!(predict(q, [0.0; 3], 1.0), q);
}

#[test]
fn lambda_max_is_the_largest_eigenvalue() {
    let mut rng = StdRng::seed_from_u64(2);
    for _ in 0..200 {
        // R diag(l) Rᵀ with known eigenvalues.
        let mut l: [f64; 3] = core::array::from_fn(|_| rng.random_range(0.0..5.0));
        let r = random_q(&mut rng).to_m3();
        let m: M3 = core::array::from_fn(|i| {
            core::array::from_fn(|j| (0..3).map(|k| r[k][i] * l[k] * r[k][j]).sum())
        });
        l.sort_by(f64::total_cmp);
        assert!(
            (lambda_max(m) - l[2]).abs() < 1e-9,
            "{} vs {}",
            lambda_max(m),
            l[2]
        );
    }
    assert_eq!(
        lambda_max([[1.0, 0.0, 0.0], [0.0, 3.0, 0.0], [0.0, 0.0, 2.0]]),
        3.0
    );
}
