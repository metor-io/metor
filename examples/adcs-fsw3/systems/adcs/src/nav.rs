//! The attitude filter.
//!
//! Nav runs a multiplicative extended Kalman filter on the gyro, the
//! magnetometer, and the sun direction it reconstructs from the sun sensor
//! heads. It evaluates its references, the sun direction and the WMM field, at
//! the GPS position and at the epoch stamped on each sensor sample, so a late or
//! slow nav process still models what the sensors saw. In eclipse it runs on the
//! magnetometer alone. Star vectors, when a tracker is connected, go through the
//! same vector update, gated on their fit residuals; while the attitude is
//! lost, the tracker's own solution resets it first.

use adcs_contracts::{
    AttitudeEstimate, CSS_THRESHOLD, Gps, MAX_STARS, MagneticModel, NavParams, NavStats, Quat,
    Sensors, StarSolution, StarStatus, StarVectors, V3, epoch_at, mag_field_eci, sun_dir_eci,
};
use metor_adcs::mekf::{UpdateCounts, VectorObs};
use metor_fsw_3::{Input, Output, Timestamp, system};
use nox::{Quaternion, tensor};

/// The oldest star frame (s) nav fuses.
const MAX_STAR_AGE: f64 = 1.0;
/// Attitude 1σ (rad) above which a star frame first resets the attitude to
/// the tracker's solution.
const RESET_ABOVE: f64 = 1.0 * core::f64::consts::PI / 180.0;
/// The attitude 1σ (rad) a reset leaves: tight enough to linearise about,
/// loose enough that the frame's own stars set the covariance.
const RESET_SIGMA: f64 = 0.05 * core::f64::consts::PI / 180.0;
/// Stars are live while a frame fused a star within this long (s).
const LIVE_FOR: f64 = 1.0;
/// Reconstructs the body-frame sun direction from the sun sensor readings, or
/// returns `None` when every head is dark. Each opposed pair of heads gives one
/// component.
pub fn sun_from_css(css: &[f64; 6]) -> Option<V3> {
    if css.iter().all(|r| *r < CSS_THRESHOLD) {
        return None;
    }
    let v: V3 = tensor![css[0] - css[3], css[1] - css[4], css[2] - css[5]];
    Some(v.normalize())
}

/// `v` turned by `−ω·dt` (Rodrigues): a fixed inertial direction seen from a
/// body turning at `omega` for `dt` seconds.
pub fn rotate_back(v: V3, omega: V3, dt: f64) -> V3 {
    let rate: f64 = omega.norm().into_buf();
    let angle = rate * dt;
    if angle == 0.0 {
        return v;
    }
    let k = omega * (1.0 / rate);
    let k_dot_v: f64 = k.dot(&v).into_buf();
    v * angle.cos() - k.cross(&v) * angle.sin() + k * (k_dot_v * (1.0 - angle.cos()))
}

/// `q` carried `dt` seconds on at body rate `omega`: the attitude that sees
/// [`rotate_back`]'s vectors.
pub fn rotate_on(q: Quat, omega: V3, dt: f64) -> Quat {
    let angle: f64 = omega.norm().into_buf() * dt;
    if angle == 0.0 {
        return q;
    }
    q * Quaternion::from_axis_angle(omega, angle)
}

/// The frame's star vectors moved from image time to nav time, `dt ≥ 0`
/// seconds later, with σ² grown by the gyro noise `gyro_noise` (rad/s/√Hz)
/// over `dt`.
pub fn star_obs(
    sv: &StarVectors,
    omega: V3,
    dt: f64,
    gyro_noise: f64,
) -> impl Iterator<Item = VectorObs> + '_ {
    sv.obs().map(move |(b, r, sigma)| VectorObs {
        body: rotate_back(b, omega, dt),
        reference: r,
        sigma: (sigma * sigma + gyro_noise * gyro_noise * dt).sqrt(),
    })
}

/// The gyro the filter models.
#[derive(Clone, Copy, Debug)]
struct Gyro {
    /// White noise (rad/s/√Hz).
    noise: f64,
    /// Bias random walk (rad/s/√s).
    walk: f64,
}

/// The navigation filter and its reference field model.
pub struct NavState {
    mekf: metor_adcs::mekf::State,
    sigma: f64,
    mag_model: MagneticModel,
    /// Stamp of the first sensor sample, where the reference epoch starts.
    first: Option<Timestamp>,
    /// Stamp of the last sample filtered, so a retained sample is not filtered twice.
    last: Option<Timestamp>,
    /// Sun visibility last cycle, so each loss and reacquisition logs once.
    sun_visible: Option<bool>,
    /// Whether a GPS fix has arrived, so the first one logs once.
    had_fix: bool,
    star_gate: f64,
    sun_mag: bool,
    /// The last star frame fused, so a retained frame is not fused twice.
    last_star: Option<u64>,
    /// Whether stars were live last cycle, so each change logs once.
    stars_live: Option<bool>,
    /// When a star frame last fused a star.
    star_used_at: Option<f64>,
    gyro: Gyro,
    stats: NavStats,
}

/// The filter for a step of `dt` seconds: the gyro's noise and bias walk,
/// and the process noise they give over the step.
fn mekf_for(gyro: Gyro, dt: f64) -> metor_adcs::mekf::State {
    let noise: V3 = tensor![gyro.noise, gyro.noise, gyro.noise];
    let walk: V3 = tensor![gyro.walk, gyro.walk, gyro.walk];
    metor_adcs::mekf::State::new(noise, walk, dt)
}

impl NavState {
    pub fn new(p: NavParams) -> NavState {
        let gyro = Gyro {
            noise: p.gyro_noise,
            walk: p.bias_walk,
        };
        NavState {
            mekf: mekf_for(gyro, 0.0).with_bias_sigma(p.bias_sigma),
            sigma: p.meas_sigma,
            mag_model: MagneticModel::default(),
            first: None,
            last: None,
            sun_visible: None,
            had_fix: false,
            star_gate: p.star_gate,
            sun_mag: p.sun_mag,
            last_star: None,
            stars_live: None,
            star_used_at: None,
            gyro,
            stats: NavStats {
                timestamp: Timestamp(0),
                vectors_used: 0,
                vectors_gated: 0,
                stars_used: 0,
                stars_gated: 0,
                star_frame_id: 0,
                contradictions: 0,
                prior_resets: 0,
            },
        }
    }

    /// Returns seconds from the first sample to `stamp`, and from the last
    /// filtered sample to it, or `None` if `stamp` was already filtered.
    fn elapsed(&mut self, stamp: Timestamp) -> Option<(f64, f64)> {
        if self.last == Some(stamp) {
            return None;
        }
        let last = self.last.replace(stamp).unwrap_or(stamp);
        let first = *self.first.get_or_insert(stamp);
        Some(((stamp - first).as_secs_f64(), (stamp - last).as_secs_f64()))
    }

    /// Fuses the latest star frame if it is new and no older than
    /// [`MAX_STAR_AGE`] at `t_nav`. `solution` is the tracker's latest.
    fn fuse_stars(
        &mut self,
        sv: Option<&StarVectors>,
        solution: Option<&StarSolution>,
        t_nav: Timestamp,
        t_sim: f64,
    ) {
        let Some(sv) = sv else {
            return; // no tracker, or no frame yet
        };
        let age = (t_nav.0 - sv.timestamp.0) as f64 * 1e-6;
        let fresh = (0.0..=MAX_STAR_AGE).contains(&age);
        if !fresh || self.last_star == Some(sv.frame_id) {
            return;
        }
        self.last_star = Some(sv.frame_id);
        let mut buf = [VectorObs {
            body: V3::zeros(),
            reference: V3::zeros(),
            sigma: 0.0,
        }; MAX_STARS];
        let n = star_obs(sv, self.mekf.omega, age, self.gyro.noise)
            .zip(buf.iter_mut())
            .map(|(o, slot)| *slot = o)
            .count();
        let solution = solution
            .filter(|s| s.frame_id == sv.frame_id && s.status() == Ok(StarStatus::Valid))
            .map(|s| rotate_on(s.q_b_eci, self.mekf.omega, age));
        let counts = self.update_stars(&buf[..n], solution);
        if counts.used > 0 {
            self.star_used_at = Some(t_sim);
        }
        self.stats.stars_used = counts.used.into();
        self.stats.stars_gated = counts.gated.into();
        self.stats.star_frame_id = sv.frame_id;
    }

    /// Fuses one frame of star vectors. While the attitude 1σ is over
    /// [`RESET_ABOVE`], or when the gate rejects the whole frame, the
    /// attitude is first reset to `solution`, the tracker's own at nav time,
    /// which its verification already vouches for.
    fn update_stars(&mut self, obs: &[VectorObs], solution: Option<Quat>) -> UpdateCounts {
        let lost = self.mekf.attitude_variance() > RESET_ABOVE * RESET_ABOVE;
        if !lost {
            let counts = self.mekf.update_vectors(obs, self.star_gate);
            if counts.used > 0 {
                return counts;
            }
            self.stats.contradictions += 1;
            if solution.is_none() {
                return counts;
            }
        }
        if let Some(q) = solution {
            self.reset(q);
        }
        self.mekf.update_vectors(obs, self.star_gate)
    }

    fn reset(&mut self, q: Quat) {
        self.mekf.q_hat = q;
        self.mekf.reset_attitude_variance(RESET_SIGMA * RESET_SIGMA);
        self.stats.prior_resets += 1;
    }

    /// Whether a star frame fused a star within [`LIVE_FOR`] of `t_sim`,
    /// logging each change once.
    fn stars_live(&mut self, t_sim: f64) -> bool {
        let live = self.star_used_at.is_some_and(|t| t_sim - t <= LIVE_FOR);
        match (self.stars_live.replace(live), live) {
            (Some(true), false) => tracing::warn!(t_sim, "star vectors stale"),
            (Some(false) | None, true) => tracing::info!(t_sim, "star vectors live"),
            _ => {}
        }
        live
    }

    /// Fuses the sun and magnetometer vectors. Once star vectors are live
    /// they are gated like stars, so a biased coarse sensor cannot pull the
    /// estimate away from the stars.
    fn fuse_sun_mag(&mut self, obs: &[VectorObs], t_sim: f64) {
        let gate = if self.stars_live(t_sim) {
            self.star_gate
        } else {
            f64::INFINITY
        };
        let counts = self.mekf.update_vectors(obs, gate);
        self.record_vectors(counts);
    }

    fn record_vectors(&mut self, counts: UpdateCounts) {
        self.stats.vectors_used = counts.used.into();
        self.stats.vectors_gated = counts.gated.into();
    }

    /// Sets the filter's step to `dt` seconds, with the process noise for it.
    fn seed(&mut self, dt: f64) {
        let seeded = mekf_for(self.gyro, dt);
        self.mekf.dt = seeded.dt;
        self.mekf.yqy = seeded.yqy;
    }
}

#[system]
/// Updates the filter from the latest sensor sample and writes the estimate.
#[allow(clippy::too_many_arguments)]
pub fn nav(
    #[state] state: &mut NavState,
    now: Timestamp,
    sensors: &mut Input<Sensors>,
    gps: &mut Input<Gps>,
    stars: &mut Input<StarVectors>,
    star_solution: &mut Input<StarSolution>,
    attitude_estimate: &mut Output<AttitudeEstimate>,
    nav_stats: &mut Output<NavStats>,
) {
    let Ok(Some(s)) = sensors.latest() else {
        return; // no sensor sample yet
    };
    let s = *s;
    let Ok(Some(g)) = gps.latest() else {
        return; // no GPS fix yet
    };
    let Some((t_sim, dt)) = state.elapsed(s.timestamp) else {
        return; // already filtered
    };
    state.seed(dt);
    let epoch = epoch_at(t_sim);
    let gps_pos: V3 = g.pos_eci;
    if !state.had_fix {
        state.had_fix = true;
        tracing::info!(t_sim, "first GPS fix; filter references live");
    }

    let sun_eci = sun_dir_eci(epoch);
    let mag_eci = mag_field_eci(&mut state.mag_model, epoch, &gps_pos).normalize();

    state.mekf.omega = s.gyro_b;
    let sun_b = sun_from_css(&s.css);

    match (state.sun_visible, sun_b.is_some()) {
        (Some(true), false) => {
            tracing::warn!(t_sim, "sun lost; running magnetometer-only")
        }
        (Some(false), true) => tracing::info!(t_sim, "sun reacquired"),
        _ => {}
    }

    state.sun_visible = Some(sun_b.is_some());
    state.mekf.propagate();
    if state.sun_mag {
        let obs = |body, reference| VectorObs {
            body,
            reference,
            sigma: state.sigma,
        };
        let mag = obs(s.mag_b.normalize(), mag_eci);
        match sun_b {
            Some(sun_b) => state.fuse_sun_mag(&[obs(sun_b, sun_eci), mag], t_sim),
            None => state.fuse_sun_mag(&[mag], t_sim),
        }
    }
    let star_frame = stars.latest().ok().flatten().map(|sv| *sv);
    let solution = star_solution.latest().ok().flatten().map(|s| *s);
    state.fuse_stars(star_frame.as_ref(), solution.as_ref(), s.timestamp, t_sim);
    state.mekf.reset_if_invalid();

    let _ = attitude_estimate.write(&AttitudeEstimate {
        timestamp: now,
        q_hat_b_eci: state.mekf.q_hat,
        omega_b: s.gyro_b, // the measured rate, passed through
        b_hat_b: state.mekf.b_hat,
        p: state.mekf.p,
    });
    state.stats.timestamp = now;
    let _ = nav_stats.write(&state.stats);
}

#[cfg(test)]
mod tests {
    use adcs_contracts::StarMode;

    use super::*;

    fn params() -> NavParams {
        NavParams {
            meas_sigma: 0.01,
            star_gate: 13.8,
            sun_mag: true,
            gyro_noise: 1.83e-4,
            bias_walk: 2.19e-4,
            bias_sigma: 1e-3,
        }
    }

    fn frame(stamp: i64, id: u64, rows: &[([f64; 3], [f64; 3])]) -> StarVectors {
        let mut sv = StarVectors::empty(Timestamp(stamp), id);
        for (b, r) in rows {
            sv.push(*b, *r, 34e-6, 1);
        }
        sv
    }

    /// A noiseless frame of [`MAX_STARS`] stars within about 8° of the
    /// boresight, and its solution, as a tracker at attitude `q` gives them.
    fn stars_at(q: Quat, t: Timestamp, id: u64) -> (StarVectors, StarSolution) {
        let mut sv = StarVectors::empty(t, id);
        for i in 0..MAX_STARS {
            // A golden-angle spiral, so the stars spread over the field.
            let (r, az) = (
                0.14 * ((i as f64 + 0.5) / MAX_STARS as f64).sqrt(),
                i as f64 * 2.4,
            );
            let b: V3 = (tensor![r * az.cos(), r * az.sin(), 1.0] as V3).normalize();
            sv.push(b.into_buf(), (q * b).into_buf(), 34e-6, i as u32);
        }
        let mut solution = StarSolution::new(t, id, StarMode::Tracking, StarStatus::Valid);
        solution.q_b_eci = q;
        (sv, solution)
    }

    /// The angle (rad) between two attitudes, which `angular_distance`'s
    /// `acos` makes NaN for a noiseless match.
    fn angle_between(a: Quat, b: Quat) -> f64 {
        let [x, y, z, w] = (a.inverse() * b).0.into_buf();
        2.0 * (x * x + y * y + z * z).sqrt().atan2(w.abs())
    }

    #[test]
    fn star_obs_turns_by_the_rate_over_the_age() {
        let sv = frame(0, 1, &[([1.0, 0.0, 0.0], [1.0, 0.0, 0.0])]);
        let omega: V3 = tensor![0.0, 0.0, 0.2];
        let o = star_obs(&sv, omega, 0.5, 0.01).next().expect("one row");
        let expected: V3 = tensor![0.1f64.cos(), -0.1f64.sin(), 0.0];
        let err: f64 = (o.body - expected).norm().into_buf();
        assert!(err < 1e-15, "{err}");
        let grown = (34e-6f64.powi(2) + 0.01f64.powi(2) * 0.5).sqrt();
        assert!((o.sigma - grown).abs() < 1e-18);
    }

    #[test]
    fn star_obs_at_zero_age_is_the_frame() {
        let b = [0.6, 0.0, 0.8];
        let sv = frame(0, 1, &[(b, [0.0, 0.0, 1.0])]);
        let o = star_obs(&sv, tensor![0.1, -0.2, 0.3], 0.0, 0.01)
            .next()
            .expect("one row");
        assert_eq!(o.body.into_buf(), b);
        assert_eq!(o.sigma, 34e-6);
    }

    #[test]
    fn future_and_old_frames_are_dropped() {
        let mut nav = NavState::new(params());
        let before = nav.mekf.clone();
        let rows = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        nav.fuse_stars(
            Some(&frame(2_000_000, 1, &rows)),
            None,
            Timestamp(1_000_000),
            0.0,
        );
        nav.fuse_stars(Some(&frame(0, 2, &rows)), None, Timestamp(1_500_000), 0.0);
        assert_eq!(nav.last_star, None);
        assert_eq!(nav.star_used_at, None);
        assert_eq!(nav.mekf.p.into_buf(), before.p.into_buf());
        nav.fuse_stars(Some(&frame(0, 3, &rows)), None, Timestamp(500_000), 0.0);
        assert_eq!(nav.last_star, Some(3));
        assert_ne!(nav.mekf.p.into_buf(), before.p.into_buf());
    }

    #[test]
    fn a_frame_is_fused_once() {
        let mut nav = NavState::new(params());
        let rows = [
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let sv = frame(0, 7, &rows);
        nav.fuse_stars(Some(&sv), None, Timestamp(0), 0.0);
        let after = nav.mekf.clone();
        nav.fuse_stars(Some(&sv), None, Timestamp(0), 0.0);
        assert_eq!(nav.mekf.p.into_buf(), after.p.into_buf());
    }

    #[test]
    fn zero_rows_are_gated_not_fused() {
        let mut nav = NavState::new(params());
        let before = nav.mekf.clone();
        let mut sv = StarVectors::empty(Timestamp(0), 1);
        sv.count = MAX_STARS as u64;
        nav.fuse_stars(Some(&sv), None, Timestamp(0), 0.0);
        assert_eq!(nav.last_star, Some(1));
        assert_eq!(nav.mekf.p.into_buf(), before.p.into_buf());
        assert_eq!(nav.mekf.q_hat.0.into_buf(), before.q_hat.0.into_buf());
    }

    /// A filter at identity with `P = I` resets to a turning truth 170° away
    /// on one frame 0.1 s old: the solution is carried to nav time, and its
    /// stars then fuse.
    #[test]
    fn a_lost_filter_resets_to_the_solution() {
        let truth: Quat =
            nox::Quaternion::from_axis_angle(tensor![1.0, 1.0, 1.0], 170f64.to_radians());
        let omega: V3 = tensor![0.05, -0.1, 0.08];
        let mut nav = NavState::new(NavParams {
            sun_mag: false,
            ..params()
        });
        nav.mekf.omega = omega;
        let (sv, solution) = stars_at(truth, Timestamp(0), 1);
        nav.fuse_stars(Some(&sv), Some(&solution), Timestamp(100_000), 0.0);
        assert_eq!(nav.stats.prior_resets, 1);
        assert_eq!(nav.stats.stars_used, MAX_STARS as u64);
        for (b, r, _) in sv.obs() {
            let seen = rotate_back(b, omega, 0.1);
            let err: f64 = (nav.mekf.q_hat.inverse() * r - seen).norm().into_buf();
            assert!(err < 1e-6, "{err}");
        }
        assert!(nav.mekf.attitude_variance() < 1e-7);
    }

    /// Returns noiseless readings for a body-frame sun direction. Written out here
    /// so nav does not depend on the plant.
    fn readings_for(sun_b: &V3) -> [f64; 6] {
        let [x, y, z] = sun_b.into_buf();
        [
            x.max(0.0),
            y.max(0.0),
            z.max(0.0),
            (-x).max(0.0),
            (-y).max(0.0),
            (-z).max(0.0),
        ]
    }

    /// Noiseless readings reconstruct the sun direction exactly, with every head
    /// lit by at least one case.
    #[test]
    fn css_reconstruction_recovers_sun_direction() {
        for sun in [
            (tensor![0.3, -0.5, 0.8] as V3).normalize(),
            (tensor![-1.0, 0.1, 0.0] as V3).normalize(),
            (tensor![0.0, 0.0, -1.0] as V3).normalize(),
            (tensor![1.0, 1.0, 1.0] as V3).normalize(),
        ] {
            let s = sun_from_css(&readings_for(&sun)).expect("lit readings reconstruct");
            let err: f64 = (s - sun).norm().into_buf();
            assert!(
                err < 1e-12,
                "reconstruction error {err} for {:?}",
                sun.into_buf()
            );
        }
    }

    /// The port keeps returning its latest sample, and the filter uses it once.
    #[test]
    fn repeated_sample_is_not_reobserved() {
        let mut nav = NavState::new(params());
        assert_eq!(nav.elapsed(Timestamp(1_000)), Some((0.0, 0.0)));
        assert_eq!(nav.elapsed(Timestamp(1_000)), None);
        assert_eq!(nav.elapsed(Timestamp(1_000)), None);
    }

    /// The reference epoch and the filter's step follow the sample stamps, so
    /// skipped cycles advance them by their full span.
    #[test]
    fn epoch_and_step_follow_the_samples_stamps() {
        let mut nav = NavState::new(params());
        let step = 8_333;
        let secs = step as f64 * 1e-6;
        assert_eq!(nav.elapsed(Timestamp(7 * step)), Some((0.0, 0.0)));
        let (one, dt) = nav.elapsed(Timestamp(8 * step)).expect("a new sample");
        assert!(
            (one - secs).abs() < 1e-9 && (dt - secs).abs() < 1e-9,
            "{one} {dt}"
        );
        // Five cycles arrive at once.
        let (skipped, dt) = nav.elapsed(Timestamp(13 * step)).expect("a new sample");
        assert!((skipped - 6.0 * secs).abs() < 1e-9, "{skipped}");
        assert!((dt - 5.0 * secs).abs() < 1e-9, "{dt}");
        nav.seed(dt);
        assert_eq!(nav.mekf.dt, dt);
    }

    /// With every head below the threshold the sun is lost, so noise never becomes
    /// a direction.
    #[test]
    fn css_all_dark_is_sun_lost() {
        assert!(sun_from_css(&[0.0; 6]).is_none());
        // Noise just under the threshold on every head.
        assert!(sun_from_css(&[0.05, 0.02, -0.03, 0.04, 0.01, 0.09]).is_none());
        // One head just over the threshold counts.
        assert!(sun_from_css(&[0.11, 0.0, 0.0, 0.0, 0.0, 0.0]).is_some());
    }

    /// A filter and a still truth, stepped one 120 Hz cycle at a time with a
    /// star frame every 12th cycle.
    struct Bench {
        nav: NavState,
        truth: Quat,
        k: i64,
    }

    impl Bench {
        fn new(sun_mag: bool) -> Bench {
            let truth: Quat = nox::Quaternion::from_axis_angle(tensor![0.2, -1.0, 0.4], 1.1);
            let mut nav = NavState::new(NavParams {
                sun_mag,
                ..params()
            });
            nav.seed(1.0 / 120.0);
            Bench { nav, truth, k: 0 }
        }

        /// One cycle, with sun and magnetometer vectors turned by `coarse`
        /// when given, and a star frame when `stars` and one is due.
        fn step(&mut self, coarse: Option<Quat>, stars: bool) {
            let t = Timestamp(self.k * 8_333);
            self.nav.mekf.omega = V3::zeros();
            self.nav.mekf.propagate();
            if let Some(bias) = coarse {
                let refs: [V3; 2] = [tensor![0.3, 0.9, -0.2], tensor![-0.8, 0.1, 0.6]];
                let obs = refs.map(|r| {
                    let r = r.normalize();
                    VectorObs {
                        body: bias * (self.truth.inverse() * r),
                        reference: r,
                        sigma: 0.02,
                    }
                });
                self.nav.fuse_sun_mag(&obs, self.k as f64 / 120.0);
            }
            if stars && self.k % 12 == 0 {
                let (sv, solution) = stars_at(self.truth, t, self.k as u64);
                let t_sim = self.k as f64 / 120.0;
                self.nav.fuse_stars(Some(&sv), Some(&solution), t, t_sim);
            }
            self.k += 1;
        }

        fn error_deg(&self) -> f64 {
            angle_between(self.nav.mekf.q_hat, self.truth).to_degrees()
        }
    }

    /// Sun and magnetometer references 2° off settle the filter there before
    /// stars arrive; the gate rejects the first star frame, which resets it
    /// to the solution, and the coarse sensors do not pull it back.
    #[test]
    fn stars_override_biased_coarse_sensors() {
        let bias = nox::Quaternion::from_axis_angle(tensor![0.0, 0.6, 0.8], 2f64.to_radians());
        let mut b = Bench::new(true);
        for _ in 0..120 * 10 {
            b.step(Some(bias), false);
        }
        assert!(b.error_deg() > 1.0, "{}°", b.error_deg());
        for _ in 0..120 * 30 {
            b.step(Some(bias), true);
        }
        assert!(b.nav.stats.prior_resets >= 1);
        assert!(b.nav.stats.stars_used >= 16, "{:?}", b.nav.stats);
        assert!(b.error_deg() < 0.01, "{}°", b.error_deg());
    }
}
