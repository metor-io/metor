use super::*;
use crate::geom::norm;

fn intrinsics() -> Intrinsics {
    Intrinsics {
        f_m: 16e-3,
        pitch_m: 5.5e-6,
        cx: 512.0,
        cy: 512.0,
        k1: 0.0,
        k2: 0.0,
        p1: 0.0,
        p2: 0.0,
        width: 1024,
        height: 1024,
    }
}

/// About 0.8% radial distortion at the corners and 0.1 px of tangential.
fn distorted() -> Intrinsics {
    Intrinsics {
        k1: 500.0,
        k2: 2e4,
        p1: 0.05,
        p2: -0.03,
        cx: 515.3,
        cy: 508.9,
        ..intrinsics()
    }
}

fn camera(intr: Intrinsics) -> Camera {
    Camera {
        intr,
        q_b_st: Quat::IDENTITY,
    }
}

/// Pixel → vector → pixel on a 33×33 grid from corner to corner.
fn assert_round_trips(cam: &Camera) {
    let (w, h) = (cam.intr.width as f64, cam.intr.height as f64);
    for i in 0..=32 {
        for j in 0..=32 {
            let (u, v) = (w * i as f64 / 32.0, h * j as f64 / 32.0);
            let b = cam.pixel_to_st(u, v).expect("on-sensor pixel inverts");
            let (u2, v2) = cam.st_to_pixel(b).expect("projects back");
            let err = (u2 - u).abs().max((v2 - v).abs());
            assert!(err < 1e-9, "({u}, {v}) -> ({u2}, {v2}): {err} px");
        }
    }
}

#[test]
fn round_trip_without_distortion() {
    assert_round_trips(&camera(intrinsics()));
}

#[test]
fn round_trip_with_distortion() {
    assert_round_trips(&camera(distorted()));
}

#[test]
fn zero_distortion_is_the_pinhole() {
    let cam = camera(intrinsics());
    let i = cam.intr;
    let (u, v) = (100.25, 900.5);
    let pinhole = normalize([(u - i.cx) * i.pitch_m, (v - i.cy) * i.pitch_m, i.f_m]).unwrap();
    assert_eq!(cam.pixel_to_st(u, v), Some(pinhole));
    assert_eq!(i.distort(1e-3, -2e-3), (1e-3, -2e-3));
}

#[test]
fn principal_point_is_the_boresight() {
    for intr in [intrinsics(), distorted()] {
        let cam = camera(intr);
        assert_eq!(cam.pixel_to_st(intr.cx, intr.cy), Some([0.0, 0.0, 1.0]));
        assert_eq!(cam.st_to_pixel([0.0, 0.0, 1.0]), Some((intr.cx, intr.cy)));
    }
}

/// `+u` is `+x`: a pixel right of centre looks toward `+x`.
#[test]
fn pixel_axes_follow_the_frame() {
    let cam = camera(intrinsics());
    let b = cam.pixel_to_st(612.0, 512.0).unwrap();
    assert!(b[0] > 0.0 && b[1] == 0.0);
    let b = cam.pixel_to_st(512.0, 612.0).unwrap();
    assert!(b[1] > 0.0 && b[0] == 0.0);
    // Pixel 0's centre is half a pixel from its edge.
    let (u, _) = cam
        .st_to_pixel(cam.pixel_to_st(0.5, 512.0).unwrap())
        .unwrap();
    assert!((u - 0.5).abs() < 1e-9);
}

#[test]
fn behind_or_off_sensor_is_none() {
    let cam = camera(intrinsics());
    assert_eq!(cam.st_to_pixel([0.0, 0.0, -1.0]), None);
    assert_eq!(cam.st_to_pixel([1.0, 0.0, 0.0]), None);
    assert_eq!(cam.st_to_pixel([f64::NAN, 0.0, 1.0]), None);
    let past_edge = cam
        .pixel_to_st(1024.0 + EDGE_MARGIN_PX + 1.0, 512.0)
        .unwrap();
    assert_eq!(cam.st_to_pixel(past_edge), None);
    let in_margin = cam
        .pixel_to_st(1024.0 + EDGE_MARGIN_PX - 1.0, 512.0)
        .unwrap();
    assert!(cam.st_to_pixel(in_margin).is_some());
}

/// A barrel distortion strong enough that `x_d` peaks and turns back inside
/// the field: past the peak there is no ideal point, and on the folded side
/// the inverse is refused rather than taken from the wrong branch.
#[test]
fn folded_field_does_not_undistort() {
    let intr = Intrinsics {
        k1: -1e5,
        ..intrinsics()
    };
    // x_d = x (1 + k1 x²) peaks at x = 1/√(−3 k1) ≈ 1.83 mm, x_d ≈ 1.22 mm.
    assert_eq!(intr.undistort(1.5e-3, 0.0), None);
    let folded_x = 2.5e-3;
    let (xd, yd) = intr.distort(folded_x, 0.0);
    if let Some((x, _)) = intr.undistort(xd, yd) {
        assert!(x < 1.83e-3, "returned the folded branch {x}");
    }
    assert!(camera(intr).pixel_to_st(1023.0, 512.0).is_none());
}

#[test]
fn undistort_rejects_non_finite() {
    assert_eq!(distorted().undistort(f64::NAN, 0.0), None);
    assert_eq!(distorted().undistort(f64::INFINITY, 0.0), None);
}

#[test]
fn mount_rotates_both_ways() {
    let h = core::f64::consts::FRAC_1_SQRT_2;
    let q = Quat([0.0, h, 0.0, h]);
    let cam = Camera {
        intr: intrinsics(),
        q_b_st: q,
    };
    let b_st = [0.1, -0.2, 0.97];
    let body = cam.st_to_body(b_st);
    assert_eq!(body, q.rotate(b_st));
    let back = cam.body_to_st(body);
    assert!(b_st.iter().zip(back).all(|(a, b)| (a - b).abs() < 1e-15));
}

#[test]
fn angles_from_pixels() {
    let cam = camera(intrinsics());
    assert!((cam.pixel_sigma_to_rad(0.1) - 0.1 * 5.5e-6 / 16e-3).abs() < 1e-18);
    // Half the 1024-pixel diagonal at 5.5 µm over 16 mm.
    let half_diag = (512.0f64 * 5.5e-6 * 2f64.sqrt() / 16e-3).atan();
    assert!((cam.half_diagonal_rad() - half_diag).abs() < 1e-12);
    assert!(camera(distorted()).half_diagonal_rad() < half_diag * 1.01);
}

#[test]
fn aberration_is_first_order_and_unit() {
    let b = [0.0, 0.0, 1.0];
    let beta = [1e-4, 0.0, 0.0];
    let a = aberrate(b, beta).unwrap();
    assert!((norm(a) - 1.0).abs() < 1e-15);
    assert!((a[0] - 1e-4).abs() < 1e-12);
    assert_eq!(aberrate(b, [0.0; 3]), Some(b));
}

/// `k₁` giving `k₁ r² = fraction` at the ideal radius `r` that images onto
/// the sensor corner, `r (1 + fraction) = r_corner`.
fn corner_k1(fraction: f64) -> f64 {
    let r_corner = 512.0 * 2f64.sqrt() * 5.5e-6;
    let r = r_corner / (1.0 + fraction);
    fraction / (r * r)
}

#[test]
fn round_trip_at_twenty_percent_distortion() {
    for fraction in [0.2, -0.2] {
        let intr = Intrinsics {
            k1: corner_k1(fraction),
            ..intrinsics()
        };
        assert_round_trips(&camera(intr));
    }
}

/// With `k₁ < 0` and `k₂ > 0` the radial map rises, folds back, and rises
/// again. A point on the far rising branch has a positive Jacobian
/// determinant but is not on the central branch, so neither direction maps it.
#[test]
fn far_branch_is_refused() {
    let intr = Intrinsics {
        k1: -1e5,
        k2: 4e9,
        ..intrinsics()
    };
    let far = 3.5e-3;
    assert!(1.0 + 3.0 * intr.k1 * far * far + 5.0 * intr.k2 * far.powi(4) > 0.0);
    assert!(!intr.unfolded(far, 0.0));
    let (xd, _) = intr.distort(far, 0.0);
    if let Some((x, _)) = intr.undistort(xd, 0.0) {
        assert!(intr.unfolded(x, 0.0) && x < far, "returned {x}");
    }
    let cam = camera(intr);
    let n = (far * far + 16e-3 * 16e-3).sqrt();
    assert_eq!(cam.st_to_pixel([far / n, 0.0, 16e-3 / n]), None);
}

/// A direction past the fold of a strong barrel lens does not project.
#[test]
fn folded_direction_does_not_project() {
    let cam = camera(Intrinsics {
        k1: -1e5,
        ..intrinsics()
    });
    let x: f64 = 2.5e-3;
    let n = (x * x + 16e-3 * 16e-3).sqrt();
    assert_eq!(cam.st_to_pixel([x / n, 0.0, 16e-3 / n]), None);
    let x: f64 = 1.0e-3;
    let n = (x * x + 16e-3 * 16e-3).sqrt();
    assert!(cam.st_to_pixel([x / n, 0.0, 16e-3 / n]).is_some());
}
