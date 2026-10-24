//! The GPU renderer against the CPU oracle in `metor_star::render`: the
//! noiseless accumulator per pixel, star centroids, and the noise in
//! distribution.

use metor_star::camera::{Camera, Intrinsics};
use metor_star::geom::{Quat, V3, cross, normalize, scale};
use metor_star::render::photometry::{G_TO_V, V0_PH_S_M2};
use metor_star::render::testkit::{NO_HIP, cat_bytes};
use metor_star::render::{
    Frame, HotPixels, Motion, Photometry, RenderError, Renderer, Sensor, Shape, Truth,
};
use metor_star::starcat::StarCat;
use startracker_sim_3::gpu::{GpuError, GpuRenderer};

static G8: &[u8] = include_bytes!("../../../../../libs/metor-star/data/starcat-g8.bin");
const SIZE: u32 = 1024;
const DEG: f64 = core::f64::consts::PI / 180.0;
/// Accumulator counts a GPU pixel may differ from the CPU's, plus
/// [`COUNT_REL_TOL`] of its value: past 2²⁴ counts an `f32` ulp of the
/// splat product is itself more than 1 count, and WGSL's `exp` is not
/// bit-identical to libm's.
const COUNT_TOL: f64 = 2.0;
const COUNT_REL_TOL: f64 = 2e-7;
const COG_TOL_PX: f64 = 1e-3;

fn camera() -> Camera {
    Camera {
        intr: Intrinsics {
            f_m: 16e-3,
            pitch_m: 5.5e-6,
            cx: 512.0,
            cy: 512.0,
            k1: 0.0,
            k2: 0.0,
            p1: 0.0,
            p2: 0.0,
            width: SIZE,
            height: SIZE,
        },
        q_b_st: Quat::IDENTITY,
    }
}

fn shape() -> Shape {
    Shape {
        width: SIZE,
        height: SIZE,
        bits: 12,
    }
}

fn phot() -> Photometry {
    Photometry {
        flux0_ph_s_m2: V0_PH_S_M2,
        aperture_m2: core::f32::consts::PI * 0.0125 * 0.0125,
        throughput: 0.5,
        colour: G_TO_V,
    }
}

/// Shot noise only, 1 e⁻/DN, no bias, a deep well.
fn quiet() -> Sensor {
    Sensor {
        psf_sigma_px: 1.0,
        full_well_e: 1e6,
        read_noise_e: 0.0,
        dark_e_s: 0.0,
        dark_t0_c: 20.0,
        temp_c: 20.0,
        gain_milli: 1000,
        bias_dn: 0.0,
        zodi_mag_arcsec2: None,
    }
}

/// The pack's default sensor.
fn noisy() -> Sensor {
    Sensor {
        full_well_e: 2e4,
        read_noise_e: 5.0,
        dark_e_s: 10.0,
        gain_milli: 5000,
        bias_dn: 100.0,
        zodi_mag_arcsec2: Some(22.5),
        ..quiet()
    }
}

fn motion(q_b_eci: Quat, omega_b: V3) -> Motion {
    Motion {
        q_b_eci,
        omega_b,
        beta: [0.0; 3],
        t_exp: 0.1,
    }
}

fn frame(motion: &Motion, seed: u64) -> Frame {
    Frame::new(&camera(), motion, 32, seed, 0).expect("a frame")
}

/// A spread of attitudes from a fixed sequence.
fn attitudes(n: usize) -> impl Iterator<Item = Quat> {
    (0..n).map(|i| {
        let t = i as f64;
        let q = [(t * 1.3).sin(), (t * 0.7).cos(), (t * 2.1).sin(), 0.5];
        Quat(q).normalized().expect("a rotation")
    })
}

fn scene<'a>(cat: StarCat<'a>, sensor: Sensor, hot: HotPixels, mag: f32) -> Renderer<'a> {
    Renderer::new(cat, camera(), phot(), sensor, shape(), hot, mag).expect("a renderer")
}

/// The CPU oracle and the GPU renderer of one scene.
fn pair<'a>(cat: StarCat<'a>, sensor: Sensor, hot: &HotPixels, mag: f32) -> Pair<'a> {
    let cpu = scene(cat, sensor, hot.clone(), mag);
    let gpu = GpuRenderer::new(scene(cat, sensor, hot.clone(), mag)).expect("a GPU");
    Pair { cpu, gpu }
}

struct Pair<'a> {
    cpu: Renderer<'a>,
    gpu: GpuRenderer<'a>,
}

/// One frame on both: `(truth, image)` from each.
struct Both {
    cpu: (Truth, Vec<u16>),
    gpu: (Truth, Vec<u16>),
}

impl Pair<'_> {
    fn render(&mut self, frame: &Frame) -> Both {
        let mut cpu = vec![0; shape().pixels()];
        let mut gpu = vec![0; shape().pixels()];
        let cpu_truth = self.cpu.render(frame, &mut cpu).expect("a CPU frame");
        let gpu_truth = self.gpu.render(frame, &mut gpu).expect("a GPU frame");
        Both {
            cpu: (cpu_truth, cpu),
            gpu: (gpu_truth, gpu),
        }
    }

    /// Asserts the accumulators agree per pixel and at every truth star.
    fn assert_noiseless(&self, truth: &Truth, label: &str) {
        let cpu = self.cpu.acc();
        let gpu = self.gpu.read_acc().expect("the accumulator");
        let bad = cpu.iter().zip(&gpu).position(|(&a, &b)| {
            let tol = COUNT_TOL + COUNT_REL_TOL * f64::from(a.max(b));
            f64::from(a.abs_diff(b)) > tol
        });
        if let Some(i) = bad {
            panic!(
                "{label}: pixel {i} is {} on the CPU, {} on the GPU",
                cpu[i], gpu[i]
            );
        }
        for s in truth.brightest() {
            let a = cog(cpu, s.u, s.v);
            let b = cog(&gpu, s.u, s.v);
            let err = (a.0 - b.0).hypot(a.1 - b.1);
            assert!(
                err < COG_TOL_PX,
                "{label}: star at ({}, {}) off by {err}",
                s.u,
                s.v
            );
        }
    }
}

/// The centre of gravity of `counts` within 5 px of `(u, v)`.
fn cog(counts: &[u32], u: f64, v: f64) -> (f64, f64) {
    let (mut s, mut su, mut sv) = (0.0, 0.0, 0.0);
    let (cu, cv) = (u.floor() as i64, v.floor() as i64);
    for y in (cv - 5).max(0)..(cv + 6).min(i64::from(SIZE)) {
        for x in (cu - 5).max(0)..(cu + 6).min(i64::from(SIZE)) {
            let c = f64::from(counts[(y * i64::from(SIZE) + x) as usize]);
            s += c;
            su += c * (x as f64 + 0.5);
            sv += c * (y as f64 + 0.5);
        }
    }
    (su / s, sv / s)
}

fn moments(x: &[u16]) -> (f64, f64) {
    let n = x.len() as f64;
    let m = x.iter().map(|&v| f64::from(v)).sum::<f64>() / n;
    let var = x.iter().map(|&v| (f64::from(v) - m).powi(2)).sum::<f64>() / (n - 1.0);
    (m, var)
}

/// Asserts the noisy images come from one distribution: mean and variance
/// within 5 standard errors.
fn assert_same_distribution(cpu: &[u16], gpu: &[u16], label: &str) {
    let ((m1, v1), (m2, v2)) = (moments(cpu), moments(gpu));
    let n = cpu.len() as f64;
    let mean_se = (v1 / n).sqrt() * 2f64.sqrt();
    let var_se = v1 * (4.0 / n).sqrt();
    assert!(
        (m1 - m2).abs() < 5.0 * mean_se,
        "{label}: mean {m1} vs {m2}"
    );
    assert!(
        (v1 - v2).abs() < 5.0 * var_se,
        "{label}: variance {v1} vs {v2}"
    );
}

/// Asserts at most 1e-3 of pixels differ, by at most 1 DN.
fn assert_nearly_identical(cpu: &[u16], gpu: &[u16], label: &str) {
    let diff: Vec<u16> = cpu.iter().zip(gpu).map(|(a, b)| a.abs_diff(*b)).collect();
    let n_diff = diff.iter().filter(|&&d| d > 0).count();
    let worst = diff.iter().copied().max().unwrap_or(0);
    assert!(
        n_diff * 1000 <= cpu.len(),
        "{label}: {n_diff} pixels differ"
    );
    assert!(worst <= 1, "{label}: by up to {worst} DN");
}

fn no_hot() -> HotPixels {
    HotPixels::none(shape().pixels())
}

// Nominal.

#[test]
fn noiseless_g8_matches_the_cpu() {
    let cat = StarCat::parse(G8).expect("the catalog");
    let mut r = pair(cat, quiet(), &no_hot(), 12.0);
    for (i, q) in attitudes(20).enumerate() {
        let f = frame(&motion(q, [0.0; 3]), 1);
        let both = r.render(&f);
        assert_eq!(both.cpu.0, both.gpu.0, "attitude {i}");
        assert!(both.gpu.0.n_rendered > 0, "attitude {i}");
        r.assert_noiseless(&both.cpu.0, &format!("attitude {i}"));
    }
}

/// The brightest star, Sirius, on the boresight: its peak pixel holds about
/// 3e7 counts, where one `f32` ulp is about 2.
#[test]
fn sirius_matches_the_cpu() {
    let cat = StarCat::parse(G8).expect("the catalog");
    let sirius = cat.all_stars()[25232].unit();
    let axis = normalize(cross([0.0, 0.0, 1.0], sirius)).expect("an axis");
    let half = 0.5 * sirius[2].acos();
    let q = Quat::from_parts(scale(axis, half.sin()), half.cos());
    let mut r = pair(cat, quiet(), &no_hot(), 12.0);
    let both = r.render(&frame(&motion(q, [0.0; 3]), 1));
    assert_eq!(both.gpu.0.stars[0].cat_id, 32349);
    assert_eq!(both.cpu.0, both.gpu.0);
    r.assert_noiseless(&both.cpu.0, "sirius");
}

#[test]
fn motion_blur_matches_the_cpu() {
    let cat = StarCat::parse(G8).expect("the catalog");
    let mut r = pair(cat, quiet(), &no_hot(), 12.0);
    for (i, q) in attitudes(5).enumerate() {
        let f = frame(&motion(q, [0.3 * DEG, -0.5 * DEG, 0.2 * DEG]), 1);
        assert!(f.substeps().len() > 1);
        let both = r.render(&f);
        assert_eq!(both.cpu.0, both.gpu.0, "attitude {i}");
        r.assert_noiseless(&both.cpu.0, &format!("blurred {i}"));
    }
}

#[test]
fn noisy_g8_matches_the_cpu() {
    let cat = StarCat::parse(G8).expect("the catalog");
    let hot = HotPixels::new(shape().pixels(), 1e-4, 50.0, 7);
    let mut r = pair(cat, noisy(), &hot, 12.0);
    for (i, q) in attitudes(3).enumerate() {
        let f = frame(&motion(q, [0.0; 3]), 100 + i as u64);
        let both = r.render(&f);
        assert_eq!(both.cpu.0, both.gpu.0);
        let label = format!("attitude {i}");
        assert_nearly_identical(&both.cpu.1, &both.gpu.1, &label);
        assert_same_distribution(&both.cpu.1, &both.gpu.1, &label);
    }
}

/// Flat fields through each Poisson path: the sky table, Knuth's method
/// (every pixel slightly hot), and the rounded normal (a large dark mean).
#[test]
fn flat_fields_match_the_cpu_in_distribution() {
    let bytes = cat_bytes(&[]);
    let cat = StarCat::parse(&bytes).expect("an empty catalog");
    let warm = Sensor {
        dark_e_s: 200.0,
        zodi_mag_arcsec2: None,
        ..noisy()
    };
    let hot = HotPixels::new(shape().pixels(), 1.0, 1.001, 3);
    let hotter = Sensor {
        dark_e_s: 2000.0,
        ..warm
    };
    let cases = [
        ("table", warm, no_hot()),
        ("knuth", warm, hot),
        ("normal", hotter, no_hot()),
    ];
    for (label, sensor, hot) in cases {
        let mut r = pair(cat, sensor, &hot, 12.0);
        let both = r.render(&frame(&motion(Quat::IDENTITY, [0.0; 3]), 9));
        assert_eq!(both.gpu.0.n_rendered, 0);
        assert_same_distribution(&both.cpu.1, &both.gpu.1, label);
        assert_nearly_identical(&both.cpu.1, &both.gpu.1, label);
    }
}

#[test]
fn deterministic_per_seed() {
    let cat = StarCat::parse(G8).expect("the catalog");
    let scene = scene(cat, noisy(), no_hot(), 12.0);
    let mut gpu = GpuRenderer::new(scene).expect("a GPU");
    let q = attitudes(1).next().expect("an attitude");
    let mut render = |seed| {
        let mut out = vec![0; shape().pixels()];
        gpu.render(&frame(&motion(q, [0.0; 3]), seed), &mut out)
            .expect("a frame");
        out
    };
    let (a, b, c) = (render(5), render(5), render(6));
    assert_eq!(a, b);
    let same = a.iter().zip(&c).filter(|(x, y)| x == y).count();
    // Read noise of 1 DN rms leaves about a third of pixels equal by chance.
    assert!(same * 2 < a.len(), "{same} pixels equal across seeds");
}

// Edge.

#[test]
fn empty_field_and_edge_stars() {
    let at = |u, v| camera().pixel_to_st(u, v).expect("a direction");
    let stars = [
        (at(0.3, 512.5), 5.0, 1),
        (at(1023.9, 1023.9), 5.0, 2),
        (at(-2.0, 300.0), 5.0, 3),
    ];
    let bytes = cat_bytes(&stars);
    let cat = StarCat::parse(&bytes).expect("a catalog");
    let mut r = pair(cat, quiet(), &no_hot(), 12.0);
    let f = frame(&motion(Quat::IDENTITY, [0.0; 3]), 1);
    let both = r.render(&f);
    assert_eq!(both.gpu.0.n_rendered, 3);
    assert_eq!(both.cpu.0, both.gpu.0);
    r.assert_noiseless(&Truth::empty(), "edge");
    let mut empty = pair(cat, quiet(), &no_hot(), -30.0);
    let both = empty.render(&f);
    assert_eq!(both.gpu.0.n_rendered, 0);
    assert!(both.gpu.1.iter().all(|&dn| dn == 0));
}

/// Stars piled on one pixel saturate it at `u32::MAX` on both.
#[test]
fn saturation() {
    let dir = camera().pixel_to_st(512.5, 512.5).expect("a direction");
    let stars: Vec<_> = (0..8).map(|i| (dir, -8.0, i)).collect();
    let bytes = cat_bytes(&stars);
    let cat = StarCat::parse(&bytes).expect("a catalog");
    let sharp = Sensor {
        psf_sigma_px: 0.2,
        ..quiet()
    };
    let mut r = pair(cat, sharp, &no_hot(), 12.0);
    let both = r.render(&frame(&motion(Quat::IDENTITY, [0.0; 3]), 1));
    assert_eq!(both.cpu.0, both.gpu.0);
    let at = 512 * SIZE as usize + 512;
    assert_eq!(r.cpu.acc()[at], u32::MAX);
    assert_eq!(r.gpu.read_acc().expect("the accumulator")[at], u32::MAX);
    r.assert_noiseless(&Truth::empty(), "saturated");
}

/// A frame of more splats than the GPU starts with grows its buffers.
#[test]
fn splat_buffer_grows() {
    let stars: Vec<_> = (0..2500u32)
        .map(|i| {
            let (u, v) = (
                100.0 + f64::from(i % 50) * 16.0,
                100.0 + f64::from(i / 50) * 16.0,
            );
            (
                camera().pixel_to_st(u, v).expect("a direction"),
                7.0,
                NO_HIP,
            )
        })
        .collect();
    let bytes = cat_bytes(&stars);
    let cat = StarCat::parse(&bytes).expect("a catalog");
    let mut r = pair(cat, quiet(), &no_hot(), 12.0);
    let f = frame(&motion(Quat::IDENTITY, [0.0, 0.0, 20.0 * DEG]), 1);
    let both = r.render(&f);
    assert!(r.gpu.scene().splats().len() > 1 << 15);
    assert_eq!(both.cpu.0, both.gpu.0);
    r.assert_noiseless(&Truth::empty(), "grown");
}

// Failure.

#[test]
fn rejects_a_short_output() {
    let cat = StarCat::parse(G8).expect("the catalog");
    let mut gpu = GpuRenderer::new(scene(cat, quiet(), no_hot(), 12.0)).expect("a GPU");
    let f = frame(&motion(Quat::IDENTITY, [0.0; 3]), 1);
    let mut short = vec![0; 100];
    let err = gpu.render(&f, &mut short).err();
    assert_eq!(err, Some(GpuError::Render(RenderError::BadOutput)));
}

/// Frame times on `starcat-g8.bin` at the pack's defaults.
#[test]
#[ignore = "timing: cargo test --release --test gpu -- --ignored --nocapture bench"]
fn bench_cpu_vs_gpu() {
    let cat = StarCat::parse(G8).expect("the catalog");
    let hot = HotPixels::new(shape().pixels(), 1e-4, 50.0, 1);
    let mut r = pair(cat, noisy(), &hot, 12.0);
    let q = Quat([0.1, 0.2, 0.3, 0.9]).normalized().expect("a rotation");
    let mut out = vec![0; shape().pixels()];
    for (label, omega) in [("still", [0.0; 3]), ("1°/s", [DEG, 0.0, 0.0])] {
        let f = frame(&motion(q, omega), 0);
        let n = 20;
        let mut time = |render: &mut dyn FnMut(&mut [u16])| {
            render(&mut out);
            let start = std::time::Instant::now();
            for _ in 0..n {
                render(&mut out);
            }
            start.elapsed() / n
        };
        let cpu = time(&mut |o| drop(r.cpu.render(&f, o)));
        let gpu = time(&mut |o| drop(r.gpu.render(&f, o)));
        let splats = r.gpu.scene().splats().len();
        println!("{label}: {splats} splats, CPU {cpu:?}, GPU {gpu:?} per frame");
    }
}
