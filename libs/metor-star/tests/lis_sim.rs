//! The flight pipeline on the simulator's frames: render 1024², 20° frames
//! of `starcat-g8.bin` on the GPU, extract centroids, identify them against
//! the onboard DB, track, and score against the renderer's truth.
//!
//! `cargo test --release -p metor-star --test lis_sim -- --nocapture` prints
//! the rates; `LIS_TRIALS` sets the trials.

use metor_star::catalog::Db;
use metor_star::geom::{Quat, V3, dot, inv_sym, mat_vec};
use metor_star::image::{Extractor, ImageConfig};
use metor_star::lis::{LisConfig, NO_ROW, Pyramid, Step};
use metor_star::render::gpu::GpuRenderer;
use metor_star::render::testkit::{STARCAT_G8, sim_camera, sim_renderer};
use metor_star::render::{Frame, Motion, Truth};
use metor_star::starcat::StarCat;
use metor_star::tracker::{Failure, Mode, Outcome, Prior, TrackConfig, Tracker, predict, sigma_of};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use rand_distr::{Distribution, StandardNormal};

static DB: &[u8] = include_bytes!("../data/stars-u2000.stdb");
const SIZE: u32 = 1024;
const T_EXP: f64 = 0.1;

fn random_q(rng: &mut StdRng) -> Quat {
    let v: [f64; 4] = core::array::from_fn(|_| StandardNormal.sample(rng));
    Quat(v).normalized().unwrap()
}

struct Rig {
    render: GpuRenderer<'static>,
    px: Vec<u16>,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            render: sim_renderer(StarCat::parse(STARCAT_G8).unwrap()),
            px: vec![0; (SIZE * SIZE) as usize],
        }
    }

    /// Renders the frame starting at `q` turning at `omega`, and returns its
    /// truth and mid-exposure attitude.
    fn expose(&mut self, q: Quat, omega: V3, seed: u64) -> (Truth, Quat) {
        let motion = Motion {
            q_b_eci: q,
            omega_b: omega,
            beta: [0.0; 3],
            t_exp: T_EXP,
        };
        let frame = Frame::new(&sim_camera(), &motion, 16, seed, seed).unwrap();
        let truth = self.render.render(&frame, &mut self.px).unwrap();
        (truth, predict(q, omega, 0.5 * T_EXP))
    }
}

fn tracker() -> Tracker<'static> {
    let db = Db::parse(DB).unwrap();
    let (image, lis) = (ImageConfig::default(), LisConfig::default());
    Tracker::new(db, sim_camera(), image, lis, TrackConfig::default()).unwrap()
}

fn solved(outcome: Outcome) -> (Mode, metor_star::lis::Match) {
    match outcome {
        Outcome::Solved(mode, m) => (mode, m),
        other => panic!("not solved: {other:?}"),
    }
}

/// Lost-in-space on random attitudes, with 0 or 5 false stars put ahead of
/// the real ones: no false IDs, every identified star is the truth's HIP
/// star at its centroid, and at least 98% of clean frames solve.
#[test]
fn pyramid_identifies_simulated_frames() {
    let trials: usize = std::env::var("LIS_TRIALS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let db = Db::parse(DB).unwrap();
    let cam = sim_camera();
    let mut rig = Rig::new();
    let mut ex = Extractor::new(SIZE, SIZE, ImageConfig::default()).unwrap();
    let mut pyramid = Pyramid::new(db.n_stars(), LisConfig::default());
    let mut rng = StdRng::seed_from_u64(26);
    let mut solved = [0usize; 2];
    for trial in 0..trials {
        let q = random_q(&mut rng);
        let (truth, _) = rig.expose(q, [0.0; 3], rng.random());
        let real: Vec<(V3, f64, f64)> = ex
            .run(&rig.px)
            .unwrap()
            .iter()
            .filter_map(|c| Some((cam.pixel_to_st(c.u, c.v)?, c.u, c.v)))
            .collect();
        for (case, n_false) in [0usize, 5].into_iter().enumerate() {
            let mut obs: Vec<V3> = (0..n_false)
                .map(|_| {
                    let (u, v) = (rng.random_range(0.0..1024.0), rng.random_range(0.0..1024.0));
                    cam.pixel_to_st(u, v).unwrap()
                })
                .collect();
            obs.extend(real.iter().map(|r| r.0));
            pyramid.start(&obs);
            let Step::Found(m) = pyramid.step(&db, u32::MAX) else {
                continue;
            };
            assert!(m.q.angle_to(q) < 1e-3, "trial {trial}: a false ID");
            for (k, &row) in m.rows.iter().enumerate().take(m.n_obs) {
                if row == NO_ROW {
                    continue;
                }
                let (_, u, v) = *k
                    .checked_sub(n_false)
                    .and_then(|i| real.get(i))
                    .unwrap_or_else(|| panic!("trial {trial}: a false star matched"));
                // A blended double is one blob: either HIP is right.
                let hips: Vec<u32> = truth
                    .brightest()
                    .iter()
                    .filter(|t| (t.u - u).hypot(t.v - v) < 1.0 && t.cat_id != u32::MAX)
                    .map(|t| t.cat_id)
                    .collect();
                let hip = db.hip(row).unwrap();
                assert!(hips.is_empty() || hips.contains(&hip), "trial {trial}");
            }
            solved[case] += 1;
        }
    }
    println!("solved {solved:?} of {trials}");
    assert!(solved[0] as f64 >= 0.98 * trials as f64);
}

/// 100 frames at 10 Hz turning 0.1°/s: the first is lost-in-space, every
/// later one tracks from the last solution and the gyro, and the reported
/// covariance is calibrated (mean NEES near 3/3).
#[test]
fn a_turning_sequence_tracks_after_the_first_frame() {
    let (mut rig, mut tracker) = (Rig::new(), tracker());
    let mut rng = StdRng::seed_from_u64(31);
    let mut q = random_q(&mut rng);
    let omega = [1.0, 0.4, -0.3].map(|x: f64| x / 1.16f64.sqrt() * 0.1f64.to_radians());
    let mut last: Option<(Quat, f64)> = None;
    let (mut nees, mut n) = (0.0, 0);
    for k in 0..100u64 {
        let (_, truth) = rig.expose(q, omega, k + 1);
        let prior = last.map(|(q_last, sigma)| Prior {
            q_st_eci: predict(q_last, omega, 0.1),
            sigma_rad: sigma + 1e-5,
        });
        let (mode, m) = solved(tracker.process(&rig.px, prior, u32::MAX));
        let expect = if k == 0 { Mode::Lost } else { Mode::Tracking };
        assert_eq!(mode, expect, "frame {k}");
        assert!(m.n_match >= 6, "frame {k}: {} matched", m.n_match);
        assert!(m.q.angle_to(truth) < 1e-3, "frame {k}");
        let d = truth.conj() * m.q;
        let e: V3 = core::array::from_fn(|i| 2.0 * d.0[i] * d.0[3].signum());
        if let Some(inv) = inv_sym(m.p_theta) {
            nees += dot(e, mat_vec(inv, e));
            n += 1;
        }
        last = Some((m.q, sigma_of(m.p_theta)));
        q = predict(q, omega, 0.1);
    }
    let nees = nees / f64::from(n);
    println!("mean NEES {nees:.2} over {n} frames");
    assert!((0.6..2.0).contains(&nees), "NEES {nees:.2}");
}

/// A near prior tracks; a 5° wrong one falls back to lost-in-space; a vague
/// one is not used. Lost and tracking agree on the covariance.
#[test]
fn priors_near_far_and_vague() {
    let (mut rig, mut tracker) = (Rig::new(), tracker());
    let mut rng = StdRng::seed_from_u64(32);
    let turn = |q: Quat, angle: f64| predict(q, [0.436, -0.873, 0.218], angle);
    for k in 0..8 {
        let (_, truth) = rig.expose(random_q(&mut rng), [0.0; 3], 100 + k);
        let mut process = |q_st_eci, sigma_rad| {
            let prior = Prior {
                q_st_eci,
                sigma_rad,
            };
            solved(tracker.process(&rig.px, Some(prior), u32::MAX))
        };
        let (mode, near) = process(turn(truth, 150e-6), 50e-6);
        assert_eq!(mode, Mode::Tracking);
        assert!(near.q.angle_to(truth) < 1e-3);
        let (mode, lost) = process(turn(truth, 5f64.to_radians()), 50e-6);
        assert_eq!(mode, Mode::Lost);
        assert!(lost.q.angle_to(truth) < 1e-3);
        assert_eq!(process(truth, 0.1).0, Mode::Lost);
        if lost.rows == near.rows {
            let (a, b) = (sigma_of(lost.p_theta), sigma_of(near.p_theta));
            assert!((a / b - 1.0).abs() < 1e-6, "lost {a:.3e}, tracking {b:.3e}");
        }
    }
}

/// A search cut to no budget resumes to the same solution; a blank frame has
/// no stars.
#[test]
fn a_budgeted_search_resumes_and_a_blank_frame_has_no_stars() {
    let (mut rig, mut tracker) = (Rig::new(), tracker());
    let mut rng = StdRng::seed_from_u64(33);
    let mut resumed = 0;
    for k in 0..10 {
        let (_, truth) = rig.expose(random_q(&mut rng), [0.0; 3], 200 + k);
        let whole = solved(tracker.process(&rig.px, None, u32::MAX)).1;
        let mut outcome = tracker.process(&rig.px, None, 0);
        while outcome == Outcome::Searching {
            resumed += 1;
            outcome = tracker.resume(1);
        }
        let (_, m) = solved(outcome);
        assert_eq!(m.rows, whole.rows);
        assert!(m.q.angle_to(truth) < 1e-3);
    }
    assert!(resumed >= 10);
    rig.px.fill(100);
    let blank = tracker.process(&rig.px, None, u32::MAX);
    assert_eq!(blank, Outcome::Failed(Failure::NoStars));
}
