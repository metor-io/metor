//! Earth, sun, and magnetic field models, evaluated at a simulated epoch.

use hifitime::Duration;
use nox_frames::earth::{ecef_to_eci, eci_to_ecef, ned_to_ecef};
use wmm::GeodeticCoords;

use crate::{EARTH_RADIUS, Epoch, MagneticModel, V3};

/// WGS84 semi-major axis (m).
const WGS84_A: f64 = 6378137.0;
/// WGS84 first eccentricity squared, `2f - f²` with `f = 1/298.257223563`.
const WGS84_E2: f64 = 6.694379990141316e-3;

/// Converts an ECEF position to WGS84 `(latitude_rad, longitude_rad, altitude_m)`.
pub fn ecef_to_geodetic(ecef: &V3) -> (f64, f64, f64) {
    let [x, y, z] = ecef.into_buf();
    let lon = y.atan2(x);
    let p = (x * x + y * y).sqrt();
    // Start from the spherical latitude. Five iterations converge well below a
    // meter at orbital altitude.
    let mut lat = z.atan2(p * (1.0 - WGS84_E2));
    let mut n = WGS84_A;
    for _ in 0..5 {
        n = WGS84_A / (1.0 - WGS84_E2 * lat.sin().powi(2)).sqrt();
        let alt = p / lat.cos() - n;
        lat = z.atan2(p * (1.0 - WGS84_E2 * n / (n + alt)));
    }
    let alt = p / lat.cos() - n;
    (lat, lon, alt)
}

/// Converts an ECI position at `epoch` to WGS84 `(latitude_rad, longitude_rad, altitude_m)`.
pub fn eci_to_geodetic(epoch: Epoch, pos_eci: &V3) -> (f64, f64, f64) {
    ecef_to_geodetic(&eci_to_ecef(epoch).dot(pos_eci))
}

/// Returns the unnormalized WMM magnetic field in ECI (T) at `pos_eci` and `epoch`.
pub fn mag_field_eci(model: &mut MagneticModel, epoch: Epoch, pos_eci: &V3) -> V3 {
    let (lat, lon, alt) = eci_to_geodetic(epoch, pos_eci);
    let geodetic = GeodeticCoords::with_elliposid_height(
        lat.to_degrees(),
        lon.to_degrees(),
        alt / 1000.0, // km
    );
    let (elements, _) = model.calculate_field(epoch, geodetic);
    // WMM reports the field in north-east-down axes.
    let b_ned = V3::from_buf(elements.b_field());
    let b_ecef = ned_to_ecef(lat, lon).dot(&b_ned);
    ecef_to_eci(epoch).dot(&b_ecef)
}

/// Earth's rotation rate about the ECI z axis (rad/s).
const EARTH_RATE: f64 = 7.292_115_146_7e-5;
/// Seconds in a GPS week.
const WEEK: f64 = 604_800.0;

/// Converts an ECI position and velocity at `epoch` to ECEF. The velocity is
/// relative to the rotating Earth.
pub fn eci_to_ecef_state(epoch: Epoch, pos_eci: &V3, vel_eci: &V3) -> (V3, V3) {
    let r = eci_to_ecef(epoch);
    let carried = earth_rate().cross(pos_eci);
    (r.dot(pos_eci), r.dot(&(*vel_eci - carried)))
}

/// Converts an ECEF position and velocity at `epoch` to ECI, the inverse of
/// [`eci_to_ecef_state`].
pub fn ecef_to_eci_state(epoch: Epoch, pos_ecef: &V3, vel_ecef: &V3) -> (V3, V3) {
    let r = ecef_to_eci(epoch);
    let pos = r.dot(pos_ecef);
    (pos, r.dot(vel_ecef) + earth_rate().cross(&pos))
}

fn earth_rate() -> V3 {
    V3::from_buf([0.0, 0.0, EARTH_RATE])
}

/// Returns `epoch` as a GPS week and seconds into it, to the millisecond.
pub fn gps_time(epoch: Epoch) -> (u32, f64) {
    let ms = (epoch.to_gpst_seconds() * 1e3).round();
    let week = (ms / (WEEK * 1e3)).floor();
    (week as u32, (ms - week * WEEK * 1e3) / 1e3)
}

/// Returns the epoch of a GPS week and seconds into it.
pub fn gps_epoch(week: u32, seconds: f64) -> Epoch {
    Epoch::from_gpst_seconds(f64::from(week) * WEEK + seconds)
}

/// Returns the UTC epoch at simulated time zero, fixed so simulated runs repeat exactly.
pub fn target_epoch() -> Epoch {
    Epoch::from_gregorian_utc(2024, 1, 1, 0, 0, 0, 0)
}

/// Returns the epoch `t_sim_s` seconds after [`target_epoch`].
pub fn epoch_at(t_sim_s: f64) -> Epoch {
    target_epoch() + Duration::from_seconds(t_sim_s)
}

/// Returns the unit vector toward the sun in ECI at `epoch`.
pub fn sun_dir_eci(epoch: Epoch) -> V3 {
    nox_frames::earth::sun_vec(epoch)
}

/// Returns the RAAN (rad) that puts the ascending node at local mean solar time
/// `ltan_hours` at `epoch`.
pub fn raan_for_ltan(epoch: Epoch, ltan_hours: f64) -> f64 {
    let [x, y, _] = sun_dir_eci(epoch).into_buf();
    y.atan2(x) + (ltan_hours - 12.0) / 24.0 * core::f64::consts::TAU
}

/// Returns whether `pos_eci` lies in Earth's shadow, modeled as a cylinder of
/// radius [`EARTH_RADIUS`] behind a spherical Earth.
///
/// At 400 km the cylinder moves eclipse entry by under a second against the
/// true umbra cone, and the penumbra lasts about 8 s of a 35 minute eclipse.
pub fn in_earth_shadow(pos_eci: &V3, sun_eci: &V3) -> bool {
    let s = sun_eci.normalize();
    let along: f64 = pos_eci.dot(&s).into_buf();
    if along >= 0.0 {
        return false; // sunward side
    }
    let perp = *pos_eci - s * along;
    let perp: f64 = perp.norm().into_buf();
    perp < EARTH_RADIUS
}

#[cfg(test)]
mod tests {
    use nox::tensor;

    use super::*;
    use crate::ALTITUDE;

    /// The field at 400 km falls in the tens of µT, which catches unit and
    /// rotation mistakes in the WMM chain.
    #[test]
    fn wmm_field_is_sane_at_orbit() {
        let mut model = MagneticModel::default();
        let radius = EARTH_RADIUS + ALTITUDE;
        // Several latitudes.
        for pos in [
            tensor![radius, 0.0, 0.0],
            tensor![0.0, radius, 0.0],
            (tensor![1.0, 1.0, 1.0] as V3).normalize() * radius,
        ] {
            let b = mag_field_eci(&mut model, target_epoch(), &pos);
            let mag = b.norm().into_buf();
            assert!(
                (1.0e-5..6.0e-5).contains(&mag),
                "WMM field magnitude at 400 km should be tens of µT, got {mag} T"
            );
        }
    }

    /// Points on the sun line, on the terminator, and on either side of the
    /// shadow cylinder's edge classify correctly.
    #[test]
    fn earth_shadow_geometry() {
        let sun: V3 = (tensor![0.2, -0.9, -0.4] as V3).normalize();
        let r = EARTH_RADIUS + ALTITUDE;
        assert!(!in_earth_shadow(&(sun * r), &sun), "sub-solar point is lit");
        assert!(
            in_earth_shadow(&(sun * -r), &sun),
            "anti-solar point is shadowed"
        );
        let perp = sun.cross(&tensor![0.0, 0.0, 1.0]).normalize();
        assert!(
            !in_earth_shadow(&(perp * r), &sun),
            "terminator point is lit"
        );
        let graze = (sun * -r) + perp * (EARTH_RADIUS * 1.01);
        assert!(
            !in_earth_shadow(&graze, &sun),
            "outside the shadow cylinder is lit"
        );
        let inside = (sun * -r) + perp * (EARTH_RADIUS * 0.99);
        assert!(
            in_earth_shadow(&inside, &sun),
            "inside the shadow cylinder is dark"
        );
    }

    /// On the equator, geodetic altitude equals `|r| - a`.
    #[test]
    fn geodetic_recovers_equatorial_altitude() {
        let radius = EARTH_RADIUS + ALTITUDE;
        let (lat, _lon, alt) = ecef_to_geodetic(&tensor![radius, 0.0, 0.0]);
        assert!(
            lat.abs() < 1e-9,
            "equatorial point has ~zero latitude, got {lat}"
        );
        assert!(
            (alt - (radius - WGS84_A)).abs() < 1.0,
            "recovered altitude within 1 m of |r| - a: {alt}"
        );
    }

    #[test]
    fn state_round_trips_through_ecef() {
        let epoch = epoch_at(1234.5);
        let (pos, vel) = (
            V3::from_buf([6.8e6, 1.0e5, -2.0e5]),
            V3::from_buf([10.0, 7600.0, 5.0]),
        );
        let (pos_e, vel_e) = eci_to_ecef_state(epoch, &pos, &vel);
        let (pos_i, vel_i) = ecef_to_eci_state(epoch, &pos_e, &vel_e);
        assert!(norm(pos_i - pos) < 1e-6);
        assert!(norm(vel_i - vel) < 1e-9);
        assert!(norm(vel_e - vel) > 400.0, "the Earth's rotation is carried");
    }

    #[test]
    fn gps_time_round_trips_to_the_millisecond() {
        let epoch = epoch_at(98.765_4);
        let (week, seconds) = gps_time(epoch);
        assert_eq!(week, 2295);
        let back = gps_epoch(week, seconds);
        assert!((back - epoch).to_seconds().abs() <= 5e-4);
        assert_eq!(gps_time(back), (week, seconds));
    }

    fn norm(v: V3) -> f64 {
        v.norm().into_buf()
    }
}
