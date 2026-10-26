use std::vec::Vec;

use super::photometry::G_TO_V;
use super::scene::MAX_SUB;
use super::testkit::{NO_HIP, cat_bytes};
use super::truth::MAX_TRUTH;
use super::*;
use crate::geom::{Quat, cross, norm, transpose};

const DEG: f64 = core::f64::consts::PI / 180.0;

fn still(q_b_eci: Quat) -> Motion {
    Motion {
        q_b_eci,
        omega_b: [0.0; 3],
        beta: [0.0; 3],
        t_exp: 0.1,
    }
}

fn frame(motion: &Motion) -> Frame {
    Frame::new(&SIM_CAMERA, motion, MAX_SUB, 1, 0).unwrap()
}

/// The direction that lands on pixel `(u, v)` at an identity attitude.
fn at_pixel(u: f64, v: f64) -> V3 {
    SIM_CAMERA.pixel_to_st(u, v).unwrap()
}

fn scene<'a>(cat: StarCat<'a>, mag_limit: f32) -> Scene<'a> {
    let hot = HotPixels::none(SIM_SHAPE.pixels());
    Scene::new(
        cat,
        SIM_CAMERA,
        SIM_PHOTOMETRY,
        SIM_SENSOR,
        SIM_SHAPE,
        hot,
        mag_limit,
    )
    .unwrap()
}

/// Prepares one frame of `stars` and returns its truth and splat count.
fn prepare(stars: &[(V3, f32, u32)], motion: &Motion) -> (Truth, usize) {
    let bytes = cat_bytes(stars);
    let mut s = scene(StarCat::parse(&bytes).unwrap(), 12.0);
    let truth = s.prepare(&frame(motion));
    (truth, s.splats().len())
}

/// V = 0 ↔ 8.8e5 ph/s/cm², D = 2.5 cm, mag 7, τ·QE = 0.5 and 100 ms.
#[test]
fn photometry_anchor() {
    let e = SIM_PHOTOMETRY.electrons(7.0, 0.1);
    assert!((e - 342.0).abs() < 1.0, "{e}");
}

#[test]
fn truth_maps_back_to_the_catalog() {
    let q = Quat([0.2, -0.4, 0.1, 0.9]).normalized().unwrap();
    let f = frame(&still(q));
    let rt = transpose(f.r_st_eci());
    let stars: Vec<_> = [(100.5, 900.25), (511.0, 3.0), (1000.9, 1000.1)]
        .iter()
        .map(|&(u, v)| (mat_vec(rt, at_pixel(u, v)), 4.0, NO_HIP))
        .collect();
    let bytes = cat_bytes(&stars);
    let cat = StarCat::parse(&bytes).unwrap();
    let truth = scene(cat, 12.0).prepare(&f);
    assert_eq!(truth.count, 3);
    for t in truth.brightest() {
        let eci = mat_vec(rt, SIM_CAMERA.pixel_to_st(t.u, t.v).unwrap());
        let sep = cat.all_stars().iter().map(|s| norm(cross(s.unit(), eci)));
        assert!(sep.fold(f64::MAX, f64::min) < 1e-9);
    }
}

#[test]
fn truth_lists_the_brightest_with_hip() {
    let g = |i: u32| 1.0 + 0.15 * i as f32;
    let hip = |i: u32| {
        if i.is_multiple_of(3) {
            1000 + i
        } else {
            NO_HIP
        }
    };
    let stars: Vec<_> = (0..40u32)
        .map(|i| {
            let (u, v) = (
                50.0 + 23.0 * f64::from(i % 10),
                50.0 + 200.0 * f64::from(i / 10),
            );
            (at_pixel(u, v), g(i), hip(i))
        })
        .collect();
    let (truth, _) = prepare(&stars, &still(Quat::IDENTITY));
    assert_eq!((truth.count, truth.n_detectable), (MAX_TRUTH, 40));
    for (i, t) in (0u32..).zip(truth.brightest()) {
        assert_eq!(t.cat_id, hip(i), "entry {i}");
        assert!((t.mag - (g(i) + G_TO_V[0])).abs() < 1e-3, "entry {i}");
    }
}

/// Off-sensor stars are splatted (their PSF can reach in) but not listed;
/// stars at the magnitude limit are skipped.
#[test]
fn edges_and_the_magnitude_limit() {
    let stars = [
        (at_pixel(-0.25, 500.5), 4.0, NO_HIP),
        (at_pixel(1023.9, 1023.9), 4.0, NO_HIP),
        (at_pixel(300.0, 300.0), 12.0, NO_HIP),
    ];
    let (truth, splats) = prepare(&stars, &still(Quat::IDENTITY));
    assert_eq!((truth.count, splats), (1, 2));
}

/// A faint star and a streaked star are listed but not detectable.
#[test]
fn detectability() {
    let star = [(at_pixel(500.5, 400.5), 6.0, NO_HIP)];
    let faint = [(at_pixel(500.5, 400.5), 11.0, NO_HIP)];
    let streak = Motion {
        omega_b: [5.0 * DEG, 0.0, 0.0],
        ..still(Quat::IDENTITY)
    };
    let detected = |stars: &[_], m| {
        let (truth, _) = prepare(stars, &m);
        (truth.count, truth.n_detectable)
    };
    assert_eq!(detected(&star, still(Quat::IDENTITY)), (1, 1));
    assert_eq!(detected(&faint, still(Quat::IDENTITY)), (1, 0));
    assert_eq!(detected(&star, streak), (1, 0));
}

/// 5°/s streaks a star about 25 px: every sub-step is splatted, centred on
/// the mid-exposure truth, and slow rates take one sub-step.
#[test]
fn motion_blur_substeps_centre_on_mid_exposure() {
    let motion = Motion {
        omega_b: [5.0 * DEG, 0.0, 0.0],
        ..still(Quat::IDENTITY)
    };
    let f = frame(&motion);
    assert_eq!(f.substeps().len(), MAX_SUB);
    let dir = mat_vec(transpose(f.r_st_eci()), at_pixel(500.3, 400.7));
    let bytes = cat_bytes(&[(dir, 3.0, NO_HIP)]);
    let mut s = scene(StarCat::parse(&bytes).unwrap(), 12.0);
    let t = s.prepare(&f).stars[0];
    assert!(
        (t.u - 500.3).abs() < 1e-3 && (t.v - 400.7).abs() < 1e-3,
        "{t:?}"
    );
    let splats = s.splats();
    assert_eq!(splats.len(), MAX_SUB);
    let n = splats.len() as f64;
    let mean_v = splats.iter().map(|p| f64::from(p.v)).sum::<f64>() / n;
    assert!((mean_v - t.v).abs() < 0.05, "{mean_v}");
    let total: f32 = splats.iter().map(|p| p.e).sum();
    let e = SIM_PHOTOMETRY.electrons(3.0 + G_TO_V[0], 0.1);
    assert!((total / e - 1.0).abs() < 1e-5);
    let slow = Motion {
        omega_b: [1e-4 * DEG, 0.0, 0.0],
        ..motion
    };
    assert_eq!(frame(&slow).substeps().len(), 1);
}

/// `r_st_eci = A(q_b_st)ᵀ A(q_b_eci)`, which is `A(q_b_eci ⊗ q_b_st⁻¹)`.
#[test]
fn frame_attitude_composition() {
    let q_b_st = Quat([0.3, 0.1, -0.2, 0.9]).normalized().unwrap();
    let q_b_eci = Quat([-0.1, 0.5, 0.2, 0.8]).normalized().unwrap();
    let cam = Camera {
        q_b_st,
        ..SIM_CAMERA
    };
    let f = Frame::new(&cam, &still(q_b_eci), 1, 0, 0).unwrap();
    assert!(f.q_cam_eci().angle_to(q_b_eci * q_b_st.conj()) < 1e-12);
    let r = [0.6, 0.0, 0.8];
    let direct = cam.body_to_st(q_b_eci.rotate(r));
    let ours = mat_vec(f.r_st_eci(), r);
    assert!((0..3).all(|i| (direct[i] - ours[i]).abs() < 1e-15));
}

#[test]
fn hot_pixels() {
    let n = 1 << 20;
    let hot = HotPixels::new(n, 1e-3, 100.0, 3);
    let count = hot.as_slice().iter().filter(|&&m| m > 1.0).count() as f64;
    let expect = 1e-3 * n as f64;
    assert!((count - expect).abs() < 4.0 * expect.sqrt(), "{count}");
    assert!(hot.as_slice().iter().all(|&m| (1.0..=100.0).contains(&m)));
    assert_eq!(hot, HotPixels::new(n, 1e-3, 100.0, 3));
}
