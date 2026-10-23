use core::f64::consts::FRAC_1_SQRT_2;

use adcs_contracts::V3 as NoxV3;
use nox::tensor;

use super::*;

fn params(sigma_rad: f64) -> StarParams {
    StarParams {
        rate_hz: 10.0,
        fov_deg: 20.0,
        sigma_rad,
        n_stars_max: 32,
        seed: 1,
        q_b_st: [0.0, 0.0, 0.0, 1.0],
    }
}

fn truth() -> Quat {
    Quaternion::from_axis_angle(tensor![0.3, -1.0, 0.5], 2.0)
}

#[test]
fn every_star_is_inside_the_field_of_view() {
    let mut t = StarIdeal::new(StarParams {
        q_b_st: [0.0, FRAC_1_SQRT_2, 0.0, FRAC_1_SQRT_2],
        ..params(34e-6)
    });
    let q = to_star_quat(&truth());
    let frame = t.observe(q, Timestamp(0));
    assert!(frame.count >= 20, "{} stars", frame.count);
    let boresight_b = t.q_b_st.rotate([0.0, 0.0, 1.0]);
    let cos_half = 10f64.to_radians().cos();
    for (b, r, _) in frame.obs() {
        let r = r.into_buf();
        assert!(geom::dot(r, boresight_eci(q, t.q_b_st)) >= cos_half);
        assert!(geom::dot(b.into_buf(), boresight_b) >= cos_half - 1e-3);
    }
}

#[test]
fn noiseless_vectors_are_the_true_body_vectors() {
    let mut t = StarIdeal::new(params(0.0));
    let q_nox = truth();
    let frame = t.observe(to_star_quat(&q_nox), Timestamp(0));
    for (b, r, sigma) in frame.obs() {
        assert_eq!(sigma, 0.0);
        let expected: NoxV3 = q_nox.inverse() * r;
        let err: f64 = (b - expected).norm().into_buf();
        assert!(err < 1e-15, "{err}");
    }
}

#[test]
fn noiseless_solution_is_the_truth() {
    let mut t = StarIdeal::new(StarParams {
        sigma_rad: 0.0,
        ..params(0.0)
    });
    let q_nox = truth();
    let frame = t.observe(to_star_quat(&q_nox), Timestamp(0));
    let mut sol_frame = frame;
    for row in sol_frame
        .sigma
        .inner_mut()
        .buf
        .iter_mut()
        .take(frame.count as usize)
    {
        *row = 34e-6;
    }
    let sol = t.solve(&sol_frame);
    assert_eq!(sol.status(), Ok(StarStatus::Valid));
    assert_eq!(sol.mode(), Ok(StarMode::Ideal));
    let err = to_star_quat(&sol.q_b_eci).angle_to(to_star_quat(&q_nox));
    assert!(err < 1e-12, "{err}");
    assert_eq!(sol.n_matched as u64, frame.count);
}

#[test]
fn noise_has_the_configured_spread() {
    let mut rng = StdRng::seed_from_u64(2);
    let b = [0.0, 0.0, 1.0];
    let n = 20_000;
    let mean_sq = (0..n)
        .map(|_| {
            let p = perturb(b, 34e-6, &mut rng);
            let angle = geom::norm(geom::cross(b, p));
            angle * angle
        })
        .sum::<f64>()
        / n as f64;
    let rms_per_axis = (mean_sq / 2.0).sqrt();
    assert!((rms_per_axis / 34e-6 - 1.0).abs() < 0.03, "{rms_per_axis}");
}

#[test]
fn choose_picks_distinct_rows_and_caps_at_the_supply() {
    let mut rng = StdRng::seed_from_u64(3);
    let mut rows: Vec<u32> = (0..50).collect();
    let mut picked = choose(&mut rows, 32, &mut rng).to_vec();
    picked.sort();
    picked.dedup();
    assert_eq!(picked.len(), 32);
    let mut few = vec![4, 5];
    assert_eq!(choose(&mut few, 32, &mut rng).len(), 2);
    assert!(choose(&mut [], 32, &mut rng).is_empty());
}

#[test]
fn frames_follow_the_rate_on_a_cycle_clock() {
    let mut t = StarIdeal::new(params(0.0));
    let cycle = 8_333;
    let due: Vec<i64> = (0..37).filter(|k| t.due(Timestamp(k * cycle))).collect();
    assert_eq!(due, vec![0, 12, 24, 36]);
}

#[test]
fn empty_and_zero_frames_are_not_valid() {
    let mut t = StarIdeal::new(params(0.0));
    let empty = StarVectors::empty(Timestamp(0), 1);
    assert_eq!(t.solve(&empty).status(), Ok(StarStatus::NoStars));
    let mut zeros = StarVectors::empty(Timestamp(0), 2);
    zeros.count = MAX_STARS as u64;
    assert_eq!(t.solve(&zeros).status(), Ok(StarStatus::Degenerate));
}
