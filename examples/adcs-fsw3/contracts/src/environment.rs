//! Earth, sun, and magnetic field models, evaluated at a simulated epoch.

use hifitime::Duration;
use nox_frames::earth::{ecef_to_eci, eci_to_ecef, ned_to_ecef};
use wmm::GeodeticCoords;

use crate::{Epoch, MagneticModel, V3};

/// Gravitational constant (m³·kg⁻¹·s⁻²).
pub const G: f64 = 6.6743e-11;
/// Mass of Earth (kg).
pub const M: f64 = 5.972e24;
/// Earth's gravitational parameter `G·M` (m³·s⁻²).
pub const MU: f64 = G * M;
/// Earth radius (m).
pub const EARTH_RADIUS: f64 = 6378.1e3;

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
}
