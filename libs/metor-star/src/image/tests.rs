use std::vec;
use std::vec::Vec;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, Normal};

use super::label::{find, union};
use super::*;

const W: usize = 256;
const H: usize = 192;

/// A star at σ = 1 px: centre (px, pixel centres at +0.5) and total DN.
#[derive(Clone, Copy)]
struct Spot {
    u: f64,
    v: f64,
    dn: f64,
}

fn spot(u: f64, v: f64, dn: f64) -> Spot {
    Spot { u, v, dn }
}

/// The pixel-integrated Gaussian of `spots` on `bg` with Gaussian noise
/// `noise` (DN), clipped to 12 bits.
fn frame(
    w: usize,
    h: usize,
    spots: &[Spot],
    bg: impl Fn(usize, usize) -> f64,
    noise: f64,
    seed: u64,
) -> Vec<u16> {
    let mut rng = StdRng::seed_from_u64(seed);
    let normal = Normal::new(0.0, noise.max(1e-12)).unwrap();
    let cdf = |x: f64, s: f64| 0.5 * (1.0 + libm::erf(x / (s * core::f64::consts::SQRT_2)));
    let mut px = vec![0.0f64; w * h];
    for s in spots {
        let r = 6;
        let (ci, cj) = (s.u as i64, s.v as i64);
        for j in (cj - r)..=(cj + r) {
            for i in (ci - r)..=(ci + r) {
                if i < 0 || j < 0 || i >= w as i64 || j >= h as i64 {
                    continue;
                }
                let fx = cdf(i as f64 + 1.0 - s.u, 1.0) - cdf(i as f64 - s.u, 1.0);
                let fy = cdf(j as f64 + 1.0 - s.v, 1.0) - cdf(j as f64 - s.v, 1.0);
                px[j as usize * w + i as usize] += s.dn * fx * fy;
            }
        }
    }
    (0..w * h)
        .map(|k| {
            let v = px[k] + bg(k % w, k / w) + normal.sample(&mut rng);
            v.round().clamp(0.0, 4095.0) as u16
        })
        .collect()
}

fn flat(_: usize, _: usize) -> f64 {
    100.0
}

fn extractor(w: usize, h: usize) -> Extractor {
    Extractor::new(w as u32, h as u32, ImageConfig::default()).unwrap()
}

fn random_spots(rng: &mut StdRng, n: usize, w: usize, h: usize, dn: (f64, f64)) -> Vec<Spot> {
    let mut spots: Vec<Spot> = Vec::new();
    while spots.len() < n {
        let s = spot(
            rng.random_range(8.0..w as f64 - 8.0),
            rng.random_range(8.0..h as f64 - 8.0),
            rng.random_range(dn.0..dn.1),
        );
        // Keep stars apart so each is its own blob.
        if spots.iter().all(|o| (o.u - s.u).hypot(o.v - s.v) > 10.0) {
            spots.push(s);
        }
    }
    spots
}

/// Every star found, brightest first, to 0.1 px RMS in 5 DN noise; a
/// noiseless star on a pixel centre exactly.
#[test]
fn finds_every_bright_star_to_a_tenth_of_a_pixel() {
    let mut rng = StdRng::seed_from_u64(3);
    let mut ex = extractor(W, H);
    let mut errs = Vec::new();
    for seed in 0..20 {
        // Peak of a 300 DN star at σ 1 px is about 48 DN, ~10σ over 5 DN noise.
        let spots = random_spots(&mut rng, 15, W, H, (300.0, 3000.0));
        let found = ex.run(&frame(W, H, &spots, flat, 5.0, seed)).unwrap();
        assert_eq!(found.len(), spots.len(), "seed {seed}");
        assert!(found.windows(2).all(|w| w[0].flux >= w[1].flux));
        for s in &spots {
            let dist = |c: &Centroid| (c.u - s.u).hypot(c.v - s.v);
            errs.push(found.iter().map(dist).fold(f64::MAX, f64::min));
        }
    }
    let rms = (errs.iter().map(|e| e * e).sum::<f64>() / errs.len() as f64).sqrt();
    assert!(rms < 0.1, "CoG RMS {rms} px");
    let px = frame(W, H, &[spot(100.5, 60.5, 2000.0)], flat, 0.0, 0);
    let c = ex.run(&px).unwrap()[0];
    assert!(
        (c.u - 100.5).abs() < 1e-9 && (c.v - 60.5).abs() < 1e-9,
        "{c:?}"
    );
}

/// Of 40 stars the 20 kept track the 20 brightest.
#[test]
fn keeps_the_brightest() {
    let mut rng = StdRng::seed_from_u64(9);
    let spots = random_spots(&mut rng, 40, 512, 512, (300.0, 3000.0));
    let px = frame(512, 512, &spots, flat, 3.0, 1);
    let found: Vec<f64> = extractor(512, 512)
        .run(&px)
        .unwrap()
        .iter()
        .map(|c| c.flux)
        .collect();
    assert_eq!(found.len(), 20);
    let mut dn: Vec<f64> = spots.iter().map(|s| s.dn).collect();
    dn.sort_by(|a, b| b.total_cmp(a));
    for (f, d) in found.iter().zip(&dn) {
        assert!((f / d - 1.0).abs() < 0.2, "{f} vs {d}");
    }
}

#[test]
fn edge_cases_are_rejected_or_kept() {
    let mut ex = extractor(W, H);
    let mut run =
        |spots: &[Spot], seed| ex.run(&frame(W, H, spots, flat, 2.0, seed)).unwrap().len();
    // A star cut by the border is kept; two touching stars (one elongated
    // blob) and a saturated star are not.
    assert_eq!(run(&[spot(0.8, 50.0, 2000.0)], 2), 1);
    assert_eq!(run(&[spot(50.0, 50.0, 2e4), spot(57.0, 50.0, 2e4)], 3), 0);
    assert_eq!(run(&[spot(50.0, 50.0, 6e4)], 4), 0);
    // A hot pixel is under `min_area`; a uniform frame has no blobs.
    let mut hot = frame(W, H, &[], flat, 2.0, 5);
    hot[70 * W + 70] = 3000;
    assert!(ex.run(&hot).unwrap().is_empty());
    assert!(ex.run(&vec![500; W * H]).unwrap().is_empty());
    // A strong gradient is flattened by the block background.
    let ramp = |x: usize, y: usize| 100.0 + 3.0 * x as f64 + 2.0 * y as f64;
    let found = ex
        .run(&frame(W, H, &[spot(120.3, 90.7, 2000.0)], ramp, 3.0, 6))
        .unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert!((found[0].u - 120.3).hypot(found[0].v - 90.7) < 0.3);
}

#[test]
fn a_flooded_frame_is_rejected_whole() {
    let mut ex = extractor(W, H);
    // A lattice of lit pixels: each is its own run.
    let lit = |k: usize| (k % W).is_multiple_of(3) && (k / W).is_multiple_of(3);
    let px: Vec<u16> = (0..W * H).map(|k| if lit(k) { 4095 } else { 0 }).collect();
    let cfg = ImageConfig {
        max_runs: 1000,
        ..ImageConfig::default()
    };
    let mut small = Extractor::new(W as u32, H as u32, cfg).unwrap();
    assert_eq!(small.run(&px).err(), Some(ImageError::TooManyRuns));
    // All 4095: one block-median background, nothing lit.
    assert!(ex.run(&vec![4095; W * H]).unwrap().is_empty());
    assert_eq!(ex.run(&px[1..]).err(), Some(ImageError::Size));
    let zero = Extractor::new(0, 10, ImageConfig::default()).err();
    assert_eq!(zero, Some(ImageError::Config));
}

#[test]
fn union_find_keeps_parents_below() {
    let mut rng = StdRng::seed_from_u64(4);
    for _ in 0..200 {
        let n = rng.random_range(1..40);
        let mut parent: Vec<u32> = (0..n as u32).collect();
        for _ in 0..n {
            let (a, b) = (rng.random_range(0..n), rng.random_range(0..n));
            union(&mut parent, a, b);
            assert!(parent.iter().enumerate().all(|(i, p)| *p as usize <= i));
            assert_eq!(find(&mut parent, a), find(&mut parent, b));
        }
    }
}
