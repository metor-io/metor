use approx::assert_relative_eq;
use core::f64::consts::FRAC_PI_4;
use nox::{array, tensor};

use super::*;

type M3 = Matrix3<f64, ArrayRepr>;
type M3x6 = Matrix3x6<f64, ArrayRepr>;

/// A deterministic splitmix64 stream of uniform samples in [-1, 1).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }

    fn unit(&mut self) -> V3 {
        loop {
            let v: V3 = tensor![self.next(), self.next(), self.next()];
            let n: f64 = v.norm().into_buf();
            if n > 0.1 && n <= 1.0 {
                return v * (1.0 / n);
            }
        }
    }

    fn quat(&mut self) -> Quat {
        loop {
            let q: Vector<f64, 4, ArrayRepr> =
                tensor![self.next(), self.next(), self.next(), self.next()];
            let n: f64 = q.norm().into_buf();
            if n > 0.1 && n <= 1.0 {
                return Quaternion(q * (1.0 / n));
            }
        }
    }
}

fn filter() -> State {
    State::new(
        tensor![0.01, 0.01, 0.01],
        tensor![0.01, 0.01, 0.01],
        1.0 / 120.0,
    )
}

fn obs(q: Quat, reference: V3, sigma: f64) -> VectorObs {
    VectorObs {
        body: q.inverse() * reference,
        reference,
        sigma,
    }
}

fn max_abs(m: M6) -> f64 {
    m.into_buf()
        .as_flattened()
        .iter()
        .fold(0.0, |a, x| a.max(x.abs()))
}

fn theta_variance(p: &M6, axis: V3) -> f64 {
    let p_tt: M3 = p.fixed_slice(&[0, 0]);
    axis.dot(&p_tt.dot(&axis)).into_buf()
}

/// The update as it was before the split into `propagate` and
/// `update_vectors`: `(I − KH)P` with no gate.
fn estimate_attitude_pre_split<const N: usize>(
    s: State,
    measured_bodys: [V3; N],
    references: [V3; N],
    sigma_r: [f64; N],
) -> State {
    let omega = s.omega - s.b_hat;
    let q_hat = propagate_quaternion(s.q_hat, omega, s.dt);
    let mut p = propagate_state_covariance(s.p, omega, s.yqy, s.dt);
    let mut delta_x_hat = V6::zeros();
    for ((reference, measured_body), sigma) in
        references.into_iter().zip(measured_bodys).zip(sigma_r)
    {
        let var_r = M3::eye() * sigma.powi(2);
        let body_r = q_hat.inverse() * reference;
        let e = measured_body - body_r;
        let h: M3x6 = Matrix::concat_in_dim([body_r.skew(), M3::zeros()], 1);
        let h_trans = h.transpose();
        let s = (h.dot(&p).dot(&h_trans) + var_r)
            .try_inverse()
            .unwrap_or_else(|_| Matrix::eye());
        let k = p.dot(&h_trans.dot(&s));
        p = (M6::eye() - k.dot(&h)).dot(&p);
        let d: V3 = h.dot(&delta_x_hat);
        delta_x_hat = delta_x_hat + k.dot(&(e - d));
    }
    let delta_alpha: V3 = delta_x_hat.fixed_slice(&[0]);
    let delta_beta: V3 = delta_x_hat.fixed_slice(&[3]);
    State {
        q_hat: q_hat.integrate_body(delta_alpha),
        b_hat: s.b_hat + delta_beta,
        p,
        omega,
        ..s
    }
}

/// The `test_mekf` sequence: 180 still steps, then 120 steps at 1 rad/s about x.
/// Calls `check` with the truth and the filter after each step.
fn run_test_mekf_sequence(
    mut step: impl FnMut(State, [V3; 2], [V3; 2]) -> State,
    mut check: impl FnMut(&Quat, &State),
) {
    let ref_a = tensor![0.0, 1.0, 0.0];
    let ref_b = tensor![1.0, 0.0, 0.0];
    let mut q: Quat = Quaternion::from_axis_angle(Vector::z_axis(), FRAC_PI_4);
    let mut state = filter();
    for i in 0..300 {
        if i >= 180 {
            q = q.integrate_body(tensor![1.0 / 120.0, 0.0, 0.0]);
            state.omega = tensor![1.0, 0.0, 0.0];
        } else {
            state.omega = Default::default();
        }
        let body = [ref_a, ref_b].map(|r| (q.inverse() * r).normalize());
        state = step(state, body, [ref_a, ref_b]);
        check(&q, &state);
    }
}

#[test]
fn test_calculate_covariance() {
    #[rustfmt::skip]
    let expected_q = tensor![
        [8.33352623e-07, 0.00000000e+00, 0.00000000e+00, 3.47222222e-09, 0.00000000e+00, 0.00000000e+00,],
        [0.00000000e+00, 8.33352623e-07, 0.00000000e+00, 0.00000000e+00, 3.47222222e-09, 0.00000000e+00,],
        [0.00000000e+00, 0.00000000e+00, 8.33352623e-07, 0.00000000e+00, 0.00000000e+00, 3.47222222e-09,],
        [3.47222222e-09, 0.00000000e+00, 0.00000000e+00, 8.33333333e-07, 0.00000000e+00, 0.00000000e+00,],
        [0.00000000e+00, 3.47222222e-09, 0.00000000e+00, 0.00000000e+00, 8.33333333e-07, 0.00000000e+00,],
        [0.00000000e+00, 0.00000000e+00, 3.47222222e-09, 0.00000000e+00, 0.00000000e+00, 8.33333333e-07,],
    ];
    let q = calculate_covariance(
        tensor![0.01, 0.01, 0.01],
        tensor![0.01, 0.01, 0.01],
        1.0 / 120.0,
    );
    assert_relative_eq!(q, expected_q, epsilon = 1e-5);
}

#[test]
fn test_propagate_state_covariance() {
    // from: cube-sat.py
    // TODO(sphw): bring this function into rust
    let yqy = tensor![
        [8.33352623e-07, 0.0, 0.0, -3.4722e-09, 0.0, 0.0],
        [0.0, 8.33352623e-07, 0.0, 0.0, -3.4722e-09, 0.0],
        [0.0, 0.0, 8.33352623e-07, 0.0, 0.0, -3.4722e-09],
        [-3.4722e-09, 0.0, 0.0, 8.3333e-07, 0.0, 0.0],
        [0.0, -3.4722e-09, 0.0, 0.0, 8.3333e-07, 0.0],
        [0.0, 0.0, -3.4722e-09, 0.0, 0.0, 8.3333e-07]
    ];
    let out = propagate_state_covariance(Matrix::eye(), tensor![1.0, 0.0, 0.0], yqy, 1.0 / 120.0);
    #[rustfmt::skip]
    let expected = tensor![[ 1.00007028e+00,  0.00000000e+00,  0.00000000e+00,
        -8.33333681e-03,  0.00000000e+00,  0.00000000e+00],
    [ 0.00000000e+00,  1.00007028e+00,  5.55768107e-19,
        0.00000000e+00, -8.33324036e-03, -3.47220213e-05],
    [ 0.00000000e+00,  5.55768107e-19,  1.00007028e+00,
        0.00000000e+00,  3.47220213e-05, -8.33324036e-03],
    [-8.33333681e-03,  0.00000000e+00,  0.00000000e+00,
        1.00000083e+00,  0.00000000e+00,  0.00000000e+00],
    [ 0.00000000e+00, -8.33324036e-03,  3.47220213e-05,
        0.00000000e+00,  1.00000083e+00,  0.00000000e+00],
    [ 0.00000000e+00, -3.47220213e-05, -8.33324036e-03,
        0.00000000e+00,  0.00000000e+00,  1.00000083e+00]];
    assert_relative_eq!(out.inner(), expected.inner(), epsilon = 1e-6);
}

#[test]
fn test_propagate_quaternion() {
    let q = propagate_quaternion(Quaternion::identity(), tensor![1.0, 0.0, 0.0], 1.0 / 60.0);
    assert_relative_eq!(
        q.0.inner(),
        &array![0.00833324, 0., 0., 0.99996528],
        epsilon = 1e-5
    );
}

#[test]
fn test_mekf() {
    #[rustfmt::skip]
    let expected_yqy = tensor![
        [8.33352623e-07, 0.0, 0.0, -3.4722e-09, 0.0, 0.0],
        [0.0, 8.33352623e-07, 0.0, 0.0, -3.4722e-09, 0.0],
        [0.0, 0.0, 8.33352623e-07, 0.0, 0.0, -3.4722e-09],
        [-3.4722e-09, 0.0, 0.0, 8.3333e-07, 0.0, 0.0],
        [0.0, -3.4722e-09, 0.0, 0.0, 8.3333e-07, 0.0],
        [0.0, 0.0, -3.4722e-09, 0.0, 0.0, 8.3333e-07]
    ];
    assert_relative_eq!(filter().yqy, expected_yqy, epsilon = 1e-4);
    let mut step = 0;
    run_test_mekf_sequence(
        |s, body, refs| s.estimate_attitude(body, refs, [0.03, 0.03]),
        |q, s| {
            step += 1;
            if step == 180 || step == 300 {
                assert_relative_eq!(s.q_hat.0, q.0, epsilon = 1e-3);
            }
            if step == 300 {
                assert_relative_eq!(s.b_hat.inner(), V3::zeros().inner(), epsilon = 1e-2);
            }
        },
    );
}

/// The Joseph form changes `estimate_attitude` only by rounding.
#[test]
fn estimate_attitude_matches_pre_split() {
    let mut reference = vec![];
    run_test_mekf_sequence(
        |s, body, refs| estimate_attitude_pre_split(s, body, refs, [0.03, 0.03]),
        |_, s| reference.push(s.clone()),
    );
    let mut i = 0;
    run_test_mekf_sequence(
        |s, body, refs| s.estimate_attitude(body, refs, [0.03, 0.03]),
        |_, s| {
            let old = &reference[i];
            i += 1;
            let dq: f64 = (s.q_hat.0 - old.q_hat.0).norm().into_buf();
            assert!(dq < 1e-9, "step {i}: q_hat moved {dq}");
            let dp = max_abs(s.p - old.p) / max_abs(old.p);
            assert!(dp < 1e-9, "step {i}: P moved {dp} relative");
        },
    );
    assert_eq!(i, 300);
}

/// Pins body←ECI (`b = q⁻¹ r = R(q)ᵀ r` with `q` stored `[x, y, z, w]` and
/// Hamilton's product), and the sign of the error `integrate_body` applies.
#[test]
fn test_quaternion_convention() {
    let mut rng = Rng(7);
    for _ in 0..100 {
        let q = rng.quat();
        let r = rng.unit();
        let [x, y, z, w] = q.0.into_buf();
        let rot = [
            [
                1.0 - 2.0 * (y * y + z * z),
                2.0 * (x * y - z * w),
                2.0 * (x * z + y * w),
            ],
            [
                2.0 * (x * y + z * w),
                1.0 - 2.0 * (x * x + z * z),
                2.0 * (y * z - x * w),
            ],
            [
                2.0 * (x * z - y * w),
                2.0 * (y * z + x * w),
                1.0 - 2.0 * (x * x + y * y),
            ],
        ];
        let rv = r.into_buf();
        let rot_t_r: [f64; 3] = core::array::from_fn(|i| (0..3).map(|j| rot[j][i] * rv[j]).sum());
        assert_relative_eq!(
            (q.inverse() * r).into_buf().as_slice(),
            rot_t_r.as_slice(),
            epsilon = 1e-14
        );

        let d_alpha: V3 = rng.unit() * 1e-4;
        let half: V3 = d_alpha * 0.5;
        let angle: f64 = half.norm().into_buf();
        let exp = Quaternion((half * (angle.sin() / angle)).concat(tensor![angle.cos()]));
        let q_next = q.integrate_body(d_alpha);
        let dq: f64 = (q_next.0 - (q * exp).0).norm().into_buf();
        assert!(dq < 1e-7, "integrate_body is not q ⊗ exp(δα/2): {dq}");

        let b = q.inverse() * r;
        let b_next = q_next.inverse() * r;
        let predicted = b + b.skew().dot(&d_alpha);
        let err: f64 = (b_next - predicted).norm().into_buf();
        assert!(err < 1e-7, "body vector moved off [b×]δα: {err}");
    }
}

#[test]
fn empty_update_is_a_no_op() {
    let mut s = filter();
    s.q_hat = Rng(3).quat();
    let before = s.clone();
    assert_eq!(s.update_vectors(&[], 13.8), UpdateCounts::default());
    assert_eq!(s.q_hat.0.into_buf(), before.q_hat.0.into_buf());
    assert_eq!(s.p.into_buf(), before.p.into_buf());
}

/// 10⁴ tight star vector updates, 32 per step, from `P = I` and a 1° error
/// keep `P` symmetric and positive definite.
#[test]
fn many_tight_vectors_keep_p_symmetric_positive_definite() {
    let mut rng = Rng(11);
    let truth = rng.quat();
    let boresight: V3 = tensor![0.0, 0.0, 1.0];
    let stars: [VectorObs; 32] = core::array::from_fn(|_| {
        let b = (boresight + rng.unit() * 0.17).normalize();
        VectorObs {
            body: b,
            reference: truth * b,
            sigma: 3e-5,
        }
    });
    let mut s = filter();
    s.q_hat = truth * Quaternion::from_axis_angle(rng.unit(), 1f64.to_radians());
    for step in 0..313 {
        s.omega = V3::zeros();
        s.propagate();
        let u = s.update_vectors(&stars, f64::INFINITY);
        assert_eq!(u.used, 32, "step {step}");
        let asym = max_abs(s.p - s.p.transpose());
        assert!(
            asym <= 1e-15 * max_abs(s.p),
            "step {step}: asymmetry {asym}"
        );
        assert!(
            s.p.try_cholesky().is_ok(),
            "step {step}: P not positive definite"
        );
    }
    let err: f64 = s.q_hat.angular_distance(&truth).into_buf();
    assert!(err < 1e-6, "attitude error {err}");
}

/// One vector constrains the two axes across it, not the axis along it.
#[test]
fn one_vector_leaves_its_own_axis_at_prior() {
    let mut rng = Rng(5);
    let truth = rng.quat();
    let mut s = filter();
    s.q_hat = truth;
    let b = rng.unit();
    let u = s.update_vectors(&[obs(truth, truth * b, 3e-5)], f64::INFINITY);
    assert_eq!(u, UpdateCounts { used: 1, gated: 0 });
    assert_relative_eq!(theta_variance(&s.p, b), 1.0, epsilon = 1e-9);
    let across = b.cross(&rng.unit()).normalize();
    assert!(theta_variance(&s.p, across) < 1e-8);
}

/// A converged filter with two vectors at σ = 1 mrad.
fn converged() -> (State, Quat, [V3; 2]) {
    let mut rng = Rng(9);
    let truth = rng.quat();
    let refs = [rng.unit(), rng.unit()];
    let mut s = filter();
    s.q_hat = truth;
    for _ in 0..50 {
        s.omega = V3::zeros();
        s.propagate();
        s.update_vectors(&refs.map(|r| obs(truth, r, 1e-3)), 13.8);
    }
    (s, truth, refs)
}

#[test]
fn gate_rejects_outlier_and_keeps_inlier() {
    let (mut s, truth, refs) = converged();
    let good = obs(truth, refs[0], 1e-3);
    let off_axis = good.body.cross(&refs[1]).normalize();
    let five_deg = Quaternion::from_axis_angle(off_axis, 5f64.to_radians());
    let outlier = VectorObs {
        body: five_deg * good.body,
        ..good
    };
    let before = s.clone();
    assert_eq!(
        s.update_vectors(&[outlier], 13.8),
        UpdateCounts { used: 0, gated: 1 }
    );
    assert_eq!(s.q_hat.0.into_buf(), before.q_hat.0.into_buf());
    assert_eq!(s.p.into_buf(), before.p.into_buf());

    // In a pair the outlier alone goes, and so it does among three.
    let mut pair = s.clone();
    assert_eq!(
        pair.update_vectors(&[outlier, good], 13.8),
        UpdateCounts { used: 1, gated: 1 }
    );
    let other = obs(truth, refs[1], 1e-3);
    assert_eq!(
        s.update_vectors(&[outlier, good, other], 13.8),
        UpdateCounts { used: 2, gated: 1 }
    );
    let err: f64 = s.q_hat.angular_distance(&truth).into_buf();
    assert!(err < 1e-3, "attitude error {err}");
}

/// The half-frame floor rounds up: 3 of 5 may go only if 3 remain, so
/// 2 outliers in 5 are dropped and 3 outliers in 5 reject the frame.
#[test]
fn half_frame_floor_rounds_up() {
    let mut rng = Rng(41);
    let truth: Quat = Quaternion::from_axis_angle(rng.unit(), 1.0);
    let mut s = filter();
    s.q_hat = truth;
    s.update_vectors(&noisy_star_field(&mut rng, truth, 3e-5), 13.8);
    let frame = |outliers: usize, rng: &mut Rng| {
        let mut stars = noisy_star_field(rng, truth, 3e-5);
        for st in stars.iter_mut().take(outliers) {
            st.body = Quaternion::from_axis_angle(V3::x_axis(), 0.01) * st.body;
        }
        stars
    };
    let mut two = s.clone();
    let counts = two.update_vectors(&frame(2, &mut rng)[..5], 13.8);
    assert_eq!(counts, UpdateCounts { used: 3, gated: 2 });
    let mut three = s.clone();
    let counts = three.update_vectors(&frame(3, &mut rng)[..5], 13.8);
    assert_eq!(counts, UpdateCounts { used: 0, gated: 5 });
}

#[test]
fn ungated_outlier_is_fused() {
    let (mut s, truth, refs) = converged();
    let mut outlier = obs(truth, refs[0], 1e-3);
    outlier.body = (outlier.body + tensor![0.1, 0.0, 0.0]).normalize();
    let u = s.update_vectors(&[outlier], f64::INFINITY);
    assert_eq!(u, UpdateCounts { used: 1, gated: 0 });
}

#[test]
fn invalid_observations_are_gated_not_fused() {
    let (mut s, truth, refs) = converged();
    let good = obs(truth, refs[0], 1e-3);
    let nan: V3 = tensor![f64::NAN, 0.0, 0.0];
    let bad = [
        VectorObs { body: nan, ..good },
        VectorObs {
            reference: nan,
            ..good
        },
        VectorObs {
            sigma: f64::NAN,
            ..good
        },
        VectorObs { sigma: 0.0, ..good },
        VectorObs {
            sigma: -1e-3,
            ..good
        },
    ];
    let before = s.clone();
    let u = s.update_vectors(&bad, f64::INFINITY);
    assert_eq!(u, UpdateCounts { used: 0, gated: 5 });
    assert!(!s.is_non_finite());
    assert_eq!(s.q_hat.0.into_buf(), before.q_hat.0.into_buf());
    assert_eq!(s.p.into_buf(), before.p.into_buf());
}

/// The gate looks only across the predicted vector, so a radial error does
/// not gate a measurement whose direction is right.
#[test]
fn gate_ignores_radial_error() {
    let (mut s, truth, refs) = converged();
    let mut long = obs(truth, refs[0], 1e-3);
    long.body = long.body * 1.5;
    assert_eq!(
        s.update_vectors(&[long], 13.8),
        UpdateCounts { used: 1, gated: 0 }
    );
}

/// 32 star vectors at σ = 30 µrad in a 20° field about body +z.
fn star_field(rng: &mut Rng, truth: Quat) -> [VectorObs; 32] {
    core::array::from_fn(|_| {
        let b = (tensor![0.0, 0.0, 1.0] + rng.unit() * 0.17).normalize();
        VectorObs {
            body: b,
            reference: truth * b,
            sigma: 3e-5,
        }
    })
}

/// One frame from identity and `P = I` with the truth 170° away: the plain
/// update is left far off, and the iterated one lands on the truth with the
/// covariance a single update at the truth gives.
#[test]
fn iterated_update_converges_from_far_away() {
    let mut rng = Rng(13);
    let truth: Quat = Quaternion::from_axis_angle(rng.unit(), 170f64.to_radians());
    let stars = star_field(&mut rng, truth);

    let mut plain = filter();
    plain.update_vectors(&stars, 13.8);
    let plain_err: f64 = plain.q_hat.angular_distance(&truth).into_buf();
    assert!(plain_err > 1f64.to_radians(), "{plain_err}");

    let mut iterated = filter();
    let counts = iterated.update_vectors_iterated(&stars, 13.8, 10);
    assert_eq!(counts, UpdateCounts { used: 32, gated: 0 });
    let err: f64 = iterated.q_hat.angular_distance(&truth).into_buf();
    assert!(err < 1e-6, "{err}");

    let mut at_truth = filter();
    at_truth.q_hat = truth;
    at_truth.update_vectors(&stars, 13.8);
    let dp = max_abs(iterated.p - at_truth.p) / max_abs(at_truth.p);
    assert!(dp < 1e-3, "P differs by {dp} relative");
}

/// Near the truth the iterated update is the plain one: its second pass
/// moves `q_hat` by less than the stop tolerance.
#[test]
fn iterated_update_matches_plain_near_truth() {
    let (s, truth, refs) = converged();
    let obs = refs.map(|r| obs(truth, r, 1e-3));
    let (mut plain, mut iterated) = (s.clone(), s);
    plain.update_vectors(&obs, 13.8);
    iterated.update_vectors_iterated(&obs, 13.8, 10);
    let dq: f64 = iterated.q_hat.angular_distance(&plain.q_hat).into_buf();
    assert!(dq.abs() < 1e-9, "{dq}");
}

/// The last pass still gates: an outlier among good vectors is rejected.
#[test]
fn iterated_update_gates_on_its_last_pass() {
    let mut rng = Rng(17);
    let truth: Quat = Quaternion::from_axis_angle(rng.unit(), 1.0);
    let mut stars = star_field(&mut rng, truth);
    stars[5].body = Quaternion::from_axis_angle(tensor![1.0, 0.0, 0.0], 0.01) * stars[5].body;
    let mut s = filter();
    let counts = s.update_vectors_iterated(&stars, 13.8, 10);
    assert_eq!(counts, UpdateCounts { used: 31, gated: 1 });
    let err: f64 = s.q_hat.angular_distance(&truth).into_buf();
    assert!(err < 1e-4, "{err}");
}

/// `star_field` with each body vector turned by about `sigma` across itself.
fn noisy_star_field(rng: &mut Rng, truth: Quat, sigma: f64) -> [VectorObs; 32] {
    let mut stars = star_field(rng, truth);
    for s in &mut stars {
        let n = rng.unit() * sigma;
        s.body = (s.body + n - s.body * s.body.dot(&n).into_buf()).normalize();
        s.sigma = sigma;
    }
    stars
}

fn converges_from(start: Quat, truth: Quat, stars: &[VectorObs]) -> f64 {
    let mut s = filter();
    s.q_hat = start;
    s.update_vectors_iterated(stars, 13.8, 10);
    s.q_hat.angular_distance(&truth).into_buf()
}

/// Starts just short of a half turn sit near the Gauss–Newton saddle; the
/// restarts get out of it.
#[test]
fn iterated_update_escapes_the_half_turn_saddle() {
    let mut rng = Rng(19);
    let truth: Quat = Quaternion::from_axis_angle(rng.unit(), 0.7);
    let stars = noisy_star_field(&mut rng, truth, 3e-5);
    for axis in [V3::x_axis(), V3::y_axis(), V3::z_axis()] {
        for deg in [179.9, 180.0] {
            let start = truth * Quaternion::from_axis_angle(axis, -f64::to_radians(deg));
            let err = converges_from(start, truth, &stars);
            assert!(err < 1e-4, "{deg}° about {:?}: {err}", axis.into_buf());
        }
    }
}

/// From identity, 500 random attitudes (any angle) converge in one frame.
#[test]
fn iterated_update_converges_from_random_attitudes() {
    let mut rng = Rng(23);
    for trial in 0..500 {
        let truth = Quaternion(
            Vector::from_buf([rng.next(), rng.next(), rng.next(), rng.next()]).normalize(),
        );
        let stars = noisy_star_field(&mut rng, truth, 3e-5);
        let err = converges_from(Quaternion::identity(), truth, &stars);
        assert!(err < 1e-4, "trial {trial}: {err}");
    }
}

/// When no start reaches a fixed point within the cap, the state stays at the
/// prior and the frame counts as gated.
#[test]
fn iterated_update_that_does_not_converge_changes_nothing() {
    let mut rng = Rng(29);
    let truth: Quat = Quaternion::from_axis_angle(rng.unit(), 2.0);
    let stars = star_field(&mut rng, truth);
    let mut s = filter();
    let before = s.clone();
    assert_eq!(
        s.update_vectors_iterated(&stars, 13.8, 1),
        UpdateCounts { used: 0, gated: 32 }
    );
    assert_eq!(s.q_hat.0.into_buf(), before.q_hat.0.into_buf());
    assert_eq!(s.p.into_buf(), before.p.into_buf());
}

/// A prior 4σ off still fuses: the gate looks at the fit, not the prior.
#[test]
fn prior_four_sigma_off_still_fuses() {
    let mut rng = Rng(31);
    let truth: Quat = Quaternion::from_axis_angle(rng.unit(), 1.0);
    let mut s = filter();
    s.q_hat = truth;
    s.update_vectors(&noisy_star_field(&mut rng, truth, 3e-5), 13.8);
    s.omega = V3::zeros();
    s.propagate();
    let axis = V3::x_axis();
    let sigma_x = s.p.into_buf()[0][0].sqrt();
    s.q_hat = s.q_hat * Quaternion::from_axis_angle(axis, 4.0 * sigma_x);
    let counts = s.update_vectors(&noisy_star_field(&mut rng, truth, 3e-5), 13.8);
    assert_eq!(counts, UpdateCounts { used: 32, gated: 0 });
}

/// One gross outlier among good stars is dropped and the rest fuse.
#[test]
fn one_outlier_in_a_frame_is_dropped() {
    let mut rng = Rng(37);
    let truth: Quat = Quaternion::from_axis_angle(rng.unit(), 1.0);
    let mut s = filter();
    s.q_hat = truth;
    s.update_vectors(&noisy_star_field(&mut rng, truth, 3e-5), 13.8);
    let mut stars = noisy_star_field(&mut rng, truth, 3e-5);
    stars[9].body = Quaternion::from_axis_angle(V3::y_axis(), 0.01) * stars[9].body;
    let counts = s.update_vectors(&stars, 13.8);
    assert_eq!(counts, UpdateCounts { used: 31, gated: 1 });
    let err: f64 = s.q_hat.angular_distance(&truth).into_buf();
    assert!(err < 1e-4, "{err}");
}
