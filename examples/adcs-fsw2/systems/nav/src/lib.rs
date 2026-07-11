//! The MEKF **navigation filter** of the `adcs-fsw2` mission, as a `dlopen`-loadable
//! `cdylib` (dl-open.md §3, §8). It wraps [`metor_fsw_adcs::mekf::State`] (a multiplicative
//! EKF) and estimates attitude from the two body-frame vector observations + gyro, modeling
//! the inertial **references** for those observations itself (cube-sat's `Nav::from_sensors`):
//! the sun direction from the Vallado ephemeris at its own sim-time epoch, and the magnetic
//! field from the NOAA WMM evaluated at the **GPS** position. It knows only what the flight
//! software knows — the noisy GPS measurement — not the plant's truth `world` frame.
//!
//! Authored with `#[system]` (`docs/design-system-macro.md`): the port set is the `execute`
//! signature, and the bundles/trait impls/`BuildSystem`/`fsw_*` exports are all generated.

use adcs_contracts::{
    AttitudeEstimate, DT, Gps, MagneticModel, NavParams, Sensors, V3, epoch_at, mag_field_eci,
    sun_dir_eci,
};
use metor_fsw_2::{Input, Output, Timestamp, system};
use nox::tensor;

pub struct NavSystem {
    state: metor_fsw_adcs::mekf::State,
    sigma: f64,
    /// The NOAA WMM handle for the reference magnetic field (built once — holds C-library state).
    mag_model: MagneticModel,
    /// Seconds of simulated mission time — a deterministic per-cycle counter kept in lockstep
    /// with the plant's (both start at 0 and advance by `DT` each cycle), so nav evaluates its
    /// sun/field references at the same epoch the plant generated the measurement at.
    t_sim: f64,
}

#[system(name = "nav", export = "export")]
impl NavSystem {
    pub fn new(p: NavParams) -> Self {
        let state = metor_fsw_adcs::mekf::State::new(
            tensor![0.01, 0.01, 0.01],
            tensor![0.01, 0.01, 0.01],
            DT,
        );
        Self {
            state,
            sigma: p.meas_sigma,
            mag_model: MagneticModel::default(),
            t_sim: 0.0,
        }
    }

    fn execute(
        &mut self,
        now: Timestamp,
        sensors: &mut Input<Sensors>,
        gps: &mut Input<Gps>,
        estimate: &mut Output<AttitudeEstimate>,
    ) {
        // Advance the deterministic mission clock in lockstep with the plant (every cycle,
        // regardless of whether a sample is ready), so the reference epoch matches the epoch the
        // plant stamped this cycle's measurement at.
        let epoch = epoch_at(self.t_sim);
        self.t_sim += DT;

        let Some(s) = sensors.latest() else {
            return; // no sensor sample yet
        };
        let s = s.get().clone();
        let Some(g) = gps.latest() else {
            return; // no GPS fix yet
        };
        let gps_pos: V3 = g.get().pos_eci;

        // The inertial (ECI) references nav models itself from what the flight software knows:
        // the sun direction from the ephemeris at this epoch (a unit vector), and the WMM
        // magnetic field at the GPS position (normalized), NOT the plant's truth `world` frame.
        let sun_eci = sun_dir_eci(epoch);
        let mag_eci = mag_field_eci(&mut self.mag_model, epoch, &gps_pos).normalize();

        self.state.omega = s.gyro_b;
        // The magnetometer reads the physical field (Tesla); the MEKF's vector observation
        // stays a unit vector, so normalize the measurement like the reference.
        self.state = self.state.clone().estimate_attitude(
            [s.sun_b, s.mag_b.normalize()],
            [sun_eci, mag_eci],
            [self.sigma, self.sigma],
        );
        self.state.reset_if_invalid();

        estimate.publish(&AttitudeEstimate {
            timestamp: now,
            q_hat_b_eci: self.state.q_hat,
            omega_b: s.gyro_b, // pass the measured body rate through to the controller
            b_hat_b: self.state.b_hat,
        });
    }
}
