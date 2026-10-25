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
    let q_b_st = Quat::IDENTITY;
    Camera { intr, q_b_st }
}

/// The unit direction at ideal focal-plane `x` (m) on the `+x` axis.
fn at_x(x: f64) -> V3 {
    let n = (x * x + 16e-3 * 16e-3).sqrt();
    [x / n, 0.0, 16e-3 / n]
}

/// Pixel → vector → pixel on a 33×33 grid from corner to corner, without
/// distortion, with a little, and with ±20% at the corners.
#[test]
fn pixels_round_trip() {
    let r_corner = 512.0 * 2f64.sqrt() * 5.5e-6;
    let corner_k1 = |f: f64| f / (r_corner / (1.0 + f)).powi(2);
    let strong = [0.2, -0.2].map(|f| Intrinsics {
        k1: corner_k1(f),
        ..intrinsics()
    });
    for intr in [intrinsics(), distorted(), strong[0], strong[1]] {
        let cam = camera(intr);
        for i in 0..=32 {
            for j in 0..=32 {
                let (u, v) = (1024.0 * f64::from(i) / 32.0, 1024.0 * f64::from(j) / 32.0);
                let b = cam.pixel_to_st(u, v).expect("on-sensor pixel inverts");
                let (u2, v2) = cam.st_to_pixel(b).expect("projects back");
                assert!((u2 - u).abs().max((v2 - v).abs()) < 1e-9, "({u}, {v})");
            }
        }
    }
}

/// The principal point is the boresight, `+u` is `+x`, and zero distortion
/// is the pinhole.
#[test]
fn pixel_geometry() {
    for intr in [intrinsics(), distorted()] {
        let cam = camera(intr);
        assert_eq!(cam.pixel_to_st(intr.cx, intr.cy), Some([0.0, 0.0, 1.0]));
        assert_eq!(cam.st_to_pixel([0.0, 0.0, 1.0]), Some((intr.cx, intr.cy)));
    }
    let cam = camera(intrinsics());
    let b = cam.pixel_to_st(612.0, 512.0).unwrap();
    assert!(b[0] > 0.0 && b[1] == 0.0);
    let i = cam.intr;
    let (u, v) = (100.25, 900.5);
    let pinhole = normalize([(u - i.cx) * i.pitch_m, (v - i.cy) * i.pitch_m, i.f_m]);
    assert_eq!(cam.pixel_to_st(u, v), pinhole);
    let half_diag = (512.0f64 * 5.5e-6 * 2f64.sqrt() / 16e-3).atan();
    assert!((cam.half_diagonal_rad() - half_diag).abs() < 1e-12);
}

#[test]
fn behind_or_off_sensor_is_none() {
    let cam = camera(intrinsics());
    assert_eq!(cam.st_to_pixel([0.0, 0.0, -1.0]), None);
    assert_eq!(cam.st_to_pixel([1.0, 0.0, 0.0]), None);
    assert_eq!(cam.st_to_pixel([f64::NAN, 0.0, 1.0]), None);
    let edge = |du: f64| {
        cam.pixel_to_st(1024.0 + EDGE_MARGIN_PX + du, 512.0)
            .unwrap()
    };
    assert_eq!(cam.st_to_pixel(edge(1.0)), None);
    assert!(cam.st_to_pixel(edge(-1.0)).is_some());
    assert_eq!(distorted().undistort(f64::NAN, 0.0), None);
}

/// A barrel lens whose `x_d = x (1 + k₁x²)` peaks at x ≈ 1.83 mm: past the
/// peak there is no ideal point, and the folded side neither projects nor
/// undistorts onto the wrong branch. With `k₂ > 0` the map rises again; that
/// far branch is refused too.
#[test]
fn folded_lenses_refuse_the_wrong_branch() {
    let barrel = Intrinsics {
        k1: -1e5,
        ..intrinsics()
    };
    assert_eq!(barrel.undistort(1.5e-3, 0.0), None);
    let (xd, yd) = barrel.distort(2.5e-3, 0.0);
    assert!(barrel.undistort(xd, yd).is_none_or(|(x, _)| x < 1.83e-3));
    assert!(camera(barrel).pixel_to_st(1023.0, 512.0).is_none());
    assert_eq!(camera(barrel).st_to_pixel(at_x(2.5e-3)), None);
    assert!(camera(barrel).st_to_pixel(at_x(1.0e-3)).is_some());
    let far = Intrinsics { k2: 4e9, ..barrel };
    assert!(!far.unfolded(3.5e-3, 0.0));
    let (xd, _) = far.distort(3.5e-3, 0.0);
    assert!(
        far.undistort(xd, 0.0)
            .is_none_or(|(x, _)| far.unfolded(x, 0.0) && x < 3.5e-3)
    );
    assert_eq!(camera(far).st_to_pixel(at_x(3.5e-3)), None);
}

#[test]
fn aberration() {
    let a = aberrate([0.0, 0.0, 1.0], [1e-4, 0.0, 0.0]).unwrap();
    assert!((norm(a) - 1.0).abs() < 1e-15 && (a[0] - 1e-4).abs() < 1e-12);
    assert_eq!(aberrate([0.0, 0.0, 1.0], [0.0; 3]), Some([0.0, 0.0, 1.0]));
}
