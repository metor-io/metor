//! The flight pipeline on the simulator's frames: render a 1024², 20° frame
//! of `starcat-g8.bin` at a random attitude with the startracker-sim pack's
//! default camera and sensor, extract centroids, identify them against the
//! committed `stars-u2000.stdb` with Pyramid, and score each identified star
//! against the renderer's truth (HIP numbers).
//!
//! `cargo test --release -p metor-star --features sim --test lis_sim --
//! --nocapture` prints the rates and timings; `LIS_TRIALS` sets the trials.

use std::time::{Duration, Instant};

use metor_star::camera::Camera;
use metor_star::catalog::Db;
use metor_star::geom::{Quat, V3};
use metor_star::image::{Extractor, ImageConfig};
use metor_star::lis::{LisConfig, NO_ROW, Pyramid, Step};
use metor_star::render::testkit::{STARCAT_G8, sim_camera, sim_renderer};
use metor_star::render::{Frame, Motion, Renderer, Truth};
use metor_star::starcat::StarCat;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, StandardNormal};

static STARCAT: &[u8] = STARCAT_G8;
static DB: &[u8] = include_bytes!("../data/stars-u2000.stdb");
const SIZE: u32 = 1024;

fn camera() -> Camera {
    sim_camera()
}

fn renderer(cat: StarCat<'_>) -> Renderer<'_> {
    sim_renderer(cat)
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
    let mut cerr = Vec::new();
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
        for c in &centroids {
            let near: Vec<_> = truth
                .brightest()
                .iter()
                .filter(|t| (t.u - c.u).hypot(t.v - c.v) < 2.0)
                .collect();
            if near.len() == 1 {
                cerr.push((near[0].u - c.u).hypot(near[0].v - c.v));
            }
        }
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
    let d: Vec<f64> = cerr.iter().map(|e| e * e).collect();
    println!(
        "centroid error RMS per axis {:.3} px over {}",
        (d.iter().sum::<f64>() / (2.0 * d.len() as f64)).sqrt(),
        d.len()
    );
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

mod tracking {
    use metor_star::lis::LisConfig;
    use metor_star::tracker::{
        Failure, Mode, Outcome, Prior, TrackConfig, Tracker, predict, sigma_of,
    };

    use super::*;

    const T_EXP: f64 = 0.1;

    struct Rig<'a> {
        render: Renderer<'a>,
        tracker: Tracker<'a>,
        px: Vec<u16>,
    }

    fn rig() -> Rig<'static> {
        let db = Db::parse(DB).unwrap();
        let cam = camera();
        let tracker = Tracker::new(
            db,
            cam,
            ImageConfig::default(),
            LisConfig::default(),
            TrackConfig::default(),
        )
        .unwrap();
        Rig {
            render: renderer(StarCat::parse(STARCAT).unwrap()),
            tracker,
            px: vec![0; (SIZE * SIZE) as usize],
        }
    }

    impl Rig<'_> {
        /// Renders the frame starting at `q` with rate `omega`, and returns the
        /// mid-exposure attitude.
        fn expose(&mut self, q: Quat, omega: V3, seed: u64) -> Quat {
            let motion = Motion {
                q_b_eci: q,
                omega_b: omega,
                beta: [0.0; 3],
                t_exp: T_EXP,
            };
            let frame = Frame::new(&camera(), &motion, 16, seed, seed).unwrap();
            self.render.render(&frame, &mut self.px).unwrap();
            predict(q, omega, 0.5 * T_EXP)
        }

        fn process(&mut self, prior: Option<Prior>, budget: u32) -> Outcome {
            self.tracker.process(&self.px, prior, budget)
        }
    }

    fn solved(outcome: Outcome) -> (Mode, metor_star::lis::Match) {
        match outcome {
            Outcome::Solved(mode, m) => (mode, m),
            other => panic!("not solved: {other:?}"),
        }
    }

    /// 100 frames at 10 Hz turning 0.1°/s: the first is lost-in-space, every
    /// later one tracks from the last solution and the gyro.
    #[test]
    fn a_turning_sequence_tracks_after_the_first_frame() {
        let mut rig = rig();
        let mut rng = StdRng::seed_from_u64(31);
        let mut q = random_q(&mut rng);
        let axis = normalize_v([1.0, 0.4, -0.3]);
        let omega = axis.map(|x| x * 0.1f64.to_radians());
        let mut last: Option<(Quat, f64)> = None;
        let (mut nees, mut n) = (0.0, 0);
        for k in 0..100u64 {
            let truth = rig.expose(q, omega, k + 1);
            // The prior: the last solution turned by the gyro over 0.1 s.
            let prior = last.map(|(q_last, sigma)| Prior {
                q_st_eci: predict(q_last, omega, 0.1),
                sigma_rad: sigma + 1e-5,
            });
            let (mode, m) = solved(rig.process(prior, u32::MAX));
            assert_eq!(
                mode,
                if k == 0 { Mode::Lost } else { Mode::Tracking },
                "frame {k}"
            );
            assert!(m.n_match >= 6, "frame {k}: {} matched", m.n_match);
            let err = m.q.angle_to(truth);
            assert!(err < 1e-3, "frame {k}: {err}");
            // NEES of the attitude error under p_theta.
            let d = truth.conj() * m.q;
            let e: V3 = core::array::from_fn(|i| 2.0 * d.0[i] * d.0[3].signum());
            if let Some(inv) = metor_star::geom::inv_sym(m.p_theta) {
                let pe = metor_star::geom::mat_vec(inv, e);
                nees += metor_star::geom::dot(e, pe);
                n += 1;
            }
            last = Some((m.q, sigma_of(m.p_theta)));
            q = predict(q, omega, 0.1);
        }
        let nees = nees / n as f64;
        println!("mean NEES {nees:.2} over {n} frames");
        // The flux-dependent σ is calibrated: neither the 0.4 of a flat
        // 0.1 px nor overconfident (3 is consistent; 1 leaves the tails
        // margin).
        assert!((0.6..2.0).contains(&nees), "NEES {nees:.2}");
    }

    #[test]
    fn priors_near_far_and_vague() {
        let mut rig = rig();
        let mut rng = StdRng::seed_from_u64(32);
        for k in 0..10 {
            let q = random_q(&mut rng);
            let truth = rig.expose(q, [0.0; 3], 100 + k);
            let turn = |q: Quat, angle: f64| {
                let axis = normalize_v([1.0, -2.0, 0.5]);
                predict(q, axis, angle)
            };
            // 3σ off with σ = 50 µrad still tracks.
            let near = Prior {
                q_st_eci: turn(truth, 150e-6),
                sigma_rad: 50e-6,
            };
            let (mode, m) = solved(rig.process(Some(near), u32::MAX));
            assert_eq!(mode, Mode::Tracking);
            assert!(m.q.angle_to(truth) < 1e-3);
            // 5° off with a small σ: tracking fails and LIS solves.
            let wrong = Prior {
                q_st_eci: turn(truth, 5f64.to_radians()),
                sigma_rad: 50e-6,
            };
            let (mode, m) = solved(rig.process(Some(wrong), u32::MAX));
            assert_eq!(mode, Mode::Lost);
            assert!(m.q.angle_to(truth) < 1e-3);
            // A vague prior is not used.
            let vague = Prior {
                q_st_eci: truth,
                sigma_rad: 0.1,
            };
            assert_eq!(solved(rig.process(Some(vague), u32::MAX)).0, Mode::Lost);
        }
    }

    /// Per-star σ falls with flux as shot noise does, from the faint-star σ
    /// down to the floor.
    #[test]
    fn star_sigma_falls_with_flux() {
        let rig = rig();
        let (t, cfg) = (&rig.tracker, TrackConfig::default());
        let rad = |px: f64| camera().pixel_sigma_to_rad(px);
        assert_eq!(t.star_sigma_rad(1.0), t.sigma_rad());
        let f = 400.0;
        assert!((t.star_sigma_rad(f) - rad(cfg.sigma_flux_px / 20.0)).abs() < 1e-15);
        assert_eq!(t.star_sigma_rad(1e9), rad(cfg.sigma_floor_px));
    }

    /// Lost-in-space and tracking weigh the stars with the same σ and report
    /// the covariance of the stars they matched, so one frame solved both
    /// ways has the same covariance.
    #[test]
    fn lost_and_tracking_agree_on_the_covariance() {
        let mut rig = rig();
        assert_eq!(rig.tracker.lis_config().sigma_rad, rig.tracker.sigma_rad());
        let mut rng = StdRng::seed_from_u64(34);
        for k in 0..5 {
            let truth = rig.expose(random_q(&mut rng), [0.0; 3], 300 + k);
            let (mode, lost) = solved(rig.process(None, u32::MAX));
            assert_eq!(mode, Mode::Lost);
            let prior = Prior {
                q_st_eci: truth,
                sigma_rad: 1e-4,
            };
            let (mode, tracked) = solved(rig.process(Some(prior), u32::MAX));
            assert_eq!(mode, Mode::Tracking);
            assert_eq!(lost.rows, tracked.rows);
            let (a, b) = (sigma_of(lost.p_theta), sigma_of(tracked.p_theta));
            assert!((a / b - 1.0).abs() < 1e-6, "lost {a:.3e}, tracking {b:.3e}");
        }
    }

    #[test]
    fn a_budgeted_search_resumes_and_a_blank_frame_has_no_stars() {
        let mut rig = rig();
        let mut rng = StdRng::seed_from_u64(33);
        let mut resumed = 0;
        for k in 0..10 {
            let q = random_q(&mut rng);
            let truth = rig.expose(q, [0.0; 3], 200 + k);
            let whole = solved(rig.process(None, u32::MAX)).1;
            let mut outcome = rig.process(None, 0);
            while outcome == Outcome::Searching {
                resumed += 1;
                outcome = rig.tracker.resume(1);
            }
            let (_, m) = solved(outcome);
            assert_eq!(m.rows, whole.rows);
            assert!(m.q.angle_to(truth) < 1e-3);
        }
        assert!(resumed >= 10);
        rig.px.fill(100);
        assert_eq!(
            rig.process(None, u32::MAX),
            Outcome::Failed(Failure::NoStars)
        );
    }

    fn normalize_v(v: V3) -> V3 {
        let n = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        v.map(|x| x / n)
    }
}
