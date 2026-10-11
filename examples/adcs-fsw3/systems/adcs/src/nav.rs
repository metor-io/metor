//! The attitude filter.
//!
//! Nav runs a multiplicative extended Kalman filter on the gyro, the
//! magnetometer, and the sun direction it reconstructs from the sun sensor
//! heads. It evaluates its references, the sun direction and the WMM field, at
//! the GPS position and at the epoch stamped on each sensor sample, so a late or
//! slow nav process still models what the sensors saw. In eclipse it runs on the
//! magnetometer alone.

use adcs_contracts::{
    AttitudeEstimate, CSS_THRESHOLD, DT, Gps, MagneticModel, NavParams, Sensors, V3, epoch_at,
    mag_field_eci, sun_dir_eci,
};
use metor_fsw_3::{Input, Output, Timestamp, system};
use nox::tensor;

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
}

impl NavState {
    pub fn new(p: NavParams) -> NavState {
        let mekf =
            metor_adcs::mekf::State::new(tensor![0.01, 0.01, 0.01], tensor![0.01, 0.01, 0.01], DT);
        NavState {
            mekf,
            sigma: p.meas_sigma,
            mag_model: MagneticModel::default(),
            first: None,
            last: None,
            sun_visible: None,
            had_fix: false,
        }
    }

    /// Returns seconds from the first sample to `stamp`, or `None` if `stamp` was
    /// already filtered.
    fn elapsed(&mut self, stamp: Timestamp) -> Option<f64> {
        if self.last == Some(stamp) {
            return None;
        }
        self.last = Some(stamp);
        let first = *self.first.get_or_insert(stamp);
        Some((stamp - first).as_secs_f64())
    }
}

#[system]
/// Updates the filter from the latest sensor sample and writes the estimate.
pub fn nav(
    #[state] state: &mut NavState,
    now: Timestamp,
    sensors: &mut Input<Sensors>,
    gps: &mut Input<Gps>,
    attitude_estimate: &mut Output<AttitudeEstimate>,
) {
    let Ok(Some(s)) = sensors.latest() else {
        return; // no sensor sample yet
    };
    let s = *s;
    let Ok(Some(g)) = gps.latest() else {
        return; // no GPS fix yet
    };
    let Some(t_sim) = state.elapsed(s.timestamp) else {
        return; // already filtered
    };
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
    state.mekf = match sun_b {
        Some(sun_b) => state.mekf.clone().estimate_attitude(
            [sun_b, s.mag_b.normalize()],
            [sun_eci, mag_eci],
            [state.sigma, state.sigma],
        ),
        None => {
            state
                .mekf
                .clone()
                .estimate_attitude([s.mag_b.normalize()], [mag_eci], [state.sigma])
        }
    };
    state.mekf.reset_if_invalid();

    let _ = attitude_estimate.write(&AttitudeEstimate {
        timestamp: now,
        q_hat_b_eci: state.mekf.q_hat,
        omega_b: s.gyro_b, // the measured rate, passed through
        b_hat_b: state.mekf.b_hat,
        p: state.mekf.p,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut nav = NavState::new(NavParams { meas_sigma: 0.01 });
        assert_eq!(nav.elapsed(Timestamp(1_000)), Some(0.0));
        assert_eq!(nav.elapsed(Timestamp(1_000)), None);
        assert_eq!(nav.elapsed(Timestamp(1_000)), None);
    }

    /// The reference epoch follows the sample stamps, so skipped cycles advance it
    /// by their full span.
    #[test]
    fn epoch_follows_the_samples_stamps() {
        let mut nav = NavState::new(NavParams { meas_sigma: 0.01 });
        let step = (DT * 1e6) as i64;
        assert_eq!(nav.elapsed(Timestamp(7 * step)), Some(0.0));
        let one = nav.elapsed(Timestamp(8 * step)).expect("a new sample");
        assert!((one - DT).abs() < 1e-6, "{one}");
        // Five cycles arrive at once.
        let skipped = nav.elapsed(Timestamp(13 * step)).expect("a new sample");
        assert!((skipped - 6.0 * DT).abs() < 1e-5, "{skipped}");
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
}
