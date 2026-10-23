use std::vec;
use std::vec::Vec;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use zerocopy::{FromZeros, IntoBytes};

use super::noise::{KNUTH_MAX, PoissonTable, TABLE_MAX, expose};
use super::photometry::{G_TO_V, V0_PH_S_M2};
use super::psf::{MAX_TAPS, erf_as};
use super::scene::MAX_SUB;
use super::splat::FIXED_SCALE;
use super::truth::MAX_TRUTH;
use super::*;
use crate::camera::Intrinsics;
use crate::geom::{Quat, cross, norm, normalize, scale, transpose};
use crate::starcat::{Cell, Header, HipEntry, MAGIC, Star as CatStar, VERSION};

static G8: &[u8] = include_bytes!("../../data/starcat-g8.bin");

const DEG: f64 = core::f64::consts::PI / 180.0;
const SIZE: u32 = 1024;
const NO_HIP: u32 = u32::MAX;

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
        width: SIZE,
        height: SIZE,
    }
}

fn camera() -> Camera {
    Camera {
        intr: intrinsics(),
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

/// D = 2 cm, τ·QE = 0.5, the default band.
fn phot() -> Photometry {
    Photometry {
        flux0_ph_s_m2: V0_PH_S_M2,
        aperture_m2: core::f32::consts::PI * 0.01 * 0.01,
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

/// 10 DN/e⁻, so read noise alone spreads each pixel over about 100 DN.
fn noisy() -> Sensor {
    Sensor {
        read_noise_e: 10.0,
        dark_e_s: 20.0,
        temp_c: 25.0,
        gain_milli: 100,
        bias_dn: 1000.0,
        zodi_mag_arcsec2: Some(21.0),
        full_well_e: 2e4,
        ..quiet()
    }
}

fn still(q_b_eci: Quat) -> Motion {
    Motion {
        q_b_eci,
        omega_b: [0.0; 3],
        beta: [0.0; 3],
        t_exp: 0.1,
    }
}

fn frame(motion: &Motion, seed: u64) -> Frame {
    Frame::new(&camera(), motion, MAX_SUB, seed, 0).unwrap()
}

/// A catalog of `(direction, G, HIP)` stars with BP − RP = 0, all in cell 0
/// of an nside-1 grid with a radius of π.
fn cat_bytes(stars: &[(V3, f32, u32)]) -> Vec<u8> {
    let mut stars = stars.to_vec();
    stars.sort_by(|a, b| a.1.total_cmp(&b.1));
    let n = stars.len();
    let hips: Vec<(u32, u32)> = (0u32..)
        .zip(&stars)
        .filter(|(_, s)| s.2 != NO_HIP)
        .map(|(row, s)| (row, s.2))
        .collect();
    let stars_off = Header::LEN + 12 * size_of::<Cell>();
    let mut header = Header::new_zeroed();
    header.magic = MAGIC;
    header.version.set(VERSION);
    header.nside.set(1);
    header.n_cells.set(12);
    header.n_stars.set(n as u32);
    header.n_hip.set(hips.len() as u32);
    header.cells_off.set(Header::LEN as u64);
    header.stars_off.set(stars_off as u64);
    header
        .hip_off
        .set((stars_off + n * size_of::<CatStar>()) as u64);
    let mut bytes = header.as_bytes().to_vec();
    for i in 0..12 {
        let mut cell = Cell::new_zeroed();
        cell.centre = [0.0f32, 0.0, 1.0].map(Into::into);
        let first = i == 0;
        cell.radius
            .set(if first { core::f32::consts::PI } else { 0.0 });
        cell.offset.set(if first { 0 } else { n as u32 });
        cell.count.set(if first { n as u32 } else { 0 });
        bytes.extend_from_slice(cell.as_bytes());
    }
    for (dir, g, _) in &stars {
        let mut star = CatStar::new_zeroed();
        star.dir = normalize(*dir).unwrap().map(|c| (c as f32).into());
        star.mags.set(u32::from((g * 1000.0).round() as i16 as u16));
        bytes.extend_from_slice(star.as_bytes());
    }
    for (row, hip) in hips {
        let mut e = HipEntry::new_zeroed();
        e.row.set(row);
        e.hip.set(hip);
        bytes.extend_from_slice(e.as_bytes());
    }
    bytes
}

/// The direction that lands on pixel `(u, v)` at an identity attitude.
fn at_pixel(u: f64, v: f64) -> V3 {
    camera().pixel_to_st(u, v).unwrap()
}

fn renderer<'a>(cat: StarCat<'a>, sensor: Sensor, mag_limit: f32) -> Renderer<'a> {
    let hot = HotPixels::none(shape().pixels());
    Renderer::new(cat, camera(), phot(), sensor, shape(), hot, mag_limit).unwrap()
}

/// Renders one still frame at an identity attitude.
fn render_one(stars: &[(V3, f32, u32)], sensor: Sensor) -> (Truth, Vec<u32>, Vec<u16>) {
    let bytes = cat_bytes(stars);
    let cat = StarCat::parse(&bytes).unwrap();
    let mut r = renderer(cat, sensor, 12.0);
    let mut out = vec![0; shape().pixels()];
    let truth = r
        .render(&frame(&still(Quat::IDENTITY), 1), &mut out)
        .unwrap();
    (truth, r.acc().to_vec(), out)
}

/// The centre of gravity of `counts` (row-major, `width` wide) in the box of
/// half-width `half` around `(u, v)`, with pixel centres at integer + ½.
fn cog(counts: &[u32], width: usize, u: f64, v: f64, half: i64) -> (f64, f64) {
    let height = (counts.len() / width) as i64;
    let (mut s, mut su, mut sv) = (0.0, 0.0, 0.0);
    for y in (v as i64 - half).max(0)..=(v as i64 + half).min(height - 1) {
        for x in (u as i64 - half).max(0)..=(u as i64 + half).min(width as i64 - 1) {
            let c = f64::from(counts[y as usize * width + x as usize]);
            s += c;
            su += c * (x as f64 + 0.5);
            sv += c * (y as f64 + 0.5);
        }
    }
    (su / s, sv / s)
}

fn total_e(counts: &[u32]) -> f64 {
    counts.iter().map(|&c| f64::from(c)).sum::<f64>() / f64::from(FIXED_SCALE)
}

/// A BP − RP = 0 star's electrons in 100 ms.
fn electrons(g: f32) -> f32 {
    phot().electrons(phot().band_mag(g, 0.0), 0.1)
}

fn moments(x: &[f64]) -> (f64, f64) {
    let n = x.len() as f64;
    let m = x.iter().sum::<f64>() / n;
    (
        m,
        x.iter().map(|v| (v - m) * (v - m)).sum::<f64>() / (n - 1.0),
    )
}

// Math.

#[test]
fn erf_matches_libm() {
    let worst = (-6000..=6000)
        .map(|i| i as f32 * 1e-3)
        .map(|x| (f64::from(erf_as(x)) - libm::erf(f64::from(x))).abs())
        .fold(0.0, f64::max);
    // A&S's 1.5e-7 plus about 3e-7 from evaluating `1 − poly·exp` in f32,
    // worst near |x| = 0.25.
    assert!(worst < 5e-7, "{worst}");
    assert_eq!(erf_as(0.0), 0.0);
    assert_eq!(erf_as(-2.0), -erf_as(2.0));
}

#[test]
fn psf_weights_sum_to_one() {
    let mut rng = StdRng::seed_from_u64(1);
    let mut w = [0.0; MAX_TAPS];
    for sigma in [0.3, 0.7, 1.0, 1.5] {
        let psf = Psf::new(sigma).unwrap();
        for _ in 0..1000 {
            let x0: f32 = rng.random_range(-10.0..1100.0);
            let first = psf.weights(x0, &mut w);
            let taps = &w[..psf.taps()];
            let sum: f32 = taps.iter().sum();
            assert!((sum - 1.0).abs() < 1e-6, "σ {sigma} x0 {x0}: {sum}");
            assert!(taps.iter().all(|&x| x >= 0.0));
            let end = (first + psf.taps() as i32) as f32;
            assert!(first as f32 <= x0 - 4.0 * sigma && x0 + 4.0 * sigma < end);
        }
    }
    assert_eq!(Psf::new(1.5).unwrap().taps(), MAX_TAPS);
}

#[test]
fn psf_rejects_bad_sigma() {
    for sigma in [0.0, -1.0, 1.51, f32::NAN, f32::INFINITY] {
        assert_eq!(Psf::new(sigma), None, "{sigma}");
    }
}

/// V = 0 ↔ 8.8e5 ph/s/cm², D = 2 cm, mag 7, τ·QE = 0.5 and 100 ms.
#[test]
fn photometry_anchor() {
    let e = phot().electrons(7.0, 0.1);
    assert!((e - 219.0).abs() < 1.0, "{e}");
    assert_eq!(phot().electrons(7.0, 0.0), 0.0);
    let ratio = phot().electrons(2.0, 1.0) / phot().electrons(7.0, 1.0);
    assert!((ratio - 100.0).abs() < 1e-3, "{ratio}");
}

#[test]
fn band_mag_colour_term() {
    let p = phot();
    assert_eq!(p.band_mag(5.0, 0.0), 5.0 + G_TO_V[0]);
    let c: f32 = 1.5;
    let expect = 5.0 + G_TO_V[0] + G_TO_V[1] * c + G_TO_V[2] * c * c + G_TO_V[3] * c * c * c;
    assert!((p.band_mag(5.0, c) - expect).abs() < 1e-5);
}

#[test]
fn hash_is_uniform_and_normal() {
    let n = 200_000u32;
    let u: Vec<f64> = (0..n).map(|p| f64::from(hash::uniform(p, 3, 77))).collect();
    assert!(u.iter().all(|&x| x > 0.0 && x < 1.0));
    let (m, var) = moments(&u);
    assert!(
        (m - 0.5).abs() < 0.004 && (var - 1.0 / 12.0).abs() < 0.002,
        "{m} {var}"
    );
    let z: Vec<f64> = (0..n).map(|p| f64::from(hash::normal(p, 3, 77))).collect();
    let (m, var) = moments(&z);
    assert!(m.abs() < 0.012 && (var - 1.0).abs() < 0.02, "{m} {var}");
    assert_ne!(hash::uniform(0, 0, 1), hash::uniform(0, 0, 1 << 32));
}

#[test]
fn erfinv_inverts_erf() {
    let worst = (1..200_000)
        .map(|i| i as f32 / 200_000.0)
        .flat_map(|x| [x, -x])
        .map(|x| (libm::erf(f64::from(hash::erfinv(x))) - f64::from(x)).abs())
        .fold(0.0, f64::max);
    assert!(worst < 1e-6, "{worst}");
    let n = 1 << 18;
    let wide = (0..n)
        .filter(|&p| hash::normal(p, 5, 3).abs() > 2.0)
        .count();
    let p = libm::erfc(2.0 / core::f64::consts::SQRT_2);
    let got = wide as f64 / f64::from(n);
    assert!(
        (got - p).abs() < 5.0 * (p * (1.0 - p) / f64::from(n)).sqrt(),
        "{got} vs {p}"
    );
}

// The splat.

/// The worst CoG error (px) over 400 sub-pixel phases.
fn worst_cog_error(sigma: f32) -> f64 {
    let psf = Psf::new(sigma).unwrap();
    let mut worst: f64 = 0.0;
    for i in 0..20 {
        for j in 0..20 {
            let (u, v) = (16.0 + i as f32 / 20.0, 16.0 + j as f32 / 20.0);
            let mut counts = vec![0; 32 * 32];
            let mut acc = Accumulator {
                counts: &mut counts,
                width: 32,
                height: 32,
            };
            acc.splat(&psf, u, v, 1e5);
            let (u, v) = (f64::from(u), f64::from(v));
            let (cu, cv) = cog(&counts, 32, u, v, 8);
            worst = worst.max((cu - u).abs()).max((cv - v).abs());
        }
    }
    worst
}

#[test]
fn splat_centroid_matches_position() {
    for sigma in [0.7, 1.0, 1.5] {
        let worst = worst_cog_error(sigma);
        assert!(worst < 0.02, "σ {sigma}: {worst} px");
    }
}

/// An undersampled PSF shows the pixel-phase S-curve.
#[test]
fn narrow_psf_shows_s_curve() {
    assert!(worst_cog_error(0.3) > 0.02);
}

#[test]
fn splat_off_the_sensor_adds_nothing() {
    let psf = Psf::new(1.0).unwrap();
    let mut counts = vec![0; 16 * 16];
    let mut acc = Accumulator {
        counts: &mut counts,
        width: 16,
        height: 16,
    };
    assert!(!acc.splat(&psf, -6.0, 8.0, 1e5));
    assert!(!acc.splat(&psf, 8.0, 22.0, 1e5));
    assert!(!acc.splat(&psf, 8.0, 8.0, 0.0));
    assert!(counts.iter().all(|&c| c == 0));
}

// The renderer.

#[test]
fn rendered_centroid_matches_truth() {
    for (u, v) in [(300.3, 700.8), (512.0, 512.0), (20.71, 1000.05)] {
        let (truth, acc, _) = render_one(&[(at_pixel(u, v), 6.0, NO_HIP)], quiet());
        assert_eq!(truth.count, 1);
        let t = truth.stars[0];
        assert!((t.u - u).abs() < 1e-3 && (t.v - v).abs() < 1e-3, "{t:?}");
        let (cu, cv) = cog(&acc, SIZE as usize, t.u, t.v, 8);
        assert!(
            (cu - t.u).abs() < 0.02 && (cv - t.v).abs() < 0.02,
            "{cu} {cv} vs {t:?}"
        );
    }
}

#[test]
fn total_electrons_match_photometry() {
    for g in [2.0, 5.0, 8.0] {
        let (_, acc, _) = render_one(&[(at_pixel(400.4, 600.6), g, NO_HIP)], quiet());
        let rel = (total_e(&acc) / f64::from(electrons(g)) - 1.0).abs();
        assert!(rel < 1e-4, "G {g}: {rel}");
    }
}

#[test]
fn truth_maps_back_to_the_catalog() {
    let q = Quat([0.2, -0.4, 0.1, 0.9]).normalized().unwrap();
    let f = frame(&still(q), 0);
    let rt = transpose(f.r_st_eci());
    let stars: Vec<_> = [(100.5, 900.25), (511.0, 3.0), (1000.9, 1000.1)]
        .iter()
        .map(|&(u, v)| (mat_vec(rt, at_pixel(u, v)), 4.0, NO_HIP))
        .collect();
    let bytes = cat_bytes(&stars);
    let cat = StarCat::parse(&bytes).unwrap();
    let mut r = renderer(cat, quiet(), 12.0);
    let mut out = vec![0; shape().pixels()];
    let truth = r.render(&f, &mut out).unwrap();
    assert_eq!(truth.count, 3);
    for t in truth.brightest() {
        let eci = mat_vec(rt, camera().pixel_to_st(t.u, t.v).unwrap());
        let sep = cat.all_stars().iter().map(|s| norm(cross(s.unit(), eci)));
        let sep = sep.fold(f64::MAX, f64::min);
        assert!(sep < 1e-9, "{sep}");
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
    let (truth, _, _) = render_one(&stars, quiet());
    assert_eq!(truth.count, MAX_TRUTH);
    assert_eq!((truth.n_rendered, truth.n_detectable), (40, 40));
    for (i, t) in (0u32..).zip(truth.brightest()) {
        assert_eq!(t.cat_id, hip(i), "entry {i}");
        assert!(
            (t.mag - (g(i) + G_TO_V[0])).abs() < 1e-3,
            "entry {i}: {}",
            t.mag
        );
    }
}

#[test]
fn faint_stars_render_but_are_not_detectable() {
    let stars = [
        (at_pixel(200.0, 200.0), 5.0, NO_HIP),
        (at_pixel(800.0, 800.0), 11.9, NO_HIP),
    ];
    let (truth, _, _) = render_one(&stars, noisy());
    assert_eq!(
        (truth.count, truth.n_rendered, truth.n_detectable),
        (2, 2, 1)
    );
}

#[test]
fn empty_field() {
    let (truth, acc, out) = render_one(&[], quiet());
    assert_eq!(truth, Truth::empty());
    assert!(acc.iter().all(|&c| c == 0));
    assert!(out.iter().all(|&dn| dn == 0));
    // A star exactly at the magnitude limit is skipped.
    let bytes = cat_bytes(&[(at_pixel(512.0, 512.0), 3.0, NO_HIP)]);
    let mut r = renderer(StarCat::parse(&bytes).unwrap(), quiet(), 3.0);
    let mut out = vec![0; shape().pixels()];
    let truth = r
        .render(&frame(&still(Quat::IDENTITY), 1), &mut out)
        .unwrap();
    assert_eq!(truth, Truth::empty());
}

/// A star centred 0.25 px off the left edge puts `P(x ≥ 0)` of its light on
/// the sensor. It is rendered but not listed.
#[test]
fn star_on_the_edge() {
    let (truth, acc, _) = render_one(&[(at_pixel(-0.25, 500.5), 4.0, NO_HIP)], quiet());
    assert_eq!((truth.count, truth.n_rendered), (0, 1));
    let expect = 0.5 * (1.0 - libm::erf(0.25 / core::f64::consts::SQRT_2));
    let share = total_e(&acc) / f64::from(electrons(4.0));
    assert!((share - expect).abs() < 1e-3, "{share} vs {expect}");
    let (truth, acc, _) = render_one(&[(at_pixel(1023.9, 1023.9), 4.0, NO_HIP)], quiet());
    assert_eq!((truth.count, truth.n_rendered), (1, 1));
    assert!(total_e(&acc) > 0.0);
    let (truth, _, _) = render_one(&[(at_pixel(-30.0, 500.0), 4.0, NO_HIP)], quiet());
    assert_eq!((truth.count, truth.n_rendered), (0, 0));
}

#[test]
fn saturation() {
    let star = [(at_pixel(512.5, 512.5), -1.0, NO_HIP)];
    let centre = 512 * SIZE as usize + 512;
    let well = Sensor {
        full_well_e: 2e4,
        gain_milli: 10_000,
        bias_dn: 100.0,
        ..quiet()
    };
    let (_, _, out) = render_one(&star, well);
    assert_eq!(out[centre], 2100);
    assert_eq!(out.iter().max(), Some(&2100));
    let adc = Sensor {
        bias_dn: 100.0,
        ..quiet()
    };
    let (_, _, out) = render_one(&star, adc);
    assert_eq!(out[centre], 4095);
    let deep = Shape {
        bits: 16,
        ..shape()
    };
    assert_eq!(deep.max_dn(), u16::MAX);
}

/// A star brighter than `MAX_STAR_E` is clamped to it, and a pixel near
/// `u32::MAX` saturates there.
#[test]
fn accumulator_saturates() {
    let (_, acc, _) = render_one(&[(at_pixel(512.5, 512.5), -8.0, NO_HIP)], quiet());
    assert!(electrons(-8.0) > 10.0 * MAX_STAR_E);
    let rel = (total_e(&acc) / f64::from(MAX_STAR_E) - 1.0).abs();
    assert!(rel < 1e-4, "{rel}");
    let psf = Psf::new(1.0).unwrap();
    let mut counts = vec![u32::MAX - 10; 16 * 16];
    let mut acc = Accumulator {
        counts: &mut counts,
        width: 16,
        height: 16,
    };
    assert!(acc.splat(&psf, 8.0, 8.0, MAX_STAR_E));
    assert_eq!(counts[8 * 16 + 8], u32::MAX);
}

#[test]
fn zero_exposure_is_bias_only() {
    let bytes = cat_bytes(&[(at_pixel(512.0, 512.0), 0.0, NO_HIP)]);
    let sensor = Sensor {
        bias_dn: 50.0,
        ..quiet()
    };
    let mut r = renderer(StarCat::parse(&bytes).unwrap(), sensor, 12.0);
    let mut out = vec![0; shape().pixels()];
    let motion = Motion {
        t_exp: 0.0,
        ..still(Quat::IDENTITY)
    };
    let truth = r.render(&frame(&motion, 1), &mut out).unwrap();
    assert_eq!(
        (truth.count, truth.n_rendered, truth.n_detectable),
        (1, 0, 0)
    );
    assert!(out.iter().all(|&dn| dn == 50));
}

#[test]
fn motion_blur_streak_centres_on_mid_exposure() {
    // 5°/s about body x streaks a star about 25 px in 100 ms.
    let motion = Motion {
        omega_b: [5.0 * DEG, 0.0, 0.0],
        ..still(Quat::IDENTITY)
    };
    let f = frame(&motion, 1);
    assert_eq!(f.substeps().len(), MAX_SUB);
    let dir = mat_vec(transpose(f.r_st_eci()), at_pixel(500.3, 400.7));
    let bytes = cat_bytes(&[(dir, 3.0, NO_HIP)]);
    let mut r = renderer(StarCat::parse(&bytes).unwrap(), quiet(), 12.0);
    let mut out = vec![0; shape().pixels()];
    let truth = r.render(&f, &mut out).unwrap();
    let t = truth.stars[0];
    assert!(
        (t.u - 500.3).abs() < 1e-3 && (t.v - 400.7).abs() < 1e-3,
        "{t:?}"
    );
    let (cu, cv) = cog(r.acc(), SIZE as usize, t.u, t.v, 20);
    assert!(
        (cu - t.u).abs() < 0.02 && (cv - t.v).abs() < 0.02,
        "{cu} {cv}"
    );
    let rel = (total_e(r.acc()) / f64::from(electrons(3.0)) - 1.0).abs();
    assert!(rel < 1e-4, "{rel}");
    let slow = Motion {
        omega_b: [1e-4 * DEG, 0.0, 0.0],
        ..motion
    };
    assert_eq!(frame(&slow, 1).substeps().len(), 1);
}

/// A 25 px streak spreads a star's peak by `σ√(2π)/25`, so a star that is
/// detectable when still is not when streaked.
#[test]
fn streaks_lower_detectability() {
    let render = |omega_b| {
        let f = frame(
            &Motion {
                omega_b,
                ..still(Quat::IDENTITY)
            },
            1,
        );
        let dir = mat_vec(transpose(f.r_st_eci()), at_pixel(500.5, 400.5));
        let bytes = cat_bytes(&[(dir, 5.0, NO_HIP)]);
        let mut r = renderer(StarCat::parse(&bytes).unwrap(), noisy(), 12.0);
        let mut out = vec![0; shape().pixels()];
        r.render(&f, &mut out).unwrap()
    };
    let still_truth = render([0.0; 3]);
    let streaked = render([5.0 * DEG, 0.0, 0.0]);
    assert_eq!((still_truth.count, still_truth.n_detectable), (1, 1));
    assert_eq!((streaked.count, streaked.n_detectable), (1, 0));
}

/// End to end, as the ideal tracker sees it: a star placed through the body
/// (`b_body = A(q_b_eci) r`) and the mount (`b_body = A(q_b_st) b_st`) is
/// rendered, centroided, and mapped back through `pixel_to_st` and
/// `A(q_b_st)` onto the same body vector.
#[test]
fn round_trip_through_body_and_mount() {
    let cases = [
        ([0.3, 0.1, -0.2, 0.9], [-0.1, 0.5, 0.2, 0.8], (300.3, 700.8)),
        ([0.0, 1.0, 0.0, 1.0], [0.2, -0.4, 0.1, 0.9], (100.5, 900.25)),
        (
            [-0.5, 0.5, 0.5, 0.5],
            [0.05, 0.02, -0.9, 0.4],
            (1000.9, 20.1),
        ),
    ];
    // 0.01 px.
    let tol = 0.01 * 5.5e-6 / 16e-3;
    for (q_b_st, q_b_eci, (u, v)) in cases {
        let q_b_st = Quat(q_b_st).normalized().unwrap();
        let q_b_eci = Quat(q_b_eci).normalized().unwrap();
        let cam = Camera { q_b_st, ..camera() };
        let b_body = q_b_st.rotate(cam.pixel_to_st(u, v).unwrap());
        let r_eci = q_b_eci.conj().rotate(b_body);
        let bytes = cat_bytes(&[(r_eci, 3.0, NO_HIP)]);
        let cat = StarCat::parse(&bytes).unwrap();
        let hot = HotPixels::none(shape().pixels());
        let mut r = Renderer::new(cat, cam, phot(), quiet(), shape(), hot, 12.0).unwrap();
        let f = Frame::new(&cam, &still(q_b_eci), MAX_SUB, 1, 0).unwrap();
        let mut out = vec![0; shape().pixels()];
        let truth = r.render(&f, &mut out).unwrap();
        assert_eq!(truth.count, 1);
        let t = truth.stars[0];
        let (cu, cv) = cog(r.acc(), SIZE as usize, t.u, t.v, 8);
        let seen = q_b_st.rotate(cam.pixel_to_st(cu, cv).unwrap());
        let err = norm(cross(seen, b_body));
        assert!(err < tol, "pixel ({u}, {v}): {err} rad");
    }
}

/// `r_st_eci = A(q_b_st)ᵀ A(q_b_eci)`, which is `A(q_b_eci ⊗ q_b_st⁻¹)`.
#[test]
fn frame_attitude_composition() {
    let q_b_st = Quat([0.3, 0.1, -0.2, 0.9]).normalized().unwrap();
    let q_b_eci = Quat([-0.1, 0.5, 0.2, 0.8]).normalized().unwrap();
    let cam = Camera { q_b_st, ..camera() };
    let f = Frame::new(&cam, &still(q_b_eci), 1, 0, 0).unwrap();
    assert!(f.q_cam_eci().angle_to(q_b_eci * q_b_st.conj()) < 1e-12);
    assert!(f.q_cam_eci().angle_to(q_b_st.conj() * q_b_eci) > 1e-3);
    let r = [0.6, 0.0, 0.8];
    let direct = cam.body_to_st(q_b_eci.rotate(r));
    let ours = mat_vec(f.r_st_eci(), r);
    assert!((0..3).all(|i| (direct[i] - ours[i]).abs() < 1e-15));
}

#[test]
fn deterministic_per_seed() {
    let cat = StarCat::parse(G8).unwrap();
    let hot = HotPixels::new(shape().pixels(), 1e-3, 50.0, 9);
    let mut r = Renderer::new(cat, camera(), phot(), noisy(), shape(), hot, 8.0).unwrap();
    let q = Quat([0.0, 0.3, 0.0, 1.0]).normalized().unwrap();
    let mut render = |seed| {
        let mut out = vec![0; shape().pixels()];
        let truth = r.render(&frame(&still(q), seed), &mut out).unwrap();
        (truth, out)
    };
    let (t1, a) = render(5);
    let (t2, b) = render(5);
    let (t3, c) = render(6);
    assert!(a == b && t1 == t2 && t1 == t3);
    assert!(t1.n_rendered > 100, "{}", t1.n_rendered);
    let same = a.iter().zip(&c).filter(|(x, y)| x == y).count();
    assert!(same < a.len() / 100, "{same} pixels equal");
}

#[test]
fn renders_sirius_from_g8() {
    let cat = StarCat::parse(G8).unwrap();
    let sirius = cat.all_stars()[25232].unit();
    // The rotation taking z to Sirius, as body←ECI, puts Sirius on the
    // boresight.
    let axis = normalize(cross([0.0, 0.0, 1.0], sirius)).unwrap();
    let half = 0.5 * libm::acos(sirius[2]);
    let q = Quat::from_parts(scale(axis, libm::sin(half)), libm::cos(half));
    let f = frame(&still(q), 0);
    assert!(norm(cross(f.boresight_eci(), sirius)) < 1e-12);
    let hot = HotPixels::none(shape().pixels());
    let mut r = Renderer::new(cat, camera(), phot(), quiet(), shape(), hot, 8.0).unwrap();
    let mut out = vec![0; shape().pixels()];
    let truth = r.render(&f, &mut out).unwrap();
    let t = truth.stars[0];
    assert_eq!(t.cat_id, 32349);
    assert!(
        (t.u - 512.0).abs() < 1e-6 && (t.v - 512.0).abs() < 1e-6,
        "{t:?}"
    );
    assert!(truth.count > 5 && truth.n_rendered as usize >= truth.count);
    let (cu, cv) = cog(r.acc(), SIZE as usize, t.u, t.v, 6);
    assert!(
        (cu - t.u).abs() < 0.02 && (cv - t.v).abs() < 0.02,
        "{cu} {cv}"
    );
}

// Noise.

/// Where a flat field's mean comes from, which picks the Poisson path.
#[derive(Clone, Copy, Debug)]
enum Source {
    /// Sky with no stars: the per-frame table up to `TABLE_MAX`.
    Sky,
    /// Star counts: Knuth below `KNUTH_MAX`, else a rounded normal.
    Star,
}

fn exposure(read: f32, bias: f32) -> Exposure {
    Exposure {
        background_e: 0.0,
        dark_e: 0.0,
        read_noise_e: read,
        full_well_e: 1e6,
        gain_milli: 1000,
        bias_dn: bias,
        max_dn: u16::MAX,
        seed: 42,
    }
}

fn flat_with(lambda: f32, source: Source, exp: Exposure, n: usize) -> Vec<f64> {
    let (counts, exp) = match source {
        Source::Sky => (
            0,
            Exposure {
                background_e: lambda,
                ..exp
            },
        ),
        Source::Star => ((lambda * FIXED_SCALE) as u32, exp),
    };
    let (acc, hot) = (vec![counts; n], vec![1.0; n]);
    let mut out = vec![0; n];
    expose(&acc, &hot, &exp, &mut out);
    out.iter().map(|&d| f64::from(d)).collect()
}

fn flat(lambda: f32, source: Source) -> Vec<f64> {
    flat_with(lambda, source, exposure(0.0, 0.0), 1 << 17)
}

/// The exact Poisson CDF at `k`.
fn poisson_cdf(lambda: f64, k: f64) -> f64 {
    let mut pmf = (-lambda).exp();
    let mut sum = 0.0;
    for i in 0..=(k as i64) {
        sum += pmf;
        pmf *= lambda / (i + 1) as f64;
    }
    if k < 0.0 { 0.0 } else { sum }
}

#[test]
fn flat_field_is_poisson() {
    let cases = [
        (Source::Sky, &[0.5f32, 5.0, 29.0, 31.0, 99.0, 150.0][..]),
        (
            Source::Star,
            &[0.5, 5.0, KNUTH_MAX - 1.0, KNUTH_MAX + 1.0, 1000.0][..],
        ),
    ];
    for (source, lambdas) in cases {
        for &lambda in lambdas {
            let x = flat(lambda, source);
            let (m, var) = moments(&x);
            let (l, n) = (f64::from(lambda), x.len() as f64);
            // The sample variance of a Poisson variable has variance
            // (2λ² + λ)/n; rounding a normal draw adds 1/12.
            let m_tol = 5.0 * (l / n).sqrt();
            let v_tol = 5.0 * ((2.0 * l * l + l) / n).sqrt() + 0.1;
            assert!((m - l).abs() < m_tol, "{source:?} λ {lambda}: mean {m}");
            assert!((var - l).abs() < v_tol, "{source:?} λ {lambda}: var {var}");
        }
    }
}

/// The table and Knuth paths are exact Poisson out to ±2.5σ, where a rounded
/// normal at λ = 30 is several times off.
#[test]
fn poisson_tails() {
    let cases = [
        (Source::Sky, 0.5),
        (Source::Sky, 31.0),
        (Source::Sky, TABLE_MAX - 1.0),
        (Source::Star, 29.0),
    ];
    for (source, lambda) in cases {
        let x = flat(lambda, source);
        let (l, n) = (f64::from(lambda), x.len() as f64);
        let lo = (l - 2.5 * l.sqrt()).floor();
        let hi = (l + 2.5 * l.sqrt()).ceil();
        let tails = [
            (x.iter().filter(|&&k| k <= lo).count(), poisson_cdf(l, lo)),
            (
                x.iter().filter(|&&k| k >= hi).count(),
                1.0 - poisson_cdf(l, hi - 1.0),
            ),
        ];
        for (count, p) in tails {
            let tol = 5.0 * (p * (1.0 - p) / n).sqrt() + 1.0 / n;
            let got = count as f64 / n;
            assert!((got - p).abs() < tol, "{source:?} λ {lambda}: {got} vs {p}");
        }
    }
}

#[test]
fn poisson_table_edges() {
    assert!(PoissonTable::new(-0.1).is_none());
    assert!(PoissonTable::new(TABLE_MAX + 0.1).is_none());
    assert!(PoissonTable::new(f32::NAN).is_none());
    let zero = PoissonTable::new(0.0).unwrap();
    assert_eq!(zero.sample(0.999_999), 0.0);
}

#[test]
fn read_noise_and_bias() {
    let x = flat_with(0.0, Source::Sky, exposure(3.0, 100.0), 1 << 17);
    let (m, var) = moments(&x);
    assert!((m - 100.0).abs() < 0.05, "{m}");
    assert!((var - (9.0 + 1.0 / 12.0)).abs() < 0.25, "{var}");
    // Below the bias, negative read noise clamps at DN 0.
    let low = flat_with(0.0, Source::Sky, exposure(3.0, 0.0), 1 << 12);
    assert!(low.iter().all(|&d| d >= 0.0) && low.contains(&0.0));
}

/// Read noise is added after the full-well clamp, so a saturated pixel still
/// shows it.
#[test]
fn read_noise_after_full_well() {
    let exp = Exposure {
        full_well_e: 10.0,
        ..exposure(3.0, 100.0)
    };
    let (m, var) = moments(&flat_with(50.0, Source::Sky, exp, 1 << 16));
    assert!((m - 110.0).abs() < 0.1, "{m}");
    assert!((var - 9.0).abs() < 0.5, "{var}");
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
    assert!(HotPixels::none(4).as_slice().iter().all(|&m| m == 1.0));
}

// Failures.

#[test]
fn renderer_rejects_bad_setup() {
    let bytes = cat_bytes(&[]);
    let cat = StarCat::parse(&bytes).unwrap();
    let hot = || HotPixels::none(shape().pixels());
    let new = |sensor, shape, phot, hot| {
        Renderer::new(cat, camera(), phot, sensor, shape, hot, 12.0).err()
    };
    for sigma in [0.0, -1.0, 1.6] {
        let s = Sensor {
            psf_sigma_px: sigma,
            ..quiet()
        };
        assert_eq!(new(s, shape(), phot(), hot()), Some(RenderError::BadPsf));
    }
    let s = Sensor {
        gain_milli: 0,
        ..quiet()
    };
    assert_eq!(new(s, shape(), phot(), hot()), Some(RenderError::BadGain));
    for s in [
        Sensor {
            read_noise_e: f32::NAN,
            ..quiet()
        },
        Sensor {
            full_well_e: -1.0,
            ..quiet()
        },
    ] {
        assert_eq!(new(s, shape(), phot(), hot()), Some(RenderError::BadSensor));
    }
    for bad in [
        Shape {
            width: 512,
            ..shape()
        },
        Shape {
            height: 2048,
            ..shape()
        },
        Shape { bits: 0, ..shape() },
        Shape {
            bits: 17,
            ..shape()
        },
    ] {
        assert_eq!(
            new(quiet(), bad, phot(), hot()),
            Some(RenderError::BadShape)
        );
    }
    let p = Photometry {
        aperture_m2: 0.0,
        ..phot()
    };
    assert_eq!(
        new(quiet(), shape(), p, hot()),
        Some(RenderError::BadPhotometry)
    );
    let few = HotPixels::none(5);
    assert_eq!(
        new(quiet(), shape(), phot(), few),
        Some(RenderError::BadHotPixels)
    );
    let mut r = renderer(cat, quiet(), 12.0);
    let mut short = vec![0; 10];
    let f = frame(&still(Quat::IDENTITY), 0);
    assert_eq!(r.render(&f, &mut short), Err(RenderError::BadOutput));
}

#[test]
fn frame_rejects_bad_motion() {
    let ok = still(Quat::IDENTITY);
    let new = |m: &Motion, max_sub| Frame::new(&camera(), m, max_sub, 0, 0).err();
    assert_eq!(new(&ok, 0), Some(RenderError::BadSubsteps));
    assert_eq!(new(&ok, MAX_SUB + 1), Some(RenderError::BadSubsteps));
    for bad in [
        Motion {
            q_b_eci: Quat([0.0; 4]),
            ..ok
        },
        Motion { t_exp: -0.1, ..ok },
        Motion {
            t_exp: f64::NAN,
            ..ok
        },
        Motion {
            omega_b: [f64::INFINITY, 0.0, 0.0],
            ..ok
        },
        Motion {
            beta: [0.0, f64::NAN, 0.0],
            ..ok
        },
    ] {
        assert_eq!(new(&bad, MAX_SUB), Some(RenderError::BadMotion), "{bad:?}");
    }
}

/// Full-frame renders of `starcat-g8.bin` at a 20° field with noise.
#[test]
#[ignore = "timing: cargo test --release --features sim -- --ignored --nocapture bench"]
fn bench_render_g8() {
    let cat = StarCat::parse(G8).unwrap();
    let hot = HotPixels::new(shape().pixels(), 1e-4, 50.0, 1);
    let mut r = Renderer::new(cat, camera(), phot(), noisy(), shape(), hot, 8.0).unwrap();
    let mut out = vec![0; shape().pixels()];
    let q = Quat([0.1, 0.2, 0.3, 0.9]).normalized().unwrap();
    let f = frame(&still(q), 0);
    let n = 20;
    let start = std::time::Instant::now();
    let mut rendered = 0;
    for _ in 0..n {
        rendered = r.render(&f, &mut out).unwrap().n_rendered;
    }
    std::println!("{:?} per frame, {rendered} stars", start.elapsed() / n);
}
