//! The GPU renderer: star photometry and centroids in the noiseless
//! accumulator, the noise in distribution, determinism and saturation.

use metor_star::geom::{Quat, V3};
use metor_star::render::gpu::{GpuError, GpuRenderer};
use metor_star::render::testkit::{NO_HIP, cat_bytes};
use metor_star::render::{
    Frame, HotPixels, Motion, RenderError, SIM_CAMERA, SIM_PHOTOMETRY, SIM_SENSOR, SIM_SHAPE,
    Scene, Sensor, Truth,
};
use metor_star::starcat::StarCat;

const SIZE: usize = 1024;

/// Shot noise only, 1 e⁻/DN, no bias, a deep well.
const QUIET: Sensor = Sensor {
    full_well_e: 1e7,
    read_noise_e: 0.0,
    dark_e_s: 0.0,
    gain_milli: 1000,
    bias_dn: 0.0,
    zodi_mag_arcsec2: None,
    ..SIM_SENSOR
};

fn at_pixel(u: f64, v: f64) -> V3 {
    SIM_CAMERA.pixel_to_st(u, v).expect("a direction")
}

fn gpu<'a>(cat: StarCat<'a>, sensor: Sensor, hot: HotPixels) -> GpuRenderer<'a> {
    let scene = Scene::new(
        cat,
        SIM_CAMERA,
        SIM_PHOTOMETRY,
        sensor,
        SIM_SHAPE,
        hot,
        12.0,
    );
    GpuRenderer::new(scene.expect("a scene")).expect("a GPU")
}

/// Renders one frame at an identity attitude, turning at `omega_b`.
fn render(gpu: &mut GpuRenderer<'_>, omega_b: V3, seed: u64) -> (Truth, Vec<u16>) {
    let mut out = vec![0; SIM_SHAPE.pixels()];
    let truth = gpu
        .render(&frame(omega_b, seed), &mut out)
        .expect("a frame");
    (truth, out)
}

fn frame(omega_b: V3, seed: u64) -> Frame {
    let q_b_eci = Quat::IDENTITY;
    let (beta, t_exp) = ([0.0; 3], 0.1);
    let motion = Motion {
        q_b_eci,
        omega_b,
        beta,
        t_exp,
    };
    Frame::new(&SIM_CAMERA, &motion, 32, seed, 0).expect("a frame")
}

/// Electrons and centre of gravity of `acc` within 12 px of `(u, v)`.
fn star(acc: &[u32], u: f64, v: f64) -> (f64, f64, f64) {
    let (mut s, mut su, mut sv) = (0.0, 0.0, 0.0);
    let (cu, cv) = (u as usize, v as usize);
    for y in cv.saturating_sub(12)..(cv + 13).min(SIZE) {
        for x in cu.saturating_sub(12)..(cu + 13).min(SIZE) {
            let c = f64::from(acc[y * SIZE + x]) / 256.0;
            s += c;
            su += c * (x as f64 + 0.5);
            sv += c * (y as f64 + 0.5);
        }
    }
    (s, su / s, sv / s)
}

fn cold() -> HotPixels {
    HotPixels::none(SIZE * SIZE)
}

fn moments(x: &[u16]) -> (f64, f64) {
    let n = x.len() as f64;
    let m = x.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let var = x.iter().map(|&v| (f64::from(v) - m).powi(2)).sum::<f64>() / (n - 1.0);
    (m, var)
}

#[test]
fn stars_land_with_their_photometry_and_centroid() {
    let places = [(300.3, 700.8), (512.0, 512.0), (20.71, 1000.05)];
    let stars: Vec<_> = places
        .iter()
        .map(|&(u, v)| (at_pixel(u, v), 5.0, NO_HIP))
        .collect();
    let bytes = cat_bytes(&stars);
    let mut gpu = gpu(StarCat::parse(&bytes).unwrap(), QUIET, cold());
    let (truth, _) = render(&mut gpu, [0.0; 3], 1);
    assert_eq!((truth.count, truth.n_rendered), (3, 3));
    let acc = gpu.read_acc().expect("the accumulator");
    let expect = f64::from(SIM_PHOTOMETRY.electrons(SIM_PHOTOMETRY.band_mag(5.0, 0.0), 0.1));
    for t in truth.brightest() {
        let (e, u, v) = star(&acc, t.u, t.v);
        assert!((e / expect - 1.0).abs() < 1e-4, "{e} e⁻ vs {expect}");
        assert!((u - t.u).hypot(v - t.v) < 0.02, "({u}, {v}) vs {t:?}");
    }
    // A streak keeps the flux and centres on mid-exposure.
    let (truth, _) = render(&mut gpu, [0.0, 2f64.to_radians(), 0.0], 1);
    let acc = gpu.read_acc().expect("the accumulator");
    for t in truth.brightest() {
        let (e, u, v) = star(&acc, t.u, t.v);
        assert!((e / expect - 1.0).abs() < 1e-3, "{e} e⁻ vs {expect}");
        assert!((u - t.u).hypot(v - t.v) < 0.05, "({u}, {v}) vs {t:?}");
    }
}

/// Flat fields through each Poisson path: the sky table, Knuth's method
/// (every pixel slightly hot) and the rounded normal (a large dark mean).
/// DN mean and variance within 5 standard errors of `λ` and `λ + σ_read²`.
#[test]
fn flat_fields_have_poisson_and_read_noise() {
    let bytes = cat_bytes(&[]);
    let cat = StarCat::parse(&bytes).unwrap();
    let noisy = |dark_e_s| Sensor {
        dark_e_s,
        read_noise_e: 2.0,
        bias_dn: 100.0,
        full_well_e: 1e6,
        ..QUIET
    };
    let all_hot = HotPixels::new(SIZE * SIZE, 1.0, 1.0001, 3);
    let cases = [
        ("table", noisy(200.0), cold(), 20.0),
        ("knuth", noisy(200.0), all_hot, 20.0),
        ("normal", noisy(2000.0), cold(), 200.0),
    ];
    for (label, sensor, hot, lambda) in cases {
        let (truth, out) = render(&mut gpu(cat, sensor, hot), [0.0; 3], 9);
        assert_eq!(truth.n_rendered, 0);
        let (m, var) = moments(&out);
        let (expect_m, expect_var) = (lambda + 100.0, lambda + 4.0 + 1.0 / 12.0);
        let n = out.len() as f64;
        let (m_se, var_se) = ((expect_var / n).sqrt(), expect_var * (2.0 / n).sqrt());
        assert!((m - expect_m).abs() < 5.0 * m_se, "{label}: mean {m}");
        assert!((var - expect_var).abs() < 5.0 * var_se, "{label}: {var}");
    }
}

#[test]
fn deterministic_per_seed() {
    let cat = StarCat::parse(metor_star::render::testkit::STARCAT_G8).unwrap();
    let mut gpu = gpu(cat, SIM_SENSOR, HotPixels::new(SIZE * SIZE, 1e-4, 50.0, 1));
    let (t5, a) = render(&mut gpu, [0.0; 3], 5);
    let (_, b) = render(&mut gpu, [0.0; 3], 5);
    let (t6, c) = render(&mut gpu, [0.0; 3], 6);
    assert!(a == b && t5 == t6 && t5.n_rendered > 10);
    let same = a.iter().zip(&c).filter(|(x, y)| x == y).count();
    // Read noise of 1 DN rms leaves about a third of pixels equal by chance.
    assert!(same * 2 < a.len(), "{same} pixels equal across seeds");
}

/// Stars piled on one pixel saturate the accumulator at `u32::MAX`, and the
/// image at the full well and at the ADC ceiling.
#[test]
fn saturation() {
    let stars: Vec<_> = (0..8).map(|i| (at_pixel(512.5, 512.5), -8.0, i)).collect();
    let bytes = cat_bytes(&stars);
    let cat = StarCat::parse(&bytes).unwrap();
    let sharp = Sensor {
        psf_sigma_px: 0.2,
        ..QUIET
    };
    let mut r = gpu(cat, sharp, cold());
    let (_, out) = render(&mut r, [0.0; 3], 1);
    let at = 512 * SIZE + 512;
    assert_eq!(r.read_acc().expect("the accumulator")[at], u32::MAX);
    assert_eq!(out[at], 4095);
    let well = Sensor {
        full_well_e: 2e4,
        gain_milli: 10_000,
        bias_dn: 100.0,
        ..QUIET
    };
    let (_, out) = render(&mut gpu(cat, well, cold()), [0.0; 3], 1);
    assert_eq!(out[at], 2100);
}

/// A frame of more splats than the GPU starts with grows its buffers; a
/// short output is refused.
#[test]
fn splat_buffer_grows() {
    let stars: Vec<_> = (0..2500u32)
        .map(|i| {
            let (u, v) = (f64::from(i % 50), f64::from(i / 50));
            (at_pixel(100.0 + 16.0 * u, 100.0 + 16.0 * v), 7.0, NO_HIP)
        })
        .collect();
    let bytes = cat_bytes(&stars);
    let mut gpu = gpu(StarCat::parse(&bytes).unwrap(), QUIET, cold());
    let (truth, _) = render(&mut gpu, [0.0, 0.0, 20f64.to_radians()], 1);
    assert!(gpu.scene().splats().len() > 1 << 15);
    assert_eq!(truth.n_rendered, 2500);
    let err = gpu.render(&frame([0.0; 3], 1), &mut [0; 100]).err();
    assert_eq!(err, Some(GpuError::Render(RenderError::BadOutput)));
}
