//! The flight pipeline on the simulator's frames: render a 1024², 20° frame
//! of `starcat-g8.bin` at a random attitude with the startracker-sim pack's
//! default camera and sensor, extract centroids, identify them against the
//! committed `stars-u2000.stdb` with Pyramid, and score each identified star
//! against the renderer's truth (HIP numbers).
//!
//! `cargo test --release -p metor-star --features sim --test lis_sim --
//! --nocapture` prints the rates and timings; `LIS_TRIALS` sets the trials.

use std::time::{Duration, Instant};

use metor_star::camera::{Camera, Intrinsics};
use metor_star::catalog::Db;
use metor_star::geom::{Quat, V3};
use metor_star::image::{Extractor, ImageConfig};
use metor_star::lis::{LisConfig, NO_ROW, Pyramid, Step};
use metor_star::render::photometry::{G_TO_V, V0_PH_S_M2};
use metor_star::render::{Frame, HotPixels, Motion, Photometry, Renderer, Sensor, Shape, Truth};
use metor_star::starcat::StarCat;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, StandardNormal};

static STARCAT: &[u8] = include_bytes!("../data/starcat-g8.bin");
static DB: &[u8] = include_bytes!("../data/stars-u2000.stdb");
const SIZE: u32 = 1024;

/// `StarCamParams::default()` in startracker-sim.
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

/// startracker-sim's default optics, sensor and hot pixels.
fn renderer(cat: StarCat<'_>) -> Renderer<'_> {
    let phot = Photometry {
        flux0_ph_s_m2: V0_PH_S_M2,
        aperture_m2: core::f32::consts::PI * 0.0125 * 0.0125,
        throughput: 0.5,
        colour: G_TO_V,
    };
    let sensor = Sensor {
        psf_sigma_px: 1.0,
        full_well_e: 20_000.0,
        read_noise_e: 5.0,
        dark_e_s: 10.0,
        dark_t0_c: 20.0,
        temp_c: 20.0,
        gain_milli: 5_000,
        bias_dn: 100.0,
        zodi_mag_arcsec2: Some(22.5),
    };
    let shape = Shape {
        width: SIZE,
        height: SIZE,
        bits: 12,
    };
    let hot = HotPixels::new(shape.pixels(), 1e-4, 50.0, 13);
    Renderer::new(cat, camera(), phot, sensor, shape, hot, 12.0).unwrap()
}

fn random_q(rng: &mut StdRng) -> Quat {
    let v: [f64; 4] = core::array::from_fn(|_| StandardNormal.sample(rng));
    Quat(v).normalized().unwrap()
}

/// The HIP numbers of the truth stars within 1 px of `(u, v)`: a blended
/// double is one blob, and either component is a right answer.
fn truth_hips(truth: &Truth, u: f64, v: f64) -> Vec<u32> {
    truth
        .brightest()
        .iter()
        .filter(|t| (t.u - u).hypot(t.v - v) < 1.0 && t.cat_id != u32::MAX)
        .map(|t| t.cat_id)
        .collect()
}

#[derive(Default)]
struct Score {
    trials: usize,
    correct: usize,
    false_id: usize,
    no_solve: usize,
    /// Identified stars checked against truth, and the wrong ones.
    checked: usize,
    wrong: usize,
    times: Vec<Duration>,
    triplets: Vec<u32>,
}

impl Score {
    fn report(&mut self, label: &str) {
        let n = self.trials as f64;
        self.times.sort();
        self.triplets.sort();
        let at = |p: f64| ((n - 1.0) * p) as usize;
        println!(
            "{label}: {} trials, {:.2}% correct, {:.2}% false ID, {:.2}% no solve, \
             {}/{} stars wrong; pyramid median {:?} p99 {:?} max {:?}, \
             triplets median {} p99 {} max {}",
            self.trials,
            100.0 * self.correct as f64 / n,
            100.0 * self.false_id as f64 / n,
            100.0 * self.no_solve as f64 / n,
            self.wrong,
            self.checked,
            self.times[at(0.5)],
            self.times[at(0.99)],
            self.times[at(1.0)],
            self.triplets[at(0.5)],
            self.triplets[at(0.99)],
            self.triplets[at(1.0)],
        );
    }
}

/// A random star-tracker-frame direction on the sensor.
fn false_star(cam: &Camera, rng: &mut StdRng) -> V3 {
    let (u, v) = (
        rng.random_range(0.0..SIZE as f64),
        rng.random_range(0.0..SIZE as f64),
    );
    cam.pixel_to_st(u, v).unwrap()
}

#[test]
fn pyramid_identifies_simulated_frames() {
    let trials: usize = std::env::var("LIS_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let db = Db::parse(DB).unwrap();
    let cat = StarCat::parse(STARCAT).unwrap();
    let mut render = renderer(cat);
    let cam = camera();
    let mut ex = Extractor::new(SIZE, SIZE, ImageConfig::default()).unwrap();
    let mut pyramid = Pyramid::new(db.n_stars(), LisConfig::default());
    let mut px = vec![0u16; (SIZE * SIZE) as usize];
    let mut rng = StdRng::seed_from_u64(26);
    // Brightest-first false stars put ahead of the real ones.
    let cases = [0usize, 2, 5];
    let mut scores: Vec<Score> = cases.iter().map(|_| Score::default()).collect();
    let mut extract = Vec::new();
    for trial in 0..trials {
        let q = random_q(&mut rng);
        let motion = Motion {
            q_b_eci: q,
            omega_b: [0.0; 3],
            beta: [0.0; 3],
            t_exp: 0.1,
        };
        let frame = Frame::new(&cam, &motion, 16, rng.random(), trial as u64).unwrap();
        let truth = render.render(&frame, &mut px).unwrap();
        let t0 = Instant::now();
        let centroids = ex.run(&px).unwrap().to_vec();
        let real: Vec<(V3, (f64, f64))> = centroids
            .iter()
            .filter_map(|c| Some((cam.pixel_to_st(c.u, c.v)?, (c.u, c.v))))
            .collect();
        extract.push(t0.elapsed());
        for (&n_false, s) in cases.iter().zip(&mut scores) {
            let mut obs: Vec<V3> = (0..n_false).map(|_| false_star(&cam, &mut rng)).collect();
            obs.extend(real.iter().map(|(b, _)| *b));
            let t1 = Instant::now();
            pyramid.start(&obs);
            let step = pyramid.step(&db, u32::MAX);
            s.times.push(t1.elapsed());
            s.triplets.push(pyramid.triplets());
            s.trials += 1;
            let Step::Found(m) = step else {
                s.no_solve += 1;
                continue;
            };
            let mut ok = m.q.angle_to(q) < 1e-3;
            for (k, &row) in m.rows.iter().enumerate().take(m.n_obs) {
                if row == NO_ROW {
                    continue;
                }
                let Some((_, (u, v))) = k.checked_sub(n_false).and_then(|i| real.get(i)) else {
                    // A false star was matched.
                    s.wrong += 1;
                    ok = false;
                    continue;
                };
                let hips = truth_hips(&truth, *u, *v);
                if hips.is_empty() {
                    continue;
                }
                s.checked += 1;
                let hip = db.hip(row).unwrap_or(u32::MAX);
                if !hips.contains(&hip) {
                    eprintln!("trial {trial}: obs {k} is HIP {hip}, truth {hips:?}");
                    s.wrong += 1;
                    ok = false;
                }
            }
            if ok {
                s.correct += 1;
            } else {
                s.false_id += 1;
                eprintln!(
                    "trial {trial}, {n_false} false: false ID, {:.2e} rad off",
                    m.q.angle_to(q)
                );
            }
        }
    }
    extract.sort();
    println!(
        "extract median {:?}, max {:?}",
        extract[extract.len() / 2],
        extract[extract.len() - 1]
    );
    for (&n_false, s) in cases.iter().zip(&mut scores) {
        s.report(&format!("{n_false} false"));
        assert_eq!(s.false_id, 0, "false IDs with {n_false} false stars");
    }
    let clean = &scores[0];
    assert!(clean.correct as f64 >= 0.98 * clean.trials as f64);
}

/// The worst case: 16 false stars, which runs to `max_triplets`.
#[test]
fn only_false_stars_run_to_the_triplet_cap() {
    let db = Db::parse(DB).unwrap();
    let cam = camera();
    let cfg = LisConfig::default();
    let mut pyramid = Pyramid::new(db.n_stars(), cfg);
    let mut rng = StdRng::seed_from_u64(27);
    let mut times = Vec::new();
    for _ in 0..50 {
        let obs: Vec<V3> = (0..20).map(|_| false_star(&cam, &mut rng)).collect();
        let t = Instant::now();
        pyramid.start(&obs);
        assert_eq!(pyramid.step(&db, u32::MAX), Step::Exhausted);
        times.push(t.elapsed());
        // C(16, 3) = 560 triplets, under the cap.
        assert_eq!(pyramid.triplets(), 560.min(cfg.max_triplets));
    }
    times.sort();
    let per = times[times.len() / 2] / pyramid.triplets();
    println!(
        "all false: median {:?} per frame, {per:?} per triplet",
        times[times.len() / 2]
    );
}
