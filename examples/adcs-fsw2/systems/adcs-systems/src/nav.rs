//! The MEKF **navigation filter** of the `adcs-fsw2` target. It wraps
//! [`metor_adcs::mekf::State`] (a multiplicative EKF), reconstructing the sun
//! observation from the six raw CSS head readings ([`sun_from_css`]) and modeling the
//! inertial **references** itself (cube-sat's `Nav::from_sensors`): the sun direction from
//! the Vallado ephemeris at its own sim-time epoch, and the magnetic field from the NOAA
//! WMM evaluated at the **GPS** position. When every CSS head is dark (eclipse) the filter
//! runs magnetometer-only — the about-B̂ attitude is instantaneously unobservable and rides
//! the gyro until the sun returns. It knows only what the flight software knows — the raw
//! readings and the noisy GPS measurement — never the plant's truth.
//!
//! Fn-authored (docs/design-packs-authoring.md §4): [`NavState`] is a plain struct,
//! [`nav_init`] the `NavParams -> NavState` constructor, and [`nav_execute`]'s signature
//! the port set — the crate's [`pack`](crate::pack) registers all three.

use adcs_contracts::{
    AttitudeEstimate, CSS_THRESHOLD, DT, Gps, MagneticModel, NavParams, Sensors, V3, epoch_at,
    mag_field_eci, sun_dir_eci,
};
use metor_fsw_2::{Input, Output, Timestamp};
use nox::tensor;

/// The sun vector reconstructed from the six face-mounted CSS readings (frame order
/// `[+X, +Y, +Z, −X, −Y, −Z]`) by opposed-pair differencing: the sun's component along
/// body axis j is `reading(+j) − reading(−j)` (at most one of the pair is lit, the other is
/// FOV-clamped and its noise floor cancels in expectation). `None` when no head clears
/// [`CSS_THRESHOLD`] — the sun is lost (eclipse), which is the FSW's own call from its
/// sensor, not modeled shadow. This is the reconstruction cube-sat's FSW skipped
/// (`sun_vec: sun_pos_b // TODO: do this more legit` — this is the legit version).
pub fn sun_from_css(css: &[f64; 6]) -> Option<V3> {
    if css.iter().all(|r| *r < CSS_THRESHOLD) {
        return None;
    }
    let v: V3 = tensor![css[0] - css[3], css[1] - css[4], css[2] - css[5]];
    Some(v.normalize())
}

/// The filter's state: the MEKF itself plus the reference models it evaluates each cycle.
pub struct NavState {
    state: metor_adcs::mekf::State,
    sigma: f64,
    /// The NOAA WMM handle for the reference magnetic field (built once — holds C-library state).
    mag_model: MagneticModel,
    /// Seconds of simulated FSW time — a deterministic per-cycle counter kept in lockstep
    /// with the plant's (both start at 0 and advance by `DT` each cycle), so nav evaluates its
    /// sun/field references at the same epoch the plant generated the measurement at.
    t_sim: f64,
    /// Whether the CSS saw the sun last cycle (`None` before the first sample), so the
    /// lost/reacquired transitions log once per edge instead of per cycle.
    sun_visible: Option<bool>,
    /// Whether a GPS fix has arrived yet, so the first one logs once.
    had_fix: bool,
}

impl NavState {
    pub fn new(p: NavParams) -> NavState {
        let state = metor_adcs::mekf::State::new(
            tensor![0.01, 0.01, 0.01],
            tensor![0.01, 0.01, 0.01],
            DT,
        );
        NavState {
            state,
            sigma: p.meas_sigma,
            mag_model: MagneticModel::default(),
            t_sim: 0.0,
            sun_visible: None,
            had_fix: false,
        }
    }

    /// One filter cycle: model the references, update the MEKF, publish the estimate.
    pub fn execute(
        state: &mut NavState,
        now: Timestamp,
        sensors: &mut Input<Sensors>,
        gps: &mut Input<Gps>,
        estimate: &mut Output<AttitudeEstimate>,
    ) {
        // Advance the deterministic FSW clock in lockstep with the plant (every cycle,
        // regardless of whether a sample is ready), so the reference epoch matches the epoch the
        // plant stamped this cycle's measurement at.
        let epoch = epoch_at(state.t_sim);
        state.t_sim += DT;

        let Ok(Some(s)) = sensors.latest() else {
            return; // no sensor sample yet
        };
        let s = s.clone();
        let Ok(Some(g)) = gps.latest() else {
            return; // no GPS fix yet
        };
        let gps_pos: V3 = g.pos_eci;
        if !state.had_fix {
            state.had_fix = true;
            tracing::info!(t_sim = state.t_sim, "first GPS fix; filter references live");
        }

        // The inertial (ECI) references nav models itself from what the flight software knows:
        // the sun direction from the ephemeris at this epoch (a unit vector), and the WMM
        // magnetic field at the GPS position (normalized), NOT the plant's truth `world` frame.
        let sun_eci = sun_dir_eci(epoch);
        let mag_eci = mag_field_eci(&mut state.mag_model, epoch, &gps_pos).normalize();

        state.state.omega = s.gyro_b;
        // The magnetometer reads the physical field (Tesla); the MEKF's vector observations
        // stay unit vectors, so normalize the measurement like the reference. The sun
        // observation comes from the CSS reconstruction — when the heads are dark (eclipse)
        // the filter updates on the magnetometer alone.
        let sun_b = sun_from_css(&s.css);
        // Log the visibility edges only: losing the sun degrades the filter to
        // magnetometer-only (the about-B̂ attitude rides the gyro until it returns).
        match (state.sun_visible, sun_b.is_some()) {
            (Some(true), false) => {
                tracing::warn!(t_sim = state.t_sim, "sun lost; running magnetometer-only")
            }
            (Some(false), true) => tracing::info!(t_sim = state.t_sim, "sun reacquired"),
            _ => {}
        }
        state.sun_visible = Some(sun_b.is_some());
        state.state = match sun_b {
            Some(sun_b) => state.state.clone().estimate_attitude(
                [sun_b, s.mag_b.normalize()],
                [sun_eci, mag_eci],
                [state.sigma, state.sigma],
            ),
            None => state.state.clone().estimate_attitude(
                [s.mag_b.normalize()],
                [mag_eci],
                [state.sigma],
            ),
        };
        state.state.reset_if_invalid();

        estimate.publish(&AttitudeEstimate {
            timestamp: now,
            q_hat_b_eci: state.state.q_hat,
            omega_b: s.gyro_b, // pass the measured body rate through to the controller
            b_hat_b: state.state.b_hat,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noiseless readings for a body-frame sun direction, the plant's head model inverted
    /// by hand (kept local — nav must not depend on the plant).
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

    /// Opposed-pair differencing recovers the sun direction exactly from noiseless
    /// readings, for directions exercising every head.
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

    /// All-dark heads (eclipse: noise floor only) declare the sun lost rather than
    /// normalizing noise into a fake direction.
    #[test]
    fn css_all_dark_is_sun_lost() {
        assert!(sun_from_css(&[0.0; 6]).is_none());
        // Noise-floor readings just under the threshold on every head.
        assert!(sun_from_css(&[0.05, 0.02, -0.03, 0.04, 0.01, 0.09]).is_none());
        // One head barely clearing the threshold is a (dim) detection.
        assert!(sun_from_css(&[0.11, 0.0, 0.0, 0.0, 0.0, 0.0]).is_some());
    }
}
